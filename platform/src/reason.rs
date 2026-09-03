// SPDX-License-Identifier: Apache-2.0
//
// The typed platform-reason vocabulary: why a detected platform is not a
// supported combination.
//
// This crate owns the enum. `doctor`, deploy admission, and status are
// intended to emit these values rather than prose, so the same condition
// reads the same way everywhere; the consumers are wired up separately.
// Trigger conditions and user-facing rendering are frozen by the doctor
// work; the values themselves are frozen here.

use serde::{Deserialize, Serialize};
use tensorplate_protocol::backend_probe::BackendProbeState;

/// Why a detected platform is not a supported combination.
///
/// `try_from` pins decoding to the plain string form: serde's derived
/// `Deserialize` for a fieldless enum would also accept the
/// externally-tagged map form (`{"unsupported_os_version": null}`), which
/// a schema pinning `type: "string"` rejects.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "String")]
pub enum PlatformReason {
    /// The accelerator SKU matches neither an exact row nor an explicit
    /// family row. Never a nearest match: a near miss is unsupported, not
    /// degraded.
    UnsupportedAcceleratorSku,
    /// The OS version is below the row's floor or is not the exact version
    /// a row names.
    UnsupportedOsVersion,
    /// The CPU architecture is not one this release builds for.
    UnsupportedCpuArch,
    /// The CPU architecture is supported but the vendor is not, on a row
    /// where vendor is load-bearing.
    UnsupportedCpuVendor,
    /// The accelerator is partitioned. Partitioned devices are rejected
    /// before model load rather than served at reduced capacity.
    MigModeEnabled,
    /// A required backend package is absent from the installed package
    /// set.
    MissingBackendPackage,
    /// A required driver or compute runtime is absent or version-mismatched
    /// against the row.
    MissingDriverRuntime,
    /// The accelerator runtime is installed but unavailable at run time —
    /// distinct from a package being missing.
    AcceleratorRuntimeUnavailable,
    /// A telemetry collector the row expects failed, so signals the row
    /// depends on are unavailable.
    TelemetryDegraded,
    /// The detected identity exactly matches a Planned row: the platform
    /// is known and defined, but carries no validation evidence yet.
    RowPlannedNotValidated,
    /// The host reports a number of accelerators no row claims. Every row
    /// this release commits to is single-device, so a host with two or
    /// more is refused before any SKU is compared -- a supported SKU
    /// installed twice is not a supported machine, because no row's
    /// evidence was collected on that topology.
    ///
    /// Distinct from [`Self::UnsupportedAcceleratorSku`], which says the
    /// silicon is wrong. Here the silicon may be exactly right and there
    /// is simply more of it than anything has been validated against.
    UnsupportedAcceleratorTopology,
}

impl PlatformReason {
    /// Every reason, in declaration order. Downstream conformance tests
    /// iterate this to prove each reason has a trigger and a rendering.
    pub const ALL: [Self; 11] = [
        Self::UnsupportedAcceleratorSku,
        Self::UnsupportedOsVersion,
        Self::UnsupportedCpuArch,
        Self::UnsupportedCpuVendor,
        Self::MigModeEnabled,
        Self::MissingBackendPackage,
        Self::MissingDriverRuntime,
        Self::AcceleratorRuntimeUnavailable,
        Self::TelemetryDegraded,
        Self::RowPlannedNotValidated,
        Self::UnsupportedAcceleratorTopology,
    ];

    /// The reason a backend probe state carries, or `None` when the
    /// backend is runnable.
    ///
    /// The distinction this exists to keep: a descriptor that is absent
    /// means the package is not installed, and every other failure means
    /// the package IS installed and its runtime is not usable. Collapsing
    /// them sends an operator whose PyTorch cannot see its accelerator to
    /// reinstall a package they already have.
    #[must_use]
    pub fn for_backend_probe(state: &BackendProbeState) -> Option<Self> {
        match state {
            BackendProbeState::Runnable => None,
            BackendProbeState::DescriptorMissing => Some(Self::MissingBackendPackage),
            // Present but unusable: a malformed descriptor, an absent or
            // wrong-version interpreter, a module or framework that will
            // not import, or a runtime the backend refuses to run under.
            BackendProbeState::DescriptorMalformed { .. }
            | BackendProbeState::RuntimeVersionMismatch { .. }
            | BackendProbeState::PythonInterpreterMissing { .. }
            | BackendProbeState::PythonVersionMismatch { .. }
            | BackendProbeState::PythonModuleImportFailed { .. }
            | BackendProbeState::PytorchMissing { .. }
            | BackendProbeState::PytorchVersionMismatch { .. } => {
                Some(Self::AcceleratorRuntimeUnavailable)
            }
        }
    }

