// SPDX-License-Identifier: Apache-2.0
//
// Refusing a deploy the platform cannot honour, before anything is staged.
//
// Two questions, asked at different times because they have different
// inputs. Whether this machine matches a support row at all is a fact
// about the machine, so it is settled once at startup. Whether the
// packages a row requires for a given backend path are installed depends
// on which backend the bundle names, so it is asked per deploy.
//
// Evaluation is pure and takes what was observed as an argument. The
// agent gathers observations at startup; tests supply them directly,
// which is the only way to exercise a driver-version mismatch without the
// driver.
//
// Every rejection carries a typed reason from the frozen platform
// vocabulary — except one. A machine whose hardware matches a row but
// whose machine shape no row's evidence covers has no value in that
// vocabulary, and borrowing the nearest one would send an operator to
// reinstall an OS that is fine.

use std::collections::{BTreeMap, BTreeSet};

use tensorplate_platform::{
    DetectedPlatform, PlatformReason, PlatformRegistry, PlatformSupportRow, RowMatch,
};

/// What was observed about this machine's driver and runtime stack, and
/// which packages are installed.
///
/// Supplied rather than probed, so the evaluation below is testable
/// without the hardware or the packages it describes.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ObservedStack {
    /// Component identifier to version, in the row's vocabulary
    /// (`nvidia_driver`, `cuda`, `tensorrt`, …).
    pub components: BTreeMap<String, String>,
    /// Package names installed on this host.
    pub installed_packages: BTreeSet<String>,
}

/// Why the platform cannot admit a deploy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlatformRejection {
    /// One of the frozen typed platform reasons.
    Reason {
        reason: PlatformReason,
        detail: String,
    },
    /// The hardware matches a row but the machine shape it is running on
    /// is not one any row's evidence covers.
    ///
    /// Deliberately not carrying a [`PlatformReason`]: the frozen
    /// vocabulary has no value for "wrong machine shape", and the nearest
    /// candidates all name a dimension that is actually fine. Telling an
    /// operator their OS version is unsupported when their OS is correct
    /// and their chassis is not sends them to fix the wrong thing.
    OutsideValidatedEnvironment { detail: String },
}

impl PlatformRejection {
    /// The frozen reason, where this rejection has one.
    #[must_use]
    pub fn reason(&self) -> Option<PlatformReason> {
        match self {
            Self::Reason { reason, .. } => Some(*reason),
            Self::OutsideValidatedEnvironment { .. } => None,
        }
    }

    /// Operator-facing detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::Reason { detail, .. } | Self::OutsideValidatedEnvironment { detail } => detail,
        }
    }
}

/// The bundle-independent platform verdict, settled once at startup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlatformVerdict {
    /// This machine is a row that carries a claim, and every driver and
    /// runtime component that row declares is present at the version it
    /// declares.
    Admitted {
        row_id: String,
    },
    Rejected(PlatformRejection),
}

