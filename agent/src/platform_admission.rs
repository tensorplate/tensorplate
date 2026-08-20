// SPDX-License-Identifier: Apache-2.0
//
// Platform-row admission state owned by the agent.
//
// Detection and matching stay in tensorplate-platform. The agent keeps only
// the resolved outcome: either a supported row with its bounded capability,
// or the typed reason deployment must fail before model load.
//
// Two questions, settled at different times because they have different
// inputs. Whether this machine matches a row at all is a fact about the
// machine, so it is settled once at startup. Whether the packages a row
// requires for a given backend path are installed depends on which backend
// the bundle names, so it is asked per deploy.
//
// **A detection failure is a rejection, not a bypass.** An agent that
// cannot read its own hardware must not deploy as though the check passed;
// `detection_failed` records the failure as a rejection so startup stays
// diagnosable while deployment stays closed. Returning "no admission" there
// would leave the machine ungated, which inverts the gate on exactly the
// hardware nobody has characterised.

use std::collections::{BTreeMap, BTreeSet};

use tensorplate_platform::{
    AdmissionPosture, HostReport, PlatformCapability, PlatformReason, PlatformRegistry,
    PlatformReport, PlatformSupportRow, RowMatch, SupportLevel,
};

use crate::config::AgentConfig;
use crate::error::{AgentError, AgentResult};

/// What was observed about this machine's driver and runtime stack, and
/// which packages are installed.
///
/// Supplied rather than probed, so evaluation is testable without the
/// hardware or the packages it describes.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ObservedStack {
    /// Component identifier to version, in the row's vocabulary
    /// (`nvidia_driver`, `cuda`, `tensorrt`, …).
    pub components: BTreeMap<String, String>,
    /// Package names installed on this host.
    pub installed_packages: BTreeSet<String>,
}

/// Cached platform outcome evaluated once at agent startup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlatformAdmission {
    Supported {
        row_id: String,
        capability: Option<PlatformCapability>,
        /// Carried so a per-deploy package check is a comparison rather
        /// than a re-probe.
        installed_packages: BTreeSet<String>,
        /// The posture in force for this machine, and where it came from.
        ///
        /// Recorded rather than consulted: nothing reads it to decide
        /// anything yet. It is reported so an operator can SEE the
        /// strictness they are running at and pin it explicitly — which is
        /// what makes a future change to the default a decision they can
        /// opt out of rather than a silent behaviour change on upgrade.
        posture: AdmissionPosture,
        posture_from: &'static str,
        /// Whether this admission rests on a row whose evidence covers
        /// this machine.
        ///
        /// False where the machine was admitted on technical
        /// prerequisites instead: the hardware matches a row, the chassis
        /// is one nobody characterised, and the row's own gate semantics
        /// say its chassis signals are context rather than gates. The
        /// machine runs; what it must not do is claim a validation that
        /// was recorded somewhere else.
        validated: bool,
    },
    Rejected {
        row_id: Option<String>,
        /// `None` where the frozen vocabulary has no value for this
        /// outcome. Two cases reach that: an Experimental row, and a
        /// machine whose shape no row's evidence covers. Borrowing the
        /// nearest reason would name a dimension that is actually fine —
        /// `registry.rs` draws the first distinction explicitly and this
        /// module must not undo it.
        reason: Option<PlatformReason>,
        detail: String,
    },
}