    /// Stable serialized name (snake_case). The exhaustive match makes a
    /// newly added reason a compile error here rather than a silently
    /// unspelled one.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedAcceleratorSku => "unsupported_accelerator_sku",
            Self::UnsupportedOsVersion => "unsupported_os_version",
            Self::UnsupportedCpuArch => "unsupported_cpu_arch",
            Self::UnsupportedCpuVendor => "unsupported_cpu_vendor",
            Self::MigModeEnabled => "mig_mode_enabled",
            Self::MissingBackendPackage => "missing_backend_package",
            Self::MissingDriverRuntime => "missing_driver_runtime",
            Self::AcceleratorRuntimeUnavailable => "accelerator_runtime_unavailable",
            Self::TelemetryDegraded => "telemetry_degraded",
            Self::RowPlannedNotValidated => "row_planned_not_validated",
            Self::UnsupportedAcceleratorTopology => "unsupported_accelerator_topology",
        }
    }
}

impl std::fmt::Display for PlatformReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<String> for PlatformReason {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::ALL
            .into_iter()
            .find(|reason| reason.as_str() == value)
            .ok_or_else(|| format!("unknown platform reason `{value}`"))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::PlatformReason;

    #[test]
    fn the_vocabulary_is_eleven_distinct_values() {
        let mut spellings: Vec<&str> = PlatformReason::ALL.iter().map(|r| r.as_str()).collect();
        assert_eq!(spellings.len(), 11);
        spellings.sort_unstable();
        spellings.dedup();
        assert_eq!(spellings.len(), 11, "reason spellings must be distinct");
    }

    #[test]
    fn a_backend_probe_never_conflates_a_missing_package_with_a_dead_runtime() {
        use tensorplate_protocol::backend_probe::BackendProbeState as S;
        // Absent descriptor: the package is not installed.
        assert_eq!(
            PlatformReason::for_backend_probe(&S::DescriptorMissing),
            Some(PlatformReason::MissingBackendPackage)
        );
        // Everything else: the package IS installed and its runtime is
        // not usable. Enumerated rather than wildcarded so a new probe
        // state has to be classified deliberately.
        for state in [
            S::DescriptorMalformed {
                reason: String::new(),
            },
            S::RuntimeVersionMismatch {
                runtime_version: String::new(),
                descriptor_min: String::new(),
            },
            S::PythonInterpreterMissing {
                interpreter: String::new(),
            },
            S::PythonVersionMismatch {
                interpreter: String::new(),
                observed: String::new(),
                required: String::new(),
            },
            S::PythonModuleImportFailed {
                module: String::new(),
                detail: String::new(),
            },
            S::PytorchMissing {
                detail: String::new(),
            },
            S::PytorchVersionMismatch {
                observed: String::new(),
                required: String::new(),
            },
        ] {
            assert_eq!(
                PlatformReason::for_backend_probe(&state),
                Some(PlatformReason::AcceleratorRuntimeUnavailable),
                "{state:?} is an installed runtime that will not run"
            );
        }
        assert_eq!(PlatformReason::for_backend_probe(&S::Runnable), None);
    }

    #[test]
    fn the_cpu_dimensions_and_the_sku_stay_distinct() {
        // Three reasons that a renderer could plausibly collapse. The
        // vocabulary is only useful if each names one dimension: an
        // operator on the wrong architecture and one on an unbuilt vendor
        // need different answers, and the SKU reason is vendor-neutral so
        // it can carry an Apple chip and an NVIDIA card alike.
        let spellings = [
            PlatformReason::UnsupportedCpuArch.as_str(),
            PlatformReason::UnsupportedCpuVendor.as_str(),
            PlatformReason::UnsupportedAcceleratorSku.as_str(),
        ];
        assert_eq!(
            spellings
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            3,
            "these three must never share a spelling"
        );
        for vendor in ["nvidia", "apple", "amd", "intel"] {
            assert!(
                !PlatformReason::UnsupportedAcceleratorSku
                    .as_str()
                    .contains(vendor),
                "the SKU reason must stay vendor-neutral"
            );
        }
    }

    #[test]
    fn spellings_round_trip() {
        for reason in PlatformReason::ALL {
            let json = serde_json::to_string(&reason).expect("serialize");
            assert_eq!(json, format!("\"{}\"", reason.as_str()));
            let back: PlatformReason = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, reason);
            assert_eq!(reason.to_string(), reason.as_str());
        }
    }

    #[test]
    fn non_string_forms_reject() {
        serde_json::from_str::<PlatformReason>(r#"{"telemetry_degraded":null}"#)
            .expect_err("map form must reject");
        serde_json::from_str::<PlatformReason>("0").expect_err("variant index must reject");
        serde_json::from_str::<PlatformReason>(r#""not_a_reason""#)
            .expect_err("unknown spelling must reject");
    }
}
