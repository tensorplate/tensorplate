// SPDX-License-Identifier: Apache-2.0
//
// V01-E12-F03: Rust mirror of `protocol/schemas/failure_reason.json`.
//
// FailureReason classifies typed `ErrorCode` values for operator-facing
// rendering (CLI doctor/status/logs) and metric aggregation. The mapping
// from reason to error code is exhaustive so consumers can render the
// reason and still surface the original code for grep-style assertions
// in release validation runs.

use serde::{Deserialize, Serialize};

use crate::error::ErrorCode;
use crate::{DecodeError, ValidatePayload, SCHEMA_VERSION};

/// Operator-visible failure reason code.
///
/// The set is union-stable: post-v0.1.0 additions append rather than
/// rename. The wire format is `snake_case`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureReason {
    /// Config schema or runtime config validation failure.
    ConfigInvalid,
    /// Bundle manifest schema or required-field validation failure.
    BundleSchemaInvalid,
    /// Bundle artifact checksum or integrity check failed.
    BundleIntegrityFailed,
    /// Required runtime (libtorch, TensorRT, Python) is not installed
    /// or not supported on this host.
    UnsupportedRuntime,
    /// Hardware capability (CUDA, AI engine) is not present.
    UnsupportedHardware,
    /// Backend adapter could not start or is not registered.
    BackendUnavailable,
    /// Backend adapter does not support the requested capability
    /// (precision, op set, async).
    BackendUnsupportedCapability,
    /// Tensor shape does not match the model contract.
    ShapeMismatch,
    /// Out-of-memory at load, prepare, or infer time.
    Oom,
    /// Operation exceeded its deadline.
    Timeout,
    /// Scheduler observed a missed deadline for a successful path.
    DeadlineMissed,
    /// Python/PyTorch sidecar failed to start.
    SidecarStartupFailed,
    /// Sidecar IPC response failed schema validation.
    SidecarMalformedResponse,
    /// Sidecar process exited unexpectedly.
    SidecarProcessExit,
    /// Worker has not reached `ready` yet (or rolled back to `not_ready`).
    WorkerNotReady,
    /// Worker exited (clean or fault).
    WorkerExit,
    /// Worker re-entered the failure path more than the supervision
    /// policy permits in the configured window (V01-E09).
    WorkerCrashLoop,
    /// Observability service did not observe a heartbeat within the
    /// configured grace + missed-threshold window (V01-E10).
    NoHeartbeat,
    /// Operating system rejected the action (file system, socket,
    /// device permission).
    PermissionDenied,
    /// Unexpected internal error; usually a bug. The producer must
    /// include a correlation id when emitting this reason.
    Internal,
}

/// Coarse grouping used by CLI rendering and metric aggregation.
/// Bounded to keep label cardinality finite.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    Config,
    Bundle,
    Platform,
    Backend,
    Sidecar,
    Supervision,
    Heartbeat,
    Permission,
    Internal,
}

/// Severity hint for CLI rendering. Sinks may downsample lower
/// severities under backpressure (V01-E12-F06).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureSeverity {
    Warning,
    Error,
    Critical,
}

