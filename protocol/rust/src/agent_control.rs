// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F01: Rust mirror of `protocol/schemas/agent_control.json`.
//
// The wire encoding is newline-delimited JSON: one request, one response,
// one socket connection. See `docs/architecture/agent-control-api.md`
// for the transport decision.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::deploy_transaction::DeployState;
use crate::error::ErrorCode;
use crate::supervision_event::{SupervisionAgentState, SupervisionServingState};
use crate::{DecodeError, ValidatePayload, SCHEMA_VERSION};

/// Maximum encoded length of a deployment identifier.
///
/// The identifier becomes one filesystem path segment and part of a worker
/// config filename, so keep it comfortably below common component limits.
pub const MAX_DEPLOYMENT_ID_BYTES: usize = 128;

/// Return whether `value` is safe to use as one filesystem path segment.
#[must_use]
pub fn is_valid_deployment_id(value: &str) -> bool {
    (1..=MAX_DEPLOYMENT_ID_BYTES).contains(&value.len())
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Operation discriminator.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlOp {
    Deploy,
    Status,
    Rollback,
    Health,
    Version,
}

/// Deploy operation payload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeployRequest {
    pub bundle_path: String,
    pub deployment_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_bundle_digest: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RollbackRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StatusRequest {
    #[serde(default = "default_true")]
    pub include_quarantine: bool,
}

impl Default for StatusRequest {
    fn default() -> Self {
        Self {
            include_quarantine: true,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Top-level control request envelope.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ControlRequest {
    pub schema_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    pub op: ControlOp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy: Option<DeployRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback: Option<RollbackRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<StatusRequest>,
}

impl ControlRequest {
    /// Build a `deploy` request, stamping the schema version automatically.
    #[must_use]
    pub fn deploy(correlation_id: Option<String>, payload: DeployRequest) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            correlation_id,
            op: ControlOp::Deploy,
            deploy: Some(payload),
            rollback: None,
            status: None,
        }
    }

    #[must_use]
    pub fn rollback(correlation_id: Option<String>, payload: RollbackRequest) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            correlation_id,
            op: ControlOp::Rollback,
            deploy: None,
            rollback: Some(payload),
            status: None,
        }
    }

    #[must_use]
    pub fn status(correlation_id: Option<String>, payload: StatusRequest) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            correlation_id,
            op: ControlOp::Status,
            deploy: None,
            rollback: None,
            status: Some(payload),
        }
    }

    #[must_use]
    pub fn health(correlation_id: Option<String>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            correlation_id,
            op: ControlOp::Health,
            deploy: None,
            rollback: None,
            status: None,
        }
    }

    #[must_use]
    pub fn version(correlation_id: Option<String>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            correlation_id,
            op: ControlOp::Version,
            deploy: None,
            rollback: None,
            status: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ControlRequestError {
    #[error("deploy operation requires a `deploy` payload")]
    MissingDeployPayload,
    #[error("rollback operation must not carry a `deploy` payload")]
    UnexpectedDeployPayload,
    #[error("deploy operation must not carry a `rollback` payload")]
    UnexpectedRollbackPayload,
    #[error("deploy.bundle_path must be non-empty")]
    EmptyBundlePath,
    #[error("deploy.deployment_id must be non-empty")]
    EmptyDeploymentId,
    #[error(
        "deploy.deployment_id must be 1 to 128 bytes and contain only ASCII letters, digits, `-`, `_`, or `.`; `.` and `..` are reserved"
    )]
    InvalidDeploymentId,
    #[error("deploy.expected_bundle_digest, if present, must follow the `algo:hex` form")]
    InvalidExpectedDigest,
}

fn looks_like_digest(d: &str) -> bool {
    if let Some((algo, hex)) = d.split_once(':') {
        let algo_ok = !algo.is_empty()
            && algo
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        let hex_ok = !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit());
        algo_ok && hex_ok
    } else {
        false
    }
}

