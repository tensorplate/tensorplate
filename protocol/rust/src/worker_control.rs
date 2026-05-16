// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F05: Rust mirror of `protocol/schemas/worker_control.json`.

use serde::{Deserialize, Serialize};

use crate::error::ErrorCode;
use crate::{DecodeError, ValidatePayload, SCHEMA_VERSION};

/// Stage in the agent's prepare/warm/promote sequence.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerOp {
    Prepare,
    CapacityCheck,
    Warm,
    Promote,
    ActiveStatus,
    Unload,
}

/// Reference to a verified, staged candidate the agent asks the worker to
/// load and warm.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CandidateRef {
    pub deployment_id: String,
    pub staged_path: String,
    pub bundle_digest: String,
    pub backend_hint: String,
    pub model_class: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_relative_path: Option<String>,
}

/// Outgoing agent -> worker request envelope.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkerControlRequest {
    pub schema_version: String,
    pub transaction_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_deployment_id: Option<String>,
    pub op: WorkerOp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<CandidateRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl WorkerControlRequest {
    /// Build a request with the schema version populated.
    #[must_use]
    pub fn new(
        op: WorkerOp,
        transaction_id: impl Into<String>,
        candidate: Option<CandidateRef>,
        timeout_ms: Option<u64>,
    ) -> Self {
        let candidate_deployment_id = candidate.as_ref().map(|c| c.deployment_id.clone());
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            transaction_id: transaction_id.into(),
            correlation_id: None,
            candidate_deployment_id,
            op,
            candidate,
            timeout_ms,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerControlRequestError {
    #[error("worker control request transaction_id must be non-empty")]
    EmptyTransactionId,
    #[error("worker control op `{0:?}` requires a candidate payload")]
    MissingCandidate(WorkerOp),
}

impl ValidatePayload for WorkerControlRequest {
    fn validate_payload(self) -> Result<Self, DecodeError> {
        let invalid = |err: WorkerControlRequestError| DecodeError::InvalidPayload(err.to_string());
        if self.transaction_id.is_empty() {
            return Err(invalid(WorkerControlRequestError::EmptyTransactionId));
        }
        match self.op {
            WorkerOp::Prepare | WorkerOp::CapacityCheck | WorkerOp::Warm | WorkerOp::Promote => {
                if self.candidate.is_none() {
                    return Err(invalid(WorkerControlRequestError::MissingCandidate(
                        self.op,
                    )));
                }
            }
            WorkerOp::ActiveStatus | WorkerOp::Unload => {}
        }
        Ok(self)
    }
}

/// Outcome discriminator.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatusOutcome {
    Ok,
    Error,
    NotReady,
    Timeout,
    Unsupported,
}

/// Typed error attached to a worker response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkerError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

impl WorkerError {
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            context: None,
        }
    }
}

/// Incoming worker -> agent response envelope.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkerControlResponse {
    pub schema_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    pub status: WorkerStatusOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_deployment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_deployment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<WorkerError>,
}

impl WorkerControlResponse {
    #[must_use]
    pub fn ok(transaction_id: impl Into<String>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            transaction_id: Some(transaction_id.into()),
            correlation_id: None,
            status: WorkerStatusOutcome::Ok,
            active_deployment_id: None,
            candidate_deployment_id: None,
            ready: None,
            error: None,
        }
    }

    #[must_use]
    pub fn ready(transaction_id: impl Into<String>) -> Self {
        let mut r = Self::ok(transaction_id);
        r.ready = Some(true);
        r
    }

    #[must_use]
    pub fn not_ready(transaction_id: impl Into<String>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            transaction_id: Some(transaction_id.into()),
            correlation_id: None,
            status: WorkerStatusOutcome::NotReady,
            active_deployment_id: None,
            candidate_deployment_id: None,
            ready: Some(false),
            error: None,
        }
    }

    #[must_use]
    pub fn error(transaction_id: impl Into<String>, error: WorkerError) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            transaction_id: Some(transaction_id.into()),
            correlation_id: None,
            status: WorkerStatusOutcome::Error,
            active_deployment_id: None,
            candidate_deployment_id: None,
            ready: None,
            error: Some(error),
        }
    }

    #[must_use]
    pub fn timeout(transaction_id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            transaction_id: Some(transaction_id.into()),
            correlation_id: None,
            status: WorkerStatusOutcome::Timeout,
            active_deployment_id: None,
            candidate_deployment_id: None,
            ready: None,
            error: Some(WorkerError::new(ErrorCode::Timeout, message)),
        }
    }
}

impl ValidatePayload for WorkerControlResponse {
    fn validate_payload(self) -> Result<Self, DecodeError> {
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{
        CandidateRef, WorkerControlRequest, WorkerControlResponse, WorkerError, WorkerOp,
        WorkerStatusOutcome,
    };
    use crate::error::ErrorCode;
    use crate::{decode_with_version_check, DecodeError};

    fn sample_candidate() -> CandidateRef {
        CandidateRef {
            deployment_id: "deploy-1".into(),
            staged_path: "/var/lib/tensorplate/staging/deploy-1".into(),
            bundle_digest: "sha256:cafe".into(),
            backend_hint: "tensorrt".into(),
            model_class: "vision".into(),
            bundle_name: Some("yolov8n".into()),
            bundle_version: Some("1.0.0".into()),
            artifact_relative_path: Some("model.engine".into()),
        }
    }

    #[test]
    fn prepare_round_trips() {
        let req = WorkerControlRequest::new(
            WorkerOp::Prepare,
            "tx-1",
            Some(sample_candidate()),
            Some(30_000),
        );
        let raw = serde_json::to_string(&req).expect("serialize");
        let back: WorkerControlRequest = decode_with_version_check(&raw).expect("decode");
        assert_eq!(req, back);
        assert_eq!(back.candidate_deployment_id.as_deref(), Some("deploy-1"));
    }

    #[test]
    fn prepare_without_candidate_is_rejected() {
        let req = WorkerControlRequest::new(WorkerOp::Prepare, "tx", None, None);
        let raw = serde_json::to_string(&req).expect("serialize");
        let err = decode_with_version_check::<WorkerControlRequest>(&raw).expect_err("rejected");
        assert!(matches!(err, DecodeError::InvalidPayload(_)));
    }

    #[test]
    fn response_constructors_carry_typed_status() {
        let ok = WorkerControlResponse::ok("tx-1");
        assert_eq!(ok.status, WorkerStatusOutcome::Ok);
        let err = WorkerControlResponse::error(
            "tx-1",
            WorkerError::new(ErrorCode::Unsupported, "backend unavailable"),
        );
        assert_eq!(err.status, WorkerStatusOutcome::Error);
        let t = WorkerControlResponse::timeout("tx-1", "warm timed out");
        assert_eq!(t.status, WorkerStatusOutcome::Timeout);
        assert_eq!(t.error.as_ref().expect("error").code, ErrorCode::Timeout);
    }
}