impl PlatformAdmission {
    /// Evaluate one observation against the installed registry.
    ///
    /// Driver and runtime requirements come from the matched row, never
    /// from a constant here. A row whose evidence run has not happened yet
    /// declares no components, and this check is silent for it rather than
    /// inventing a requirement — an empty list means "not yet recorded",
    /// not "nothing required".
    #[must_use]
    pub fn evaluate(
        registry: &PlatformRegistry,
        report: &PlatformReport,
        observed: &ObservedStack,
        operator_posture: Option<AdmissionPosture>,
    ) -> Self {
        // A GPU host whose driver is missing or broken reports no
        // accelerator, and a host with no accelerator resolves to a
        // CPU-only row and deploys as a CPU box. The PCI bus says which of
        // the two this is, and it answers without a driver.
        //
        // Checked before matching, and on every posture: this is not a
        // question of how strictly the machine is judged. Serving CPU
        // work on a machine bought for its accelerator is a silent
        // downgrade under any posture, and the operator is the one who
        // should decide to accept it.
        if report.accelerator.is_none() && !report.host.exact.nvidia_pci_functions.is_empty() {
            let functions = report.host.exact.nvidia_pci_functions.join(", ");
            return Self::Rejected {
                row_id: None,
                reason: Some(PlatformReason::MissingDriverRuntime),
                detail: format!(
                    "the PCI bus reports an NVIDIA display controller at {functions} but no                      accelerator could be identified; the driver is absent or not functional.                      Deploying here would serve on the CPU without saying so"
                ),
            };
        }
        let detected = report.detected_platform();
        match registry.resolve(&detected) {
            RowMatch::Supported(row) => {
                if let Some(detail) = first_stack_mismatch(row, observed) {
                    return Self::Rejected {
                        row_id: Some(row.row_id().to_string()),
                        reason: Some(PlatformReason::MissingDriverRuntime),
                        detail,
                    };
                }
                let floor = AdmissionPosture::floor_for(row);
                Self::Supported {
                    row_id: row.row_id().to_string(),
                    capability: registry.resolved_capability(report),
                    installed_packages: observed.installed_packages.clone(),
                    posture: AdmissionPosture::resolve(floor, operator_posture),
                    posture_from: AdmissionPosture::provenance(floor, operator_posture),
                    validated: true,
                }
            }
            RowMatch::PlannedNotValidated(row) => Self::planned(row),
            RowMatch::Experimental(row) => Self::experimental(row),
            RowMatch::OutsideValidatedEnvironment {
                candidate: Some(row),
            } => {
                // Support level first. Reaching this path means the
                // machine is FURTHER from the row than an exact match is,
                // and an exact match on a Planned or Experimental row is
                // refused -- so admitting here would invert the gate,
                // making such a row deployable only where its evidence
                // covers even less. Every committed Planned row happens to
                // carry a load-bearing chassis gate, so the posture check
                // below would catch them today; that is a property of the
                // current rows, not a guarantee, and it is not what makes
                // this correct.
                if !row.is_supported_combination() {
                    return match row.support_level() {
                        SupportLevel::Planned => Self::planned(row),
                        _ => Self::experimental(row),
                    };
                }
                let floor = AdmissionPosture::floor_for(row);
                let posture = AdmissionPosture::resolve(floor, operator_posture);
                if posture == AdmissionPosture::ValidatedRowRequired {
                    return Self::Rejected {
                        row_id: Some(row.row_id().to_string()),
                        reason: None,
                        detail: format!(
                            "platform is outside the validated environment for row `{}`",
                            row.row_id()
                        ),
                    };
                }
                // Presence, not equality. The versions a row records are
                // what was validated, not a minimum and not a ceiling —
                // refusing a machine for carrying a newer driver than the
                // evidence run happened to have would refuse most of the
                // fleet this path exists to admit.
                if let Some(detail) = first_absent_component(row, observed) {
                    return Self::Rejected {
                        row_id: Some(row.row_id().to_string()),
                        reason: Some(PlatformReason::MissingDriverRuntime),
                        detail,
                    };
                }
                Self::Supported {
                    row_id: row.row_id().to_string(),
                    capability: registry.capability_outside_environment(report, row),
                    installed_packages: observed.installed_packages.clone(),
                    posture,
                    posture_from: AdmissionPosture::provenance(floor, operator_posture),
                    validated: false,
                }
            }
            // No single row's hardware matches, so there is no row to read
            // a posture floor or a prerequisite list from. Naming one
            // would be picking arbitrarily, and guessing at prerequisites
            // is what this whole path exists to avoid.
            RowMatch::OutsideValidatedEnvironment { candidate: None } => Self::Rejected {
                row_id: None,
                reason: None,
                detail: "platform is outside every validated environment".to_string(),
            },
            RowMatch::Unsupported(reason) => Self::Rejected {
                row_id: None,
                reason: Some(reason),
                detail: format!("platform is unsupported: {reason}"),
            },
        }
    }