impl FailureReason {
    /// Stable serialised name (`snake_case`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ConfigInvalid => "config_invalid",
            Self::BundleSchemaInvalid => "bundle_schema_invalid",
            Self::BundleIntegrityFailed => "bundle_integrity_failed",
            Self::UnsupportedRuntime => "unsupported_runtime",
            Self::UnsupportedHardware => "unsupported_hardware",
            Self::BackendUnavailable => "backend_unavailable",
            Self::BackendUnsupportedCapability => "backend_unsupported_capability",
            Self::ShapeMismatch => "shape_mismatch",
            Self::Oom => "oom",
            Self::Timeout => "timeout",
            Self::DeadlineMissed => "deadline_missed",
            Self::SidecarStartupFailed => "sidecar_startup_failed",
            Self::SidecarMalformedResponse => "sidecar_malformed_response",
            Self::SidecarProcessExit => "sidecar_process_exit",
            Self::WorkerNotReady => "worker_not_ready",
            Self::WorkerExit => "worker_exit",
            Self::WorkerCrashLoop => "worker_crash_loop",
            Self::NoHeartbeat => "no_heartbeat",
            Self::PermissionDenied => "permission_denied",
            Self::Internal => "internal",
        }
    }

    /// Coarse grouping. Stable for the lifetime of v0.1.
    #[must_use]
    pub fn category(self) -> FailureCategory {
        match self {
            Self::ConfigInvalid => FailureCategory::Config,
            Self::BundleSchemaInvalid | Self::BundleIntegrityFailed => FailureCategory::Bundle,
            Self::UnsupportedRuntime | Self::UnsupportedHardware => FailureCategory::Platform,
            Self::BackendUnavailable
            | Self::BackendUnsupportedCapability
            | Self::ShapeMismatch
            | Self::Oom
            | Self::Timeout
            | Self::DeadlineMissed => FailureCategory::Backend,
            Self::SidecarStartupFailed
            | Self::SidecarMalformedResponse
            | Self::SidecarProcessExit => FailureCategory::Sidecar,
            Self::WorkerNotReady | Self::WorkerExit | Self::WorkerCrashLoop => {
                FailureCategory::Supervision
            }
            Self::NoHeartbeat => FailureCategory::Heartbeat,
            Self::PermissionDenied => FailureCategory::Permission,
            Self::Internal => FailureCategory::Internal,
        }
    }

    /// Default severity hint. Producers may override for specific
    /// contexts (a transient deadline_missed during warmup is a warning;
    /// a saturated deadline_missed is critical) by constructing the
    /// payload directly.
    #[must_use]
    pub fn default_severity(self) -> FailureSeverity {
        match self {
            Self::ConfigInvalid
            | Self::BundleSchemaInvalid
            | Self::BundleIntegrityFailed
            | Self::UnsupportedRuntime
            | Self::UnsupportedHardware
            | Self::BackendUnavailable
            | Self::SidecarStartupFailed
            | Self::SidecarProcessExit
            | Self::WorkerCrashLoop
            | Self::NoHeartbeat
            | Self::PermissionDenied
            | Self::Internal => FailureSeverity::Critical,
            Self::ShapeMismatch
            | Self::Oom
            | Self::Timeout
            | Self::BackendUnsupportedCapability
            | Self::SidecarMalformedResponse
            | Self::WorkerNotReady
            | Self::WorkerExit => FailureSeverity::Error,
            Self::DeadlineMissed => FailureSeverity::Warning,
        }
    }

    /// Coarse retry hint. The agent never auto-retries permission or
    /// bundle-integrity failures.
    #[must_use]
    pub fn default_retryable(self) -> bool {
        matches!(
            self,
            Self::Timeout
                | Self::DeadlineMissed
                | Self::WorkerExit
                | Self::SidecarProcessExit
                | Self::NoHeartbeat
        )
    }

    /// Canonical mapping to the existing `ErrorCode` taxonomy.
    ///
    /// CLI consumers render the reason and still surface the code for
    /// release validation grep assertions.
    #[must_use]
    pub fn error_code(self) -> ErrorCode {
        match self {
            Self::ConfigInvalid | Self::BundleSchemaInvalid | Self::BundleIntegrityFailed => {
                ErrorCode::ConfigInvalid
            }
            Self::UnsupportedRuntime
            | Self::UnsupportedHardware
            | Self::BackendUnsupportedCapability
            | Self::PermissionDenied => ErrorCode::Unsupported,
            Self::BackendUnavailable | Self::SidecarStartupFailed | Self::SidecarProcessExit => {
                ErrorCode::LoadFailed
            }
            Self::ShapeMismatch => ErrorCode::ShapeMismatch,
            Self::Oom => ErrorCode::OomError,
            Self::Timeout | Self::DeadlineMissed => ErrorCode::Timeout,
            Self::SidecarMalformedResponse => ErrorCode::InferenceFailed,
            Self::WorkerNotReady | Self::WorkerExit | Self::WorkerCrashLoop | Self::NoHeartbeat => {
                ErrorCode::NotReady
            }
            Self::Internal => ErrorCode::Internal,
        }
    }
}