impl ValidatePayload for ControlRequest {
    fn validate_payload(self) -> Result<Self, DecodeError> {
        let invalid = |err: ControlRequestError| DecodeError::InvalidPayload(err.to_string());
        match self.op {
            ControlOp::Deploy => {
                let Some(ref d) = self.deploy else {
                    return Err(invalid(ControlRequestError::MissingDeployPayload));
                };
                if self.rollback.is_some() {
                    return Err(invalid(ControlRequestError::UnexpectedRollbackPayload));
                }
                if d.bundle_path.is_empty() {
                    return Err(invalid(ControlRequestError::EmptyBundlePath));
                }
                if d.deployment_id.is_empty() {
                    return Err(invalid(ControlRequestError::EmptyDeploymentId));
                }
                if !is_valid_deployment_id(&d.deployment_id) {
                    return Err(invalid(ControlRequestError::InvalidDeploymentId));
                }
                if let Some(ref dg) = d.expected_bundle_digest {
                    if !looks_like_digest(dg) {
                        return Err(invalid(ControlRequestError::InvalidExpectedDigest));
                    }
                }
            }
            ControlOp::Rollback => {
                if self.deploy.is_some() {
                    return Err(invalid(ControlRequestError::UnexpectedDeployPayload));
                }
            }
            ControlOp::Status | ControlOp::Health | ControlOp::Version => {
                if self.deploy.is_some() {
                    return Err(invalid(ControlRequestError::UnexpectedDeployPayload));
                }
                if self.rollback.is_some() {
                    return Err(invalid(ControlRequestError::UnexpectedRollbackPayload));
                }
            }
        }
        Ok(self)
    }
}

/// Coarse response outcome discriminator.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseStatus {
    Ok,
    Error,
    Busy,
    NotFound,
    Unavailable,
}

/// Typed error attached to error responses.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResponseError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

impl ResponseError {
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            context: None,
        }
    }

    #[must_use]
    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }
}

/// Failure metadata reported inside a deploy status.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeployFailureSummary {
    pub error_code: ErrorCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub recoverable: bool,
}

/// Snapshot of an in-flight or completed deploy transaction.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeployStatus {
    pub phase: DeployState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_monotonic_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition_monotonic_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<DeployFailureSummary>,
}

/// Snapshot of one active/previous-active/candidate deployment record.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeploymentSummary {
    pub deployment_id: String,
    pub bundle_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted_monotonic_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serving_url: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QuarantineSummary {
    pub transaction_id: String,
    pub deployment_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_digest: Option<String>,
    pub phase: DeployState,
    pub error_code: ErrorCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quarantined_monotonic_ns: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryAction {
    NoOp,
    ResumeVerify,
    ResumeStage,
    ResumePrepare,
    ResumeWarm,
    FinalizePromotion,
    RestoreActive,
    QuarantineCandidate,
    OperatorRequired,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RecoverySummary {
    pub action: RecoveryAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SupervisionStatusSummary {
    pub serving_state: SupervisionServingState,
    pub agent_state: SupervisionAgentState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_active: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_active: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    pub restart_count: u64,
    pub crash_loop_threshold: u64,
    pub crash_loop: bool,
    pub launch_sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure_code: Option<ErrorCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_restart_delay_ms: Option<u64>,
    pub stable_uptime_ms: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRunState {
    Ready,
    Degraded,
    Failed,
    #[default]
    Unknown,
}

/// Aggregate agent status payload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentStatus {
    pub agent_state: AgentRunState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<DeploymentSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_active: Option<DeploymentSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<DeploymentSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_flight_transaction: Option<DeployStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<ResponseError>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quarantined: Vec<QuarantineSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<RecoverySummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervision: Option<SupervisionStatusSummary>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ControlResponse {
    pub schema_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    pub status: ResponseStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy_status: Option<DeployStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_status: Option<AgentStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

impl ControlResponse {
    #[must_use]
    pub fn ok(correlation_id: Option<String>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            correlation_id,
            status: ResponseStatus::Ok,
            transaction_id: None,
            deploy_status: None,
            agent_status: None,
            error: None,
        }
    }

    #[must_use]
    pub fn error(correlation_id: Option<String>, error: ResponseError) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            correlation_id,
            status: ResponseStatus::Error,
            transaction_id: None,
            deploy_status: None,
            agent_status: None,
            error: Some(error),
        }
    }

    #[must_use]
    pub fn busy(correlation_id: Option<String>, message: impl Into<String>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            correlation_id,
            status: ResponseStatus::Busy,
            transaction_id: None,
            deploy_status: None,
            agent_status: None,
            error: Some(ResponseError::new(ErrorCode::NotReady, message)),
        }
    }

    #[must_use]
    pub fn unavailable(correlation_id: Option<String>, message: impl Into<String>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            correlation_id,
            status: ResponseStatus::Unavailable,
            transaction_id: None,
            deploy_status: None,
            agent_status: None,
            error: Some(ResponseError::new(ErrorCode::Unsupported, message)),
        }
    }
}