    fn planned(row: &PlatformSupportRow) -> Self {
        Self::Rejected {
            row_id: Some(row.row_id().to_string()),
            reason: Some(PlatformReason::RowPlannedNotValidated),
            detail: format!(
                "platform row `{}` is planned but not validated",
                row.row_id()
            ),
        }
    }

    fn experimental(row: &PlatformSupportRow) -> Self {
        Self::Rejected {
            row_id: Some(row.row_id().to_string()),
            reason: None,
            detail: format!(
                "platform row `{}` is experimental and not deployable",
                row.row_id()
            ),
        }
    }

    /// Classify a failure to probe the accelerator.
    ///
    /// The usual broken driver is an *installed* `nvidia-smi` exiting
    /// non-zero, which is a probe error rather than an absent accelerator
    /// -- so it never reaches [`Self::evaluate`], and the PCI evidence that
    /// would name it goes unread. Without this the operator is told
    /// detection failed, which is true and useless, when the machine could
    /// have been told its driver is broken.
    ///
    /// Rejects either way. What the PCI bus decides is whether the reason
    /// is typed: a card on the bus evidences a driver problem, and nothing
    /// on the bus evidences one, so claiming `missing_driver_runtime` there
    /// would send an operator looking for a driver on a machine with no
    /// card.
    #[must_use]
    pub fn accelerator_probe_failed(host: &HostReport, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        if host.exact.nvidia_pci_functions.is_empty() {
            return Self::detection_failed(detail);
        }
        Self::Rejected {
            row_id: None,
            reason: Some(PlatformReason::MissingDriverRuntime),
            detail: format!(
                "the PCI bus reports an NVIDIA display controller at {} but the accelerator \
                 probe failed: {detail}. The driver is present-but-broken or absent; deploying \
                 here would serve on the CPU without saying so",
                host.exact.nvidia_pci_functions.join(", ")
            ),
        }
    }

    /// Record a detection failure so startup stays diagnosable while
    /// deployment still fails closed.
    #[must_use]
    pub fn detection_failed(detail: impl Into<String>) -> Self {
        Self::Rejected {
            row_id: None,
            reason: None,
            detail: format!("platform detection failed: {}", detail.into()),
        }
    }

    /// Apply a supported row's memory ceiling to the existing agent limit.
    ///
    /// An explicitly smaller configured limit remains in force. A larger
    /// configured value is reduced to the detected-and-row-bounded maximum.
    pub fn apply_memory_limit(&self, config: &mut AgentConfig) {
        let Self::Supported {
            capability: Some(capability),
            ..
        } = self
        else {
            return;
        };
        let resolved = capability.max_resident_model_memory();
        config.device_memory_bytes = Some(
            config
                .device_memory_bytes
                .map_or(resolved, |configured| configured.min(resolved)),
        );
    }

    /// Reject an unsupported outcome before bundle preparation or model
    /// load.
    ///
    /// # Errors
    ///
    /// [`AgentError::PlatformNotAdmissible`] carrying the typed reason
    /// where the frozen vocabulary has one. The reason is projected rather
    /// than only rendered, so a caller reads it off the error record
    /// instead of parsing prose.
    pub fn ensure_supported(&self) -> AgentResult<()> {
        match self {
            Self::Supported { .. } => Ok(()),
            Self::Rejected { reason, detail, .. } => Err(AgentError::PlatformNotAdmissible {
                reason: *reason,
                detail: detail.clone(),
            }),
        }
    }

    /// Admit a deploy of `backend_path` on this machine.
    ///
    /// # Errors
    ///
    /// The startup rejection when this machine is not admissible at all, or
    /// [`PlatformReason::MissingBackendPackage`] for this backend path.
    pub fn admit_backend(
        &self,
        registry: &PlatformRegistry,
        backend_path: &str,
    ) -> AgentResult<()> {
        let Self::Supported {
            row_id,
            installed_packages,
            ..
        } = self
        else {
            return self.ensure_supported();
        };
        // Looked up rather than held, so this value does not borrow the
        // registry and the coordinator can own both.
        let Some(row) = registry.row(row_id) else {
            return Err(AgentError::PlatformNotAdmissible {
                reason: Some(PlatformReason::MissingDriverRuntime),
                detail: format!(
                    "row `{row_id}` was admitted at startup but is no longer in the registry"
                ),
            });
        };
        check_backend_packages(row, backend_path, installed_packages)
    }