impl std::fmt::Display for FailureReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Maximum bounded length for the optional `detail` string. Mirrors the
/// JSON schema constraint so encoders fail closed before emitting.
pub const MAX_FAILURE_DETAIL_BYTES: usize = 512;

/// Wire payload for `protocol/schemas/failure_reason.json`. The struct
/// is suitable for embedding in transaction status, inference error
/// envelopes, structured logs, and the V01-E12-F07 status projection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FailureReasonRecord {
    pub schema_version: String,
    pub reason: FailureReason,
    pub category: FailureCategory,
    pub severity: FailureSeverity,
    pub retryable: bool,
    pub error_code: ErrorCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}

impl FailureReasonRecord {
    /// Construct a record using the canonical category, severity, and
    /// retry hint for `reason`. Callers who need to override a hint can
    /// mutate the returned record before serialising.
    #[must_use]
    pub fn new(reason: FailureReason) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            reason,
            category: reason.category(),
            severity: reason.default_severity(),
            retryable: reason.default_retryable(),
            error_code: reason.error_code(),
            detail: None,
            correlation_id: None,
        }
    }

    /// Attach a bounded human-readable detail. Strings longer than
    /// [`MAX_FAILURE_DETAIL_BYTES`] are truncated on a UTF-8 boundary.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        let raw = detail.into();
        self.detail = Some(bounded_detail(raw));
        self
    }

    /// Attach a correlation id. The id is validated against the
    /// `[A-Za-z0-9_-]{1,64}` policy in `ValidatePayload`; out-of-policy
    /// strings are rejected at decode time.
    #[must_use]
    pub fn with_correlation_id(mut self, id: impl Into<String>) -> Self {
        self.correlation_id = Some(id.into());
        self
    }
}

fn bounded_detail(mut s: String) -> String {
    if s.len() <= MAX_FAILURE_DETAIL_BYTES {
        return s;
    }
    let mut cut = MAX_FAILURE_DETAIL_BYTES;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s
}

impl ValidatePayload for FailureReasonRecord {
    fn validate_payload(self) -> Result<Self, DecodeError> {
        if self.category != self.reason.category() {
            return Err(DecodeError::InvalidPayload(format!(
                "FailureReasonRecord.category `{:?}` does not match canonical category `{:?}` for reason `{}`",
                self.category,
                self.reason.category(),
                self.reason
            )));
        }
        if self.error_code != self.reason.error_code() {
            return Err(DecodeError::InvalidPayload(format!(
                "FailureReasonRecord.error_code `{}` does not match canonical mapping `{}` for reason `{}`",
                self.error_code,
                self.reason.error_code(),
                self.reason
            )));
        }
        if let Some(detail) = &self.detail {
            if detail.len() > MAX_FAILURE_DETAIL_BYTES {
                return Err(DecodeError::InvalidPayload(format!(
                    "FailureReasonRecord.detail exceeds {MAX_FAILURE_DETAIL_BYTES} bytes"
                )));
            }
        }
        if let Some(id) = &self.correlation_id {
            validate_correlation_id_chars(id)?;
        }
        Ok(self)
    }
}

