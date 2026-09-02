// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F01: Rust mirror of `protocol/schemas/agent_control.json`.
//
// The wire encoding is newline-delimited JSON: one request, one response,
// one socket connection. See `docs/architecture/agent-control-api.md`
// for the transport decision.

use std::collections::{BTreeMap, HashSet};

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

// serde's `skip_serializing_if` callback receives `&T`.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
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

/// Stable platform-signal names in the agent status projection.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformTelemetrySignalName {
    Thermal,
    Power,
    Throttle,
    Memory,
    GpuUtilization,
}

impl PlatformTelemetrySignalName {
    pub const ALL: [Self; 5] = [
        Self::Thermal,
        Self::Power,
        Self::Throttle,
        Self::Memory,
        Self::GpuUtilization,
    ];
}

/// Row-owned posture for a projected platform signal.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformTelemetryGate {
    LoadBearing,
    ContextOnly,
    NotApplicable,
}

/// What one platform-signal collector reported.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PlatformSignalOutcomeStatus {
    Collected,
    Unavailable { detail: String },
}

/// One row-declared signal in the status/evidence projection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlatformSignalTelemetryStatus {
    pub name: PlatformTelemetrySignalName,
    pub gate: PlatformTelemetryGate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_applicable_reason: Option<String>,
    /// Absent only when `gate` is `not_applicable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<PlatformSignalOutcomeStatus>,
}

/// Per-row memory facts in the status/evidence projection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlatformMemoryTelemetryStatus {
    pub memory_profile: crate::PlatformMemoryProfileName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_total_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accelerator_total_bytes: Option<u64>,
    /// Nominal capacity declared by the matched row.
    pub row_nominal_capacity_bytes: u64,
    /// Usable observed capacity bounded by the row's nominal claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_budget_bytes: Option<u64>,
    pub shares_one_pool: bool,
    pub gate: PlatformTelemetryGate,
}

/// Platform telemetry attached to aggregate agent status.
///
/// `signals` is empty until a live collector snapshot is supplied. Once
/// supplied it contains all row-declared signals, including explicit
/// unavailable outcomes for applicable collectors that omitted a result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlatformTelemetryStatus {
    pub row_id: String,
    /// Whether the selected row's evidence covers this exact machine.
    pub validated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<PlatformMemoryTelemetryStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signals: Vec<PlatformSignalTelemetryStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_reason: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub deployment_degraded: bool,
}

/// Validate invariants that serde's field-level representation cannot
/// express. The agent-generated projection satisfies these by construction;
/// control clients still validate them because the wire is a trust boundary.
fn validate_platform_telemetry(telemetry: &PlatformTelemetryStatus) -> Result<(), String> {
    if telemetry.row_id.trim().is_empty() {
        return Err("platform_telemetry.row_id must be non-empty".into());
    }
    if let Some(memory) = telemetry.memory.as_ref() {
        validate_memory_telemetry(memory)?;
    }

    validate_signal_telemetry(telemetry)
}

fn validate_memory_telemetry(memory: &PlatformMemoryTelemetryStatus) -> Result<(), String> {
    for (field, value) in [
        ("host_total_bytes", memory.host_total_bytes),
        ("accelerator_total_bytes", memory.accelerator_total_bytes),
        ("effective_budget_bytes", memory.effective_budget_bytes),
    ] {
        if value.is_some_and(|bytes| bytes > crate::json_numbers::MAX_SAFE_BYTES) {
            return Err(format!(
                "platform_telemetry.memory.{field} exceeds the maximum exact JSON integer"
            ));
        }
    }
    if !(1..=crate::json_numbers::MAX_SAFE_BYTES).contains(&memory.row_nominal_capacity_bytes) {
        return Err(
            "platform_telemetry.memory.row_nominal_capacity_bytes must be between 1 and the \
             maximum exact JSON integer"
                .into(),
        );
    }
    let shares_one_pool = matches!(
        memory.memory_profile,
        crate::PlatformMemoryProfileName::UnifiedMemory
    );
    if memory.shares_one_pool != shares_one_pool {
        return Err("platform_telemetry.memory.shares_one_pool contradicts memory_profile".into());
    }
    match (
        memory.accelerator_total_bytes,
        memory.effective_budget_bytes,
    ) {
        (Some(observed), Some(effective))
            if effective == observed.min(memory.row_nominal_capacity_bytes) => {}
        (None, None) => {}
        _ => {
            return Err(
                "platform_telemetry.memory.effective_budget_bytes must equal the observed \
                 accelerator capacity bounded by the row nominal capacity"
                    .into(),
            )
        }
    }
    Ok(())
}