    #[must_use]
    pub fn row_id(&self) -> Option<&str> {
        match self {
            Self::Supported { row_id, .. } => Some(row_id),
            Self::Rejected { row_id, .. } => row_id.as_deref(),
        }
    }

    #[must_use]
    pub fn reason(&self) -> Option<PlatformReason> {
        match self {
            Self::Supported { .. } => None,
            Self::Rejected { reason, .. } => *reason,
        }
    }

    /// The posture in force, where this machine is admissible at all.
    #[must_use]
    pub fn posture(&self) -> Option<(AdmissionPosture, &'static str)> {
        match self {
            Self::Supported {
                posture,
                posture_from,
                ..
            } => Some((*posture, posture_from)),
            Self::Rejected { .. } => None,
        }
    }

    /// Whether an admitted machine rests on validation evidence that
    /// covers it, or was admitted on technical prerequisites.
    #[must_use]
    pub fn validated(&self) -> Option<bool> {
        match self {
            Self::Supported { validated, .. } => Some(*validated),
            Self::Rejected { .. } => None,
        }
    }

    #[must_use]
    pub fn capability(&self) -> Option<&PlatformCapability> {
        match self {
            Self::Supported { capability, .. } => capability.as_ref(),
            Self::Rejected { .. } => None,
        }
    }
}

/// The first declared component that is absent or at the wrong version.
/// The first component a row records that this machine does not report at
/// all.
///
/// The version is deliberately not compared. `first_stack_mismatch` exists
/// for a machine claiming a row's validation, where the recorded versions
/// are the ones the evidence was taken on and matching them exactly is the
/// claim. A machine admitted on prerequisites makes no such claim: it needs
/// the driver and runtime to be *there*.
fn first_absent_component(row: &PlatformSupportRow, observed: &ObservedStack) -> Option<String> {
    row.kernel_driver_stack()
        .components
        .iter()
        .find(|component| !observed.components.contains_key(&component.component))
        .map(|component| {
            format!(
                "row `{}` requires {}, which this machine does not report",
                row.row_id(),
                component.component
            )
        })
}

fn first_stack_mismatch(row: &PlatformSupportRow, observed: &ObservedStack) -> Option<String> {
    for component in &row.kernel_driver_stack().components {
        match observed.components.get(&component.component) {
            None => {
                return Some(format!(
                    "row `{}` records {} {}, which this machine does not report",
                    row.row_id(),
                    component.component,
                    component.version
                ))
            }
            Some(found) if found != &component.version => {
                return Some(format!(
                    "row `{}` records {} {}, this machine reports {found}",
                    row.row_id(),
                    component.component,
                    component.version
                ))
            }
            Some(_) => {}
        }
    }
    None
}

/// Are the packages the matched row requires for `backend_path` installed?
///
/// A row that declares no package set for the path a bundle names is not a
/// pass — it is a row that never claimed to serve that path, which is
/// exactly the deploy that has no evidence behind it.
///
/// # Errors
///
/// [`AgentError::PlatformNotAdmissible`] naming
/// [`PlatformReason::MissingBackendPackage`].
pub fn check_backend_packages(
    row: &PlatformSupportRow,
    backend_path: &str,
    installed: &BTreeSet<String>,
) -> AgentResult<()> {
    let Some(set) = row
        .backend_packages()
        .iter()
        .find(|set| set.backend_path == backend_path)
    else {
        return Err(AgentError::PlatformNotAdmissible {
            reason: Some(PlatformReason::MissingBackendPackage),
            detail: format!(
                "row `{}` declares no package set for backend path `{backend_path}`",
                row.row_id()
            ),
        });
    };
    let missing: Vec<&str> = set
        .packages
        .iter()
        .filter(|package| !installed.contains(*package))
        .map(String::as_str)
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(AgentError::PlatformNotAdmissible {
        reason: Some(PlatformReason::MissingBackendPackage),
        detail: format!(
            "row `{}` requires {} for backend path `{backend_path}`; not installed: {}",
            row.row_id(),
            set.packages.join(", "),
            missing.join(", ")
        ),
    })
}