fn validate_correlation_id_chars(id: &str) -> Result<(), DecodeError> {
    if id.is_empty() || id.len() > 64 {
        return Err(DecodeError::InvalidPayload(
            "FailureReasonRecord.correlation_id must be 1..=64 bytes".into(),
        ));
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(DecodeError::InvalidPayload(
            "FailureReasonRecord.correlation_id must match [A-Za-z0-9_-]+".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{
        FailureCategory, FailureReason, FailureReasonRecord, FailureSeverity,
        MAX_FAILURE_DETAIL_BYTES,
    };
    use crate::error::ErrorCode;
    use crate::{decode_with_version_check, DecodeError, SCHEMA_VERSION};

    #[test]
    fn canonical_mapping_matches_taxonomy() {
        let cases = [
            (FailureReason::ConfigInvalid, ErrorCode::ConfigInvalid),
            (
                FailureReason::BundleIntegrityFailed,
                ErrorCode::ConfigInvalid,
            ),
            (FailureReason::UnsupportedHardware, ErrorCode::Unsupported),
            (FailureReason::BackendUnavailable, ErrorCode::LoadFailed),
            (FailureReason::ShapeMismatch, ErrorCode::ShapeMismatch),
            (FailureReason::Oom, ErrorCode::OomError),
            (FailureReason::Timeout, ErrorCode::Timeout),
            (FailureReason::DeadlineMissed, ErrorCode::Timeout),
            (
                FailureReason::SidecarMalformedResponse,
                ErrorCode::InferenceFailed,
            ),
            (FailureReason::WorkerCrashLoop, ErrorCode::NotReady),
            (FailureReason::NoHeartbeat, ErrorCode::NotReady),
            (FailureReason::Internal, ErrorCode::Internal),
        ];
        for (reason, expected) in cases {
            assert_eq!(
                reason.error_code(),
                expected,
                "reason {reason} mapped to wrong code"
            );
        }
    }

    #[test]
    fn record_serialises_canonical_fields() {
        let r = FailureReasonRecord::new(FailureReason::WorkerCrashLoop)
            .with_detail("worker restarted 5 times in 30s")
            .with_correlation_id("deploy-42");
        let json = serde_json::to_string(&r).expect("serialise");
        let back: FailureReasonRecord = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(back.reason, FailureReason::WorkerCrashLoop);
        assert_eq!(back.category, FailureCategory::Supervision);
        assert_eq!(back.severity, FailureSeverity::Critical);
        assert!(!back.retryable);
        assert_eq!(back.error_code, ErrorCode::NotReady);
        assert_eq!(back.correlation_id.as_deref(), Some("deploy-42"));
    }

    #[test]
    fn detail_is_truncated_on_construction() {
        let r = FailureReasonRecord::new(FailureReason::Internal)
            .with_detail("x".repeat(MAX_FAILURE_DETAIL_BYTES + 32));
        let detail = r.detail.expect("detail present");
        assert!(detail.len() <= MAX_FAILURE_DETAIL_BYTES);
    }

    #[test]
    fn decode_rejects_category_mismatch() {
        let json = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","reason":"oom","category":"config","severity":"error","retryable":false,"error_code":"oom_error"}}"#
        );
        let err = decode_with_version_check::<FailureReasonRecord>(&json).expect_err("rejected");
        assert!(matches!(err, DecodeError::InvalidPayload(_)));
    }

    #[test]
    fn decode_rejects_error_code_mismatch() {
        let json = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","reason":"timeout","category":"backend","severity":"error","retryable":true,"error_code":"internal"}}"#
        );
        let err = decode_with_version_check::<FailureReasonRecord>(&json).expect_err("rejected");
        assert!(matches!(err, DecodeError::InvalidPayload(_)));
    }

    #[test]
    fn decode_rejects_invalid_correlation_id_chars() {
        let json = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","reason":"timeout","category":"backend","severity":"warning","retryable":true,"error_code":"timeout","correlation_id":"bad id"}}"#
        );
        let err = decode_with_version_check::<FailureReasonRecord>(&json).expect_err("rejected");
        assert!(matches!(err, DecodeError::InvalidPayload(_)));
    }

    #[test]
    fn decode_rejects_overlong_correlation_id() {
        let oversize = "a".repeat(65);
        let json = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","reason":"timeout","category":"backend","severity":"warning","retryable":true,"error_code":"timeout","correlation_id":"{oversize}"}}"#
        );
        let err = decode_with_version_check::<FailureReasonRecord>(&json).expect_err("rejected");
        assert!(matches!(err, DecodeError::InvalidPayload(_)));
    }
}