/// Settle the bundle-independent half: does this machine match a row that
/// carries a claim, and does its driver/runtime stack match what that row
/// records?
///
/// Component requirements come from the matched row, never from a constant
/// here. A row whose evidence run has not happened yet declares no
/// components, and this check is silent for it rather than inventing a
/// requirement — an empty list means "not yet recorded", not "nothing
/// required".
#[must_use]
pub fn evaluate_platform(
    registry: &PlatformRegistry,
    detected: &DetectedPlatform,
    observed: &ObservedStack,
) -> PlatformVerdict {
    let row = match registry.resolve(detected) {
        RowMatch::Supported(row) => row,
        RowMatch::PlannedNotValidated(row) => {
            return PlatformVerdict::Rejected(PlatformRejection::Reason {
                reason: PlatformReason::RowPlannedNotValidated,
                detail: format!(
                    "this machine matches row `{}`, which is defined but carries no validation \
                     evidence",
                    row.row_id()
                ),
            })
        }
        RowMatch::Experimental(row) => {
            return PlatformVerdict::Rejected(PlatformRejection::Reason {
                reason: PlatformReason::RowPlannedNotValidated,
                detail: format!(
                    "this machine matches row `{}`, an Experimental integration that is not a \
                     supported combination",
                    row.row_id()
                ),
            })
        }
        RowMatch::OutsideValidatedEnvironment { candidate } => {
            return PlatformVerdict::Rejected(PlatformRejection::OutsideValidatedEnvironment {
                detail: match candidate {
                    Some(row) => format!(
                        "this machine's hardware matches row `{}`, but that row's evidence was \
                         recorded on a different machine shape",
                        row.row_id()
                    ),
                    None => "this machine's hardware matches a supported row, but no row's \
                             evidence covers the machine shape it is running on"
                        .to_string(),
                },
            })
        }
        RowMatch::Unsupported(reason) => {
            return PlatformVerdict::Rejected(PlatformRejection::Reason {
                reason,
                detail: format!("this machine matches no support row ({})", reason.as_str()),
            })
        }
    };

    if let Some(missing) = first_stack_mismatch(row, observed) {
        return PlatformVerdict::Rejected(PlatformRejection::Reason {
            reason: PlatformReason::MissingDriverRuntime,
            detail: missing,
        });
    }

    PlatformVerdict::Admitted {
        row_id: row.row_id().to_string(),
    }
}

/// The first declared component that is absent or at the wrong version.
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

/// Per-deploy: are the packages the matched row requires for `backend_path`
/// installed?
///
/// Asked here rather than at startup because which backend path matters is
/// the bundle's choice. A row that declares no package set for the path a
/// bundle names is not a pass — it is a row that never claimed to serve
/// that path, which is exactly the case a deploy must not slip through.
///
/// # Errors
///
/// Returns [`PlatformRejection`] naming
/// [`PlatformReason::MissingBackendPackage`].
pub fn check_backend_packages(
    row: &PlatformSupportRow,
    backend_path: &str,
    installed: &BTreeSet<String>,
) -> Result<(), PlatformRejection> {
    let Some(set) = row
        .backend_packages()
        .iter()
        .find(|set| set.backend_path == backend_path)
    else {
        return Err(PlatformRejection::Reason {
            reason: PlatformReason::MissingBackendPackage,
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
    Err(PlatformRejection::Reason {
        reason: PlatformReason::MissingBackendPackage,
        detail: format!(
            "row `{}` requires {} for backend path `{backend_path}`; not installed: {}",
            row.row_id(),
            set.packages.join(", "),
            missing.join(", ")
        ),
    })
}

/// The platform verdict plus the observations it was reached from, held by
/// the coordinator so deploy admission is O(1) rather than re-probing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlatformAdmission {
    verdict: PlatformVerdict,
    installed_packages: BTreeSet<String>,
}

impl PlatformAdmission {
    #[must_use]
    pub fn new(verdict: PlatformVerdict, installed_packages: BTreeSet<String>) -> Self {
        Self {
            verdict,
            installed_packages,
        }
    }

    #[must_use]
    pub fn verdict(&self) -> &PlatformVerdict {
        &self.verdict
    }

    /// Admit a deploy of `backend_path`, or say why not.
    ///
    /// # Errors
    ///
    /// Returns the startup verdict's rejection when this machine is not
    /// admissible at all, or a package rejection for this backend path.
    pub fn admit(
        &self,
        registry: &PlatformRegistry,
        backend_path: &str,
    ) -> Result<(), PlatformRejection> {
        let PlatformVerdict::Admitted { row_id } = &self.verdict else {
            let PlatformVerdict::Rejected(rejection) = &self.verdict else {
                unreachable!("verdict is either admitted or rejected")
            };
            return Err(rejection.clone());
        };
        // The row is looked up rather than held, so the admission value
        // does not borrow the registry and the coordinator can own both.
        let Some(row) = registry.row(row_id) else {
            return Err(PlatformRejection::Reason {
                reason: PlatformReason::MissingDriverRuntime,
                detail: format!(
                    "row `{row_id}` was admitted at startup but is no longer in the registry"
                ),
            });
        };
        check_backend_packages(row, backend_path, &self.installed_packages)
    }
}