fn validate_signal_telemetry(telemetry: &PlatformTelemetryStatus) -> Result<(), String> {
    if telemetry.signals.is_empty() {
        if telemetry.degraded_reason.is_some() || telemetry.deployment_degraded {
            return Err(
                "platform_telemetry without a signal snapshot cannot report signal degradation"
                    .into(),
            );
        }
        return Ok(());
    }
    if telemetry.signals.len() != PlatformTelemetrySignalName::ALL.len() {
        return Err("platform_telemetry.signals must contain exactly five entries".into());
    }
    let names: HashSet<_> = telemetry.signals.iter().map(|signal| signal.name).collect();
    if names.len() != PlatformTelemetrySignalName::ALL.len()
        || !PlatformTelemetrySignalName::ALL
            .iter()
            .all(|name| names.contains(name))
    {
        return Err(
            "platform_telemetry.signals must contain every stable signal name exactly once".into(),
        );
    }

    let mut failed = false;
    let mut deployment_degraded = false;
    for signal in &telemetry.signals {
        match signal.gate {
            PlatformTelemetryGate::NotApplicable => {
                if !signal
                    .not_applicable_reason
                    .as_deref()
                    .is_some_and(|reason| !reason.trim().is_empty())
                    || signal.outcome.is_some()
                {
                    return Err(format!(
                        "not-applicable platform signal `{:?}` requires a reason and no outcome",
                        signal.name
                    ));
                }
            }
            PlatformTelemetryGate::LoadBearing | PlatformTelemetryGate::ContextOnly => {
                if signal.not_applicable_reason.is_some() || signal.outcome.is_none() {
                    return Err(format!(
                        "applicable platform signal `{:?}` requires an outcome and no \
                         not-applicable reason",
                        signal.name
                    ));
                }
            }
        }
        if let Some(PlatformSignalOutcomeStatus::Unavailable { detail }) = signal.outcome.as_ref() {
            if detail.trim().is_empty() {
                return Err(format!(
                    "unavailable platform signal `{:?}` requires non-empty detail",
                    signal.name
                ));
            }
            failed = true;
            deployment_degraded |= signal.gate == PlatformTelemetryGate::LoadBearing;
        }
    }
    match (failed, telemetry.degraded_reason.as_deref()) {
        (false, None) | (true, Some("telemetry_degraded")) => {}
        _ => {
            return Err(
                "platform_telemetry.degraded_reason must be `telemetry_degraded` exactly when an \
                 applicable signal is unavailable"
                    .into(),
            )
        }
    }
    if telemetry.deployment_degraded != deployment_degraded {
        return Err(
            "platform_telemetry.deployment_degraded must match unavailable load-bearing signals"
                .into(),
        );
    }
    Ok(())
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform_telemetry: Option<PlatformTelemetryStatus>,
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
        if let Some(telemetry) = self
            .agent_status
            .as_ref()
            .and_then(|status| status.platform_telemetry.as_ref())
        {
            validate_platform_telemetry(telemetry).map_err(DecodeError::InvalidPayload)?;
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::default_trait_access)]

    use super::{
        is_valid_deployment_id, AgentRunState, AgentStatus, ControlOp, ControlRequest,
        ControlResponse, DeployRequest, DeployStatus, PlatformMemoryTelemetryStatus,
        PlatformSignalOutcomeStatus, PlatformSignalTelemetryStatus, PlatformTelemetryGate,
        PlatformTelemetrySignalName, PlatformTelemetryStatus, ResponseError, ResponseStatus,
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

    fn valid_platform_telemetry_wire() -> serde_json::Value {
        json!({
            "row_id": "ubuntu2404-x86-l4-g2s8",
            "validated": true,
            "memory": {
                "memory_profile": "discrete_gpu",
                "host_total_bytes": 32 * 1024 * 1024 * 1024_u64,
                "accelerator_total_bytes": 23_034 * 1024 * 1024_u64,
                "row_nominal_capacity_bytes": 24 * 1024 * 1024 * 1024_u64,
                "effective_budget_bytes": 23_034 * 1024 * 1024_u64,
                "shares_one_pool": false,
                "gate": "load_bearing"
            },
            "signals": [
                {"name": "thermal", "gate": "context_only", "outcome": {
                    "state": "unavailable", "detail": "collector failed"
                }},
                {"name": "power", "gate": "context_only", "outcome": {
                    "state": "collected"
                }},
                {"name": "throttle", "gate": "context_only", "outcome": {
                    "state": "collected"
                }},
                {"name": "memory", "gate": "load_bearing", "outcome": {
                    "state": "collected"
                }},
                {"name": "gpu_utilization", "gate": "context_only", "outcome": {
                    "state": "collected"
                }}
            ],
            "degraded_reason": "telemetry_degraded",
            "deployment_degraded": false
        })
    }

    fn response_wire(telemetry: &serde_json::Value) -> serde_json::Value {
        json!({
            "schema_version": SCHEMA_VERSION,
            "status": "ok",
            "agent_status": {
                "agent_state": "degraded",
                "platform_telemetry": telemetry
            }
        })
    }

    #[test]
    fn response_validation_rejects_inconsistent_platform_telemetry() {
        let valid = response_wire(&valid_platform_telemetry_wire());
        decode_with_version_check::<ControlResponse>(&valid.to_string())
            .expect("valid telemetry passes semantic validation");

        let mut short = valid.clone();
        short["agent_status"]["platform_telemetry"]["signals"]
            .as_array_mut()
            .expect("signals")
            .pop();

        let mut duplicate = valid.clone();
        duplicate["agent_status"]["platform_telemetry"]["signals"][4]["name"] = json!("thermal");

        let mut missing_outcome = valid.clone();
        missing_outcome["agent_status"]["platform_telemetry"]["signals"][1]
            .as_object_mut()
            .expect("signal")
            .remove("outcome");

        let mut not_applicable_with_outcome = valid.clone();
        not_applicable_with_outcome["agent_status"]["platform_telemetry"]["signals"][1]["gate"] =
            json!("not_applicable");
        not_applicable_with_outcome["agent_status"]["platform_telemetry"]["signals"][1]
            ["not_applicable_reason"] = json!("not exposed");

        let mut zero_nominal = valid.clone();
        zero_nominal["agent_status"]["platform_telemetry"]["memory"]
            ["row_nominal_capacity_bytes"] = json!(0);

        let mut oversized_nominal = valid.clone();
        oversized_nominal["agent_status"]["platform_telemetry"]["memory"]
            ["row_nominal_capacity_bytes"] = json!(crate::json_numbers::MAX_SAFE_BYTES + 1);

        let mut oversized_observation = valid.clone();
        oversized_observation["agent_status"]["platform_telemetry"]["memory"]["host_total_bytes"] =
            json!(crate::json_numbers::MAX_SAFE_BYTES + 1);

        let mut wrong_effective = valid.clone();
        wrong_effective["agent_status"]["platform_telemetry"]["memory"]["effective_budget_bytes"] =
            json!(25 * 1024 * 1024 * 1024_u64);

        let mut missing_reason = valid.clone();
        missing_reason["agent_status"]["platform_telemetry"]
            .as_object_mut()
            .expect("telemetry")
            .remove("degraded_reason");

        let mut wrong_deployment_posture = valid;
        wrong_deployment_posture["agent_status"]["platform_telemetry"]["deployment_degraded"] =
            json!(true);

        for (case, wire) in [
            ("short signal set", short),
            ("duplicate signal name", duplicate),
            ("applicable signal without outcome", missing_outcome),
            (
                "not-applicable signal with an outcome",
                not_applicable_with_outcome,
            ),
            ("zero nominal capacity", zero_nominal),
            (
                "nominal capacity outside exact JSON range",
                oversized_nominal,
            ),
            (
                "memory observation outside exact JSON range",
                oversized_observation,
            ),
            ("effective capacity above its bounds", wrong_effective),
            ("missing degraded reason", missing_reason),
            ("wrong deployment posture", wrong_deployment_posture),
        ] {
            let err =
                decode_with_version_check::<ControlResponse>(&wire.to_string()).expect_err(case);
            assert!(
                matches!(err, DecodeError::InvalidPayload(_)),
                "{case}: got {err:?}"
            );
        }
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
                platform_telemetry: Some(PlatformTelemetryStatus {
                    row_id: "ubuntu2404-x86-l4-g2s8".into(),
                    validated: true,
                    memory: Some(PlatformMemoryTelemetryStatus {
                        memory_profile: crate::PlatformMemoryProfileName::DiscreteGpu,
                        host_total_bytes: Some(32 * 1024 * 1024 * 1024),
                        accelerator_total_bytes: Some(23_034 * 1024 * 1024),
                        row_nominal_capacity_bytes: 24 * 1024 * 1024 * 1024,
                        effective_budget_bytes: Some(23_034 * 1024 * 1024),
                        shares_one_pool: false,
                        gate: PlatformTelemetryGate::LoadBearing,
                    }),
                    signals: vec![
                        PlatformSignalTelemetryStatus {
                            name: PlatformTelemetrySignalName::Thermal,
                            gate: PlatformTelemetryGate::ContextOnly,
                            not_applicable_reason: None,
                            outcome: Some(PlatformSignalOutcomeStatus::Unavailable {
                                detail: "collector failed".into(),
                            }),
                        },
                        PlatformSignalTelemetryStatus {
                            name: PlatformTelemetrySignalName::Power,
                            gate: PlatformTelemetryGate::ContextOnly,
                            not_applicable_reason: None,
                            outcome: Some(PlatformSignalOutcomeStatus::Collected),
                        },
                        PlatformSignalTelemetryStatus {
                            name: PlatformTelemetrySignalName::Throttle,
                            gate: PlatformTelemetryGate::ContextOnly,
                            not_applicable_reason: None,
                            outcome: Some(PlatformSignalOutcomeStatus::Collected),
                        },
                        PlatformSignalTelemetryStatus {
                            name: PlatformTelemetrySignalName::Memory,
                            gate: PlatformTelemetryGate::LoadBearing,
                            not_applicable_reason: None,
                            outcome: Some(PlatformSignalOutcomeStatus::Collected),
                        },
                        PlatformSignalTelemetryStatus {
                            name: PlatformTelemetrySignalName::GpuUtilization,
                            gate: PlatformTelemetryGate::ContextOnly,
                            not_applicable_reason: None,
                            outcome: Some(PlatformSignalOutcomeStatus::Collected),
                        },
                    ],
                    degraded_reason: Some("telemetry_degraded".into()),
                    deployment_degraded: false,
                }),
            }),
            error: None,
        };
        let raw = serde_json::to_string(&resp).expect("serialize");
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../schemas/agent_control.json"))
                .expect("parse schema");
        let validator = jsonschema::JSONSchema::compile(&schema).expect("schema compiles");
        let wire: serde_json::Value = serde_json::from_str(&raw).expect("wire JSON");
        assert!(
            validator.is_valid(&wire),
            "generated platform telemetry must satisfy the control schema: {wire}"
        );
        let back: ControlResponse = decode_with_version_check(&raw).expect("validate response");
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
            platform_telemetry: None,
        };
        let raw = serde_json::to_string(&status).expect("serialize");
        let back: AgentStatus = serde_json::from_str(&raw).expect("deserialize");
        assert_eq!(status, back);
    }
}
