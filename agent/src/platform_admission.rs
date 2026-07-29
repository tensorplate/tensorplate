// SPDX-License-Identifier: Apache-2.0
//
// Platform-row admission state owned by the agent.
//
// Detection and matching stay in tensorplate-platform. The agent keeps only
// the resolved outcome: either a supported row and its bounded capability,
// or the typed reason deployment must fail before model load.

use tensorplate_platform::{
    PlatformCapability, PlatformReason, PlatformRegistry, PlatformReport, RowMatch,
};

use crate::config::AgentConfig;
use crate::error::{AgentError, AgentResult};

/// Cached platform outcome evaluated once at agent startup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlatformAdmission {
    Supported {
        row_id: String,
        capability: Option<PlatformCapability>,
    },
    Rejected {
        row_id: Option<String>,
        reason: Option<PlatformReason>,
        detail: String,
    },
}

impl PlatformAdmission {
    /// Evaluate one observation against the installed registry.
    #[must_use]
    pub fn evaluate(registry: &PlatformRegistry, report: &PlatformReport) -> Self {
        let detected = report.detected_platform();
        match registry.resolve(&detected) {
            RowMatch::Supported(row) => Self::Supported {
                row_id: row.row_id().to_string(),
                capability: registry.resolved_capability(report),
            },
            RowMatch::PlannedNotValidated(row) => Self::Rejected {
                row_id: Some(row.row_id().to_string()),
                reason: Some(PlatformReason::RowPlannedNotValidated),
                detail: format!(
                    "platform row `{}` is planned but not validated",
                    row.row_id()
                ),
            },
            RowMatch::Experimental(row) => Self::Rejected {
                row_id: Some(row.row_id().to_string()),
                reason: None,
                detail: format!(
                    "platform row `{}` is experimental and not deployable",
                    row.row_id()
                ),
            },
            RowMatch::OutsideValidatedEnvironment { candidate } => Self::Rejected {
                row_id: candidate.map(|row| row.row_id().to_string()),
                reason: None,
                detail: candidate.map_or_else(
                    || "platform is outside every validated environment".to_string(),
                    |row| {
                        format!(
                            "platform is outside the validated environment for row `{}`",
                            row.row_id()
                        )
                    },
                ),
            },
            RowMatch::Unsupported(reason) => Self::Rejected {
                row_id: None,
                reason: Some(reason),
                detail: format!("platform is unsupported: {reason}"),
            },
        }
    }

    /// Record a detection failure so startup can remain diagnosable while
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

    /// Reject an unsupported outcome before bundle preparation or model load.
    pub fn ensure_supported(&self) -> AgentResult<()> {
        match self {
            Self::Supported { .. } => Ok(()),
            Self::Rejected { detail, .. } => Err(AgentError::UnsupportedHardware(detail.clone())),
        }
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

    #[must_use]
    pub fn capability(&self) -> Option<&PlatformCapability> {
        match self {
            Self::Supported { capability, .. } => capability.as_ref(),
            Self::Rejected { .. } => None,
        }
    }
}
