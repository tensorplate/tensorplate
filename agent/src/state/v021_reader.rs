// SPDX-License-Identifier: Apache-2.0
//
// Test oracle: the durable-state reader every agent from 0.1.1 through 0.2.1
// ships, transcribed so the store's tests can ask what such an agent would
// do with the files the current writer leaves behind.
//
// Transcribed from the 0.2.1 release's `agent/src/state.rs` (`open`,
// `load_one`), `protocol/rust/src/agent_state.rs` (the record types) and
// `protocol/rust/src/lib.rs` (`decode_with_version_check` with the
// `DecodeError` messages, and the "0.1" it compares against), all unchanged
// since 0.1.1. Everything is copied rather than imported so the oracle stays
// the old reader when the current crate's enums, decoder or protocol
// version move. Like the original, nothing here rejects unknown fields.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    ConfigInvalid,
    LoadFailed,
    NotReady,
    ShapeMismatch,
    Unsupported,
    OomError,
    Timeout,
    InferenceFailed,
    Internal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployState {
    Received,
    Verified,
    Staged,
    CapacityChecked,
    Prepared,
    Warmed,
    Promoted,
    Active,
    Failed,
    RolledBack,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionKind {
    Deploy,
    Rollback,
}

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

/// The 0.2.1 `decode_with_version_check::<AgentState>`, with its error
/// messages.
pub fn decode(json: &str) -> Result<AgentState, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("malformed payload: {e}"))?;
    let observed = value
        .get("schema_version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "missing or non-string `schema_version` field".to_string())?;
    if observed != "0.1" {
        return Err(format!(
            "unsupported schema_version `{observed}` (expected `0.1`)"
        ));
    }
    let parsed: AgentState =
        serde_json::from_value(value).map_err(|e| format!("malformed payload: {e}"))?;
    if parsed.store_version == 0 {
        return Err("invalid payload: AgentState.store_version must be >= 1".into());
    }
    Ok(parsed)
}

/// What the 0.2.1 agent's `StateStore::open` yields for `state_dir`: the
/// state it would run on, or the text its `main` prints after
/// `state store error: ` before exiting with status 3.
pub fn open(state_dir: &Path) -> Result<AgentState, String> {
    let primary = state_dir.join("state.json");
    let backup = state_dir.join("state.json.bak");
    match load_one(&primary) {
        Ok(Some(s)) => Ok(s),
        Ok(None) => match load_one(&backup)? {
            Some(s) => Ok(s),
            None => Ok(AgentState {
                schema_version: "0.1".into(),
                store_version: 1,
                ..AgentState::default()
            }),
        },
        Err(primary_err) => match load_one(&backup) {
            Ok(Some(s)) => Ok(s),
            _ => Err(primary_err),
        },
    }
}

fn load_one(path: &Path) -> Result<Option<AgentState>, String> {
    let raw = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.to_string()),
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    decode(&raw).map(Some).map_err(|err| {
        format!(
            "durable state file is corrupt or malformed: decode {}: {err}",
            path.display()
        )
    })
}
