// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F02: Rust mirror of `protocol/schemas/agent_state.json`.
//
// This is the durable desired-state record `tensorplate-agent` writes to
// disk after every state transition. The on-disk file is rewritten
// atomically by the agent's state store; this module only models the
// shape, not the I/O.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::deploy_transaction::DeployState;
use crate::error::ErrorCode;
use crate::{DecodeError, ValidatePayload, SCHEMA_VERSION};

/// Stored kind of in-flight transaction. Both deploy and rollback walk
/// the same phase enum but report distinct kinds so recovery can tell
/// them apart.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionKind {
    Deploy,
    Rollback,
}

/// One persisted deployment record (active, previous active, or candidate).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeploymentRecord {
    pub deployment_id: String,
    pub bundle_digest: String,
    pub bundle_name: String,
    pub bundle_version: String,
    pub backend_hint: String,
    pub model_class: String,
    pub staged_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted_monotonic_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

/// Persistable typed error attached to a failed/quarantined transaction.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ErrorRecord {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub recoverable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

impl ErrorRecord {
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            recoverable: false,
            context: None,
        }
    }

    #[must_use]
    pub fn recoverable(mut self, recoverable: bool) -> Self {
        self.recoverable = recoverable;
        self
    }

    #[must_use]
    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }
}

/// One in-flight transaction journal entry.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TransactionRecord {
    pub transaction_id: String,
    pub deployment_id: String,
    pub phase: DeployState,
    pub kind: TransactionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_monotonic_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition_monotonic_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<ErrorRecord>,
}

/// One quarantine entry persisted outside the active/previous/candidate slots.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QuarantineRecord {
    pub transaction_id: String,
    pub deployment_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_digest: Option<String>,
    pub phase: DeployState,
    pub error: ErrorRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quarantined_monotonic_ns: Option<u64>,
}

/// Root durable-state record.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentState {
    pub schema_version: String,
    pub store_version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<DeploymentRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_active: Option<DeploymentRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<DeploymentRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_flight_transaction: Option<TransactionRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<ErrorRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quarantined: Vec<QuarantineRecord>,
}

impl AgentState {
    /// Fresh state with `store_version = 1` and the current schema version.
    #[must_use]
    pub fn fresh() -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            store_version: 1,
            ..Self::default()
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AgentStateError {
    #[error("AgentState.store_version must be >= 1")]
    InvalidStoreVersion,
}

impl ValidatePayload for AgentState {
    fn validate_payload(self) -> Result<Self, DecodeError> {
        if self.store_version == 0 {
            return Err(DecodeError::InvalidPayload(
                AgentStateError::InvalidStoreVersion.to_string(),
            ));
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::collections::BTreeMap;

    use super::{
        AgentState, DeploymentRecord, ErrorRecord, QuarantineRecord, TransactionKind,
        TransactionRecord, SCHEMA_VERSION,
    };
    use crate::deploy_transaction::DeployState;
    use crate::error::ErrorCode;
    use crate::{decode_with_version_check, DecodeError};

    fn sample_deployment() -> DeploymentRecord {
        DeploymentRecord {
            deployment_id: "deploy-1".into(),
            bundle_digest: "sha256:cafe".into(),
            bundle_name: "yolov8n".into(),
            bundle_version: "1.0.0".into(),
            backend_hint: "tensorrt".into(),
            model_class: "vision".into(),
            staged_path: "/var/lib/tensorplate/staging/deploy-1".into(),
            promoted_monotonic_ns: Some(123_456_789),
            labels: BTreeMap::new(),
        }
    }

    #[test]
    fn fresh_round_trips() {
        let s = AgentState::fresh();
        let raw = serde_json::to_string(&s).expect("serialize");
        let back: AgentState = decode_with_version_check(&raw).expect("decode");
        assert_eq!(s, back);
        assert_eq!(back.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn populated_round_trips() {
        let s = AgentState {
            schema_version: SCHEMA_VERSION.to_string(),
            store_version: 5,
            active: Some(sample_deployment()),
            previous_active: Some(sample_deployment()),
            candidate: None,
            in_flight_transaction: Some(TransactionRecord {
                transaction_id: "tx-1".into(),
                deployment_id: "deploy-1".into(),
                phase: DeployState::Warmed,
                kind: TransactionKind::Deploy,
                bundle_digest: Some("sha256:cafe".into()),
                bundle_path: Some("/bundles/deploy-1".into()),
                correlation_id: Some("corr-1".into()),
                started_monotonic_ns: Some(1),
                last_transition_monotonic_ns: Some(2),
                failure: None,
            }),
            last_error: None,
            quarantined: vec![QuarantineRecord {
                transaction_id: "tx-q".into(),
                deployment_id: "deploy-bad".into(),
                bundle_digest: Some("sha256:dead".into()),
                phase: DeployState::Prepared,
                error: ErrorRecord::new(ErrorCode::OomError, "exceeded device memory"),
                quarantined_monotonic_ns: Some(42),
            }],
        };
        let raw = serde_json::to_string(&s).expect("serialize");
        let back: AgentState = decode_with_version_check(&raw).expect("decode");
        assert_eq!(s, back);
    }

    #[test]
    fn rejects_unknown_schema_version() {
        let raw = r#"{"schema_version":"99.99","store_version":1}"#;
        let err = decode_with_version_check::<AgentState>(raw).expect_err("rejected");
        assert!(matches!(err, DecodeError::UnsupportedSchemaVersion { .. }));
    }

    #[test]
    fn rejects_zero_store_version() {
        let raw = format!(r#"{{"schema_version":"{SCHEMA_VERSION}","store_version":0}}"#);
        let err = decode_with_version_check::<AgentState>(&raw).expect_err("rejected");
        assert!(matches!(err, DecodeError::InvalidPayload(_)));
    }
}