impl ValidatePayload for ControlResponse {
    fn validate_payload(self) -> Result<Self, DecodeError> {
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::default_trait_access)]

    use super::{
        is_valid_deployment_id, AgentRunState, AgentStatus, ControlOp, ControlRequest,
        ControlResponse, DeployRequest, DeployStatus, ResponseError, ResponseStatus,
        RollbackRequest, SupervisionStatusSummary, MAX_DEPLOYMENT_ID_BYTES, SCHEMA_VERSION,
    };
    use crate::deploy_transaction::DeployState;
    use crate::error::ErrorCode;
    use crate::supervision_event::{SupervisionAgentState, SupervisionServingState};
    use crate::{decode_with_version_check, DecodeError};
    use serde_json::json;

    #[test]
    fn deploy_request_round_trips() {
        let req = ControlRequest::deploy(
            Some("corr-1".into()),
            DeployRequest {
                bundle_path: "/var/lib/tensorplate/bundles/yolov8n".into(),
                deployment_id: "deploy-2024-1".into(),
                expected_bundle_digest: Some("sha256:cafebabe".into()),
                labels: Default::default(),
            },
        );
        let raw = serde_json::to_string(&req).expect("serialize");
        let back: ControlRequest = decode_with_version_check(&raw).expect("decode");
        assert_eq!(req, back);
        assert!(matches!(back.op, ControlOp::Deploy));
    }

    #[test]
    fn deploy_request_rejects_empty_bundle_path() {
        let raw = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","op":"deploy","deploy":{{"bundle_path":"","deployment_id":"x"}}}}"#
        );
        let err = decode_with_version_check::<ControlRequest>(&raw).expect_err("rejected");
        assert!(matches!(err, DecodeError::InvalidPayload(_)));
    }

    #[test]
    fn deployment_id_policy_rejects_path_components_and_unbounded_values() {
        assert!(is_valid_deployment_id("deploy-abc_1.2"));
        for invalid in ["", ".", "..", "../state", "a/b", "a\\b", "with space"] {
            assert!(
                !is_valid_deployment_id(invalid),
                "{invalid:?} must be rejected"
            );
        }
        assert!(is_valid_deployment_id(&"a".repeat(MAX_DEPLOYMENT_ID_BYTES)));
        assert!(!is_valid_deployment_id(
            &"a".repeat(MAX_DEPLOYMENT_ID_BYTES + 1)
        ));
    }

    #[test]
    fn deploy_request_rejects_unsafe_deployment_id() {
        let raw = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","op":"deploy","deploy":{{"bundle_path":"/tmp/bundle","deployment_id":"../state"}}}}"#
        );
        let err = decode_with_version_check::<ControlRequest>(&raw).expect_err("rejected");
        assert!(matches!(err, DecodeError::InvalidPayload(_)));
    }

    #[test]
    fn deployment_id_schema_matches_policy() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../schemas/agent_control.json"))
                .expect("parse schema");
        let validator =
            jsonschema::JSONSchema::compile(&schema).expect("schema compiles as Draft-07");
        let request = |deployment_id: &str| {
            json!({
                "schema_version": SCHEMA_VERSION,
                "op": "deploy",
                "deploy": {
                    "bundle_path": "/tmp/bundle",
                    "deployment_id": deployment_id
                }
            })
        };

        assert!(validator.is_valid(&request("deploy-abc_1.2")));
        for invalid in [".", "..", "../state", "a/b"] {
            assert!(!validator.is_valid(&request(invalid)));
        }
        assert!(!validator.is_valid(&request(&"a".repeat(MAX_DEPLOYMENT_ID_BYTES + 1))));
    }

    #[test]
    fn rollback_request_round_trips() {
        let req = ControlRequest::rollback(
            None,
            RollbackRequest {
                reason: Some("operator intervention".into()),
            },
        );
        let raw = serde_json::to_string(&req).expect("serialize");
        let back: ControlRequest = decode_with_version_check(&raw).expect("decode");
        assert_eq!(req, back);
    }

    #[test]
    fn rejects_unknown_schema_version() {
        let raw = r#"{"schema_version":"99.99","op":"health"}"#;
        let err = decode_with_version_check::<ControlRequest>(raw).expect_err("rejected");
        assert!(matches!(err, DecodeError::UnsupportedSchemaVersion { .. }));
    }

    #[test]
    fn response_status_carries_typed_error() {
        let resp = ControlResponse::error(
            Some("corr".into()),
            ResponseError::new(ErrorCode::Unsupported, "unknown backend"),
        );
        let raw = serde_json::to_string(&resp).expect("serialize");
        let back: ControlResponse = serde_json::from_str(&raw).expect("deserialize");
        assert_eq!(back.status, ResponseStatus::Error);
        assert_eq!(
            back.error.as_ref().expect("error present").code,
            ErrorCode::Unsupported
        );
    }

    #[test]
    fn agent_status_round_trips() {
        let resp = ControlResponse {
            schema_version: SCHEMA_VERSION.to_string(),
            correlation_id: None,
            status: ResponseStatus::Ok,
            transaction_id: None,
            deploy_status: Some(DeployStatus {
                phase: DeployState::Active,
                transaction_id: Some("tx".into()),
                deployment_id: Some("d".into()),
                bundle_digest: Some("sha256:ab".into()),
                started_monotonic_ns: Some(1),
                last_transition_monotonic_ns: Some(2),
                failure: None,
            }),
            agent_status: Some(AgentStatus {
                agent_state: AgentRunState::Ready,
                active: None,
                previous_active: None,
                candidate: None,
                in_flight_transaction: None,
                last_error: None,
                quarantined: vec![],
                recovery: None,
                supervision: None,
            }),
            error: None,
        };
        let raw = serde_json::to_string(&resp).expect("serialize");
        let back: ControlResponse = serde_json::from_str(&raw).expect("deserialize");
        assert_eq!(resp, back);
    }

    #[test]
    fn agent_status_with_supervision_round_trips() {
        let status = AgentStatus {
            agent_state: AgentRunState::Failed,
            active: None,
            previous_active: None,
            candidate: None,
            in_flight_transaction: None,
            last_error: None,
            quarantined: vec![],
            recovery: None,
            supervision: Some(SupervisionStatusSummary {
                serving_state: SupervisionServingState::CrashLoop,
                agent_state: SupervisionAgentState::Failed,
                desired_active: Some("d-1".into()),
                actual_active: None,
                backend: Some("mock".into()),
                restart_count: 5,
                crash_loop_threshold: 5,
                crash_loop: true,
                launch_sequence: 7,
                last_failure_code: Some(ErrorCode::InferenceFailed),
                last_failure_message: Some("worker exited".into()),
                next_restart_delay_ms: None,
                stable_uptime_ms: 0,
            }),
        };
        let raw = serde_json::to_string(&status).expect("serialize");
        let back: AgentStatus = serde_json::from_str(&raw).expect("deserialize");
        assert_eq!(status, back);
    }
}
