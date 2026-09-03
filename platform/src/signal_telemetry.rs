// SPDX-License-Identifier: Apache-2.0
//
// Gate-semantic handling for the platform signals a row declares.
//
// A row says, per signal, whether it gates behaviour, is reported for
// context, or is absent here. This turns that declaration plus what the
// collectors actually managed to read into one answer: whether the
// machine is degraded, and why.
//
// The three postures are genuinely different and collapsing any two of
// them is a real fault. A thermal sensor that fails on a Jetson is a
// machine that cannot be trusted to throttle itself; the same failure on
// a datacenter row is a missing number on a chassis whose cooling is
// somebody else's problem; and a power reading that macOS does not expose
// without privileges was never going to be there, so calling it a failure
// would report every Mac as broken.

use std::collections::BTreeMap;

use crate::reason::PlatformReason;
use crate::row::{GateValue, PlatformSupportRow};

/// The signals a row declares a posture for.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SignalName {
    Thermal,
    Power,
    Throttle,
    Memory,
    GpuUtilization,
}

impl SignalName {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Thermal => "thermal",
            Self::Power => "power",
            Self::Throttle => "throttle",
            Self::Memory => "memory",
            Self::GpuUtilization => "gpu_utilization",
        }
    }

    #[must_use]
    pub const fn all() -> [Self; 5] {
        [
            Self::Thermal,
            Self::Power,
            Self::Throttle,
            Self::Memory,
            Self::GpuUtilization,
        ]
    }
}

/// What a collector managed for one signal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SignalOutcome {
    /// The source answered.
    Collected,
    /// The source is expected on this row and failed at run time.
    Unavailable { detail: String },
}

/// One signal's posture and what came back for it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignalStatus {
    pub gate: GateValue,
    /// The row's own explanation for an absent signal. Free text and a
    /// row fact deliberately: the typed platform reasons say why a
    /// PLATFORM is unsupported, and a sensor macOS does not expose is not
    /// a support claim about the machine.
    pub not_applicable_reason: Option<String>,
    /// `None` where the row declares the signal absent -- nothing was
    /// asked for, so nothing succeeded or failed.
    pub outcome: Option<SignalOutcome>,
}

/// Every signal on one row, resolved against what the collectors read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignalTelemetry {
    row_id: String,
    signals: BTreeMap<SignalName, SignalStatus>,
}

impl SignalTelemetry {
    /// Resolve a row's declared postures against collector outcomes.
    ///
    /// `outcomes` names the signals whose source answered. A signal the
    /// row declares `not_applicable` is never asked for, so its presence
    /// or absence in `outcomes` is ignored rather than treated as a
    /// result.
    ///
    /// An omitted applicable signal is resolved to `Unavailable`. A
    /// partial collector map is itself a failed collection result; leaving
    /// the outcome as `None` would make the same omission indistinguishable
    /// from `not_applicable` and fail open on load-bearing signals.
    #[must_use]
    pub fn resolve(
        row: &PlatformSupportRow,
        outcomes: &BTreeMap<SignalName, SignalOutcome>,
    ) -> Self {
        let gates = row.gate_semantics();
        let mut signals = BTreeMap::new();
        for name in SignalName::all() {
            let gate = match name {
                SignalName::Thermal => &gates.thermal,
                SignalName::Power => &gates.power,
                SignalName::Throttle => &gates.throttle,
                SignalName::Memory => &gates.memory,
                SignalName::GpuUtilization => &gates.gpu_utilization,
            };
            let declared_absent = gate.gate == GateValue::NotApplicable;
            signals.insert(
                name,
                SignalStatus {
                    gate: gate.gate,
                    not_applicable_reason: declared_absent.then(|| gate.reason.clone()).flatten(),
                    outcome: (!declared_absent).then(|| {
                        outcomes
                            .get(&name)
                            .cloned()
                            .unwrap_or_else(|| SignalOutcome::Unavailable {
                                detail: "collector produced no outcome".to_string(),
                            })
                    }),
                },
            );
        }
        Self {
            row_id: row.row_id().to_string(),
            signals,
        }
    }

    #[must_use]
    pub fn row_id(&self) -> &str {
        &self.row_id
    }

    #[must_use]
    pub fn signal(&self, name: SignalName) -> Option<&SignalStatus> {
        self.signals.get(&name)
    }

    /// Every signal in stable name order, for status and evidence
    /// projections. All five entries are present; only a row-declared
    /// `not_applicable` entry has no outcome.
    pub fn signals(&self) -> impl Iterator<Item = (SignalName, &SignalStatus)> {
        self.signals.iter().map(|(name, status)| (*name, status))
    }

    /// Signals whose source was expected here and failed, whatever their
    /// posture. Both postures are reported; only one of them degrades.
    #[must_use]
    pub fn failed_signals(&self) -> Vec<SignalName> {
        self.signals
            .iter()
            .filter(|(_, status)| matches!(status.outcome, Some(SignalOutcome::Unavailable { .. })))
            .map(|(name, _)| *name)
            .collect()
    }

    /// The typed reason a failed collector carries, or `None` when every
    /// expected source answered.
    ///
    /// This is the only producer of [`PlatformReason::TelemetryDegraded`].
    #[must_use]
    pub fn degraded_reason(&self) -> Option<PlatformReason> {
        (!self.failed_signals().is_empty()).then_some(PlatformReason::TelemetryDegraded)
    }

    /// Whether a collector failure here should affect deployment.
    ///
    /// Only a `load_bearing` source does. A `context_only` source that
    /// fails is recorded and does not block: the row already said that
    /// signal informs rather than decides, and refusing a deploy over it
    /// would make a context signal load-bearing by the back door.
    #[must_use]
    pub fn degrades_deployment(&self) -> bool {
        !self.deployment_degrading_signals().is_empty()
    }

    /// Failed signals whose row posture makes them deployment gates.
    /// Context-only failures remain in [`Self::failed_signals`] and status,
    /// but must not be presented as reasons a deployment was refused.
    #[must_use]
    pub fn deployment_degrading_signals(&self) -> Vec<SignalName> {
        self.signals
            .iter()
            .filter(|(_, status)| {
                status.gate == GateValue::LoadBearing
                    && matches!(status.outcome, Some(SignalOutcome::Unavailable { .. }))
            })
            .map(|(name, _)| *name)
            .collect()
    }

    /// Signals the row declares absent, with the row's own explanation.
    #[must_use]
    pub fn not_applicable(&self) -> Vec<(SignalName, Option<&str>)> {
        self.signals
            .iter()
            .filter(|(_, status)| status.gate == GateValue::NotApplicable)
            .map(|(name, status)| (*name, status.not_applicable_reason.as_deref()))
            .collect()
    }
}
