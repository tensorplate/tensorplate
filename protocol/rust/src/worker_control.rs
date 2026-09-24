// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F05: Rust mirror of `protocol/schemas/worker_control.json`.
//
// Two families of operations share one envelope. The legacy stages
// (`prepare` .. `unload`) describe the agent's in-process worker trait and
// are never sent over the runtime control channel. The runtime operations
// (`admission_fence` .. `pressure_directive`) are the messages the agent and
// the serving worker exchange over that channel: one compact JSON frame per
// line, each request answered by one response with the same
// `correlation_id`. A request the agent repeats keeps its
// `correlation_id`, and the worker applies each one at most once, answering
// a repeat with the first outcome; that is what makes every runtime
// operation idempotent. The golden frames under
// `protocol/rust/tests/fixtures/worker_control_*.jsonl` pin the byte-exact
// encoding every implementation of the channel produces.

use serde::{Deserialize, Serialize};

use crate::agent_control::is_valid_deployment_id;
use crate::correlation_id::validate_correlation_id;
use crate::error::ErrorCode;
use crate::member_quota::MemberQuota;
use crate::resident_set::MAX_STATE_COUNTER;
use crate::serde_shape::{deserialize_some, deserialize_some_map_only};
use crate::{DecodeError, ValidatePayload, SCHEMA_VERSION};

/// Largest frame on the runtime control channel, including its newline.
/// A reader treats a longer line as a protocol error and closes the
/// channel; [`encode_frame`] refuses to write one.
pub const WORKER_CONTROL_MAX_FRAME_BYTES: usize = 65_536;

/// Most sessions a member's ledger reports, and so the largest session
/// ceiling a quota may assign over the channel. The largest legal ledger
/// frame (this many twenty-digit timestamps plus the envelope) stays under
/// [`WORKER_CONTROL_MAX_FRAME_BYTES`].
pub const WORKER_CONTROL_MAX_LEDGER_SESSIONS: u32 = 2_048;

/// `transaction_id` of the runtime operations that belong to no deploy
/// transaction: `quota_assign`, `ledger_status` and `pressure_directive`.
pub const RUNTIME_TRANSACTION_ID: &str = "runtime";

/// Worker control operation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerOp {
    // Legacy stages of the in-process trait; never sent over the channel.
    Prepare,
    CapacityCheck,
    Warm,
    Promote,
    ActiveStatus,
    Unload,
    // Runtime operations.
    /// Stop the member admitting new sessions; acknowledged before the
    /// agent durably commits a set revision.
    AdmissionFence,
    /// Open admission under the member's assigned quota.
    Activate,
    /// Start the member's retirement drain.
    Retire,
    /// Set the member's session quota.
    QuotaAssign,
    /// Report the member's session ledger.
    LedgerStatus,
    /// Apply a memory pressure level.
    PressureDirective,
}

impl WorkerOp {
    /// Whether this operation travels over the runtime control channel.
    #[must_use]
    pub fn is_runtime(self) -> bool {
        match self {
            Self::Prepare
            | Self::CapacityCheck
            | Self::Warm
            | Self::Promote
            | Self::ActiveStatus
            | Self::Unload => false,
            Self::AdmissionFence
            | Self::Activate
            | Self::Retire
            | Self::QuotaAssign
            | Self::LedgerStatus
            | Self::PressureDirective => true,
        }
    }

    /// Whether this runtime operation belongs to a deploy transaction (its
    /// `transaction_id` names that transaction) rather than to none (its
    /// `transaction_id` is [`RUNTIME_TRANSACTION_ID`]).
    #[must_use]
    pub fn is_transactional(self) -> bool {
        matches!(self, Self::AdmissionFence | Self::Activate | Self::Retire)
    }
}

/// A resident-set member: one deployment at one generation.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberRef {
    pub deployment_id: String,
    pub generation: u64,
}

impl MemberRef {
    #[must_use]
    pub fn new(deployment_id: impl Into<String>, generation: u64) -> Self {
        Self {
            deployment_id: deployment_id.into(),
            generation,
        }
    }

    fn validate(&self) -> Result<(), WorkerControlRequestError> {
        if !is_valid_deployment_id(&self.deployment_id) {
            return Err(WorkerControlRequestError::InvalidMember(
                "deployment_id must be one filesystem-safe path segment of 1-128 bytes".into(),
            ));
        }
        if !(1..=MAX_STATE_COUNTER).contains(&self.generation) {
            return Err(WorkerControlRequestError::InvalidMember(format!(
                "generation {} is outside [1, 2^53)",
                self.generation
            )));
        }
        Ok(())
    }
}

/// Memory pressure level the agent directs a member to apply. Pressure
/// levels never lift an admission fence or undo a retirement.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PressureLevel {
    /// No pressure applies; changes nothing.
    Normal,
    /// Stop admitting new sessions and drop droppable diagnostics.
    ShedAdmission,
    /// Cancel the member's `count` newest admitted sessions.
    TerminateNewest,
    /// Lift `ShedAdmission`.
    Resume,
}

/// A pressure directive: the level, and for `terminate_newest` how many of
/// the member's newest admitted sessions to cancel.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PressureDirective {
    pub level: PressureLevel,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub count: Option<u32>,
}

impl PressureDirective {
    #[must_use]
    pub fn level(level: PressureLevel) -> Self {
        Self { level, count: None }
    }

    #[must_use]
    pub fn terminate_newest(count: u32) -> Self {
        Self {
            level: PressureLevel::TerminateNewest,
            count: Some(count),
        }
    }

    fn validate(self) -> Result<(), WorkerControlRequestError> {
        match (self.level, self.count) {
            (PressureLevel::TerminateNewest, Some(n))
                if (1..=WORKER_CONTROL_MAX_LEDGER_SESSIONS).contains(&n) =>
            {
                Ok(())
            }
            (PressureLevel::TerminateNewest, _) => Err(WorkerControlRequestError::InvalidPressure(
                "terminate_newest needs a count from 1 to 2048",
            )),
            (_, Some(_)) => Err(WorkerControlRequestError::InvalidPressure(
                "only terminate_newest carries a count",
            )),
            (_, None) => Ok(()),
        }
    }
}

/// A member's session ledger, as its worker reports it.
///
/// `reserved` sessions hold a quota reservation and are opening, `active`
/// sessions are admitted and running, `closing` sessions are ending but
/// hold their reservation until physical release; together they never
/// exceed `ceiling`. `admission_monotonic_ns` lists, oldest first, the host
/// monotonic-clock time at which each reserved or active session was
/// admitted, so the agent can order the newest sessions across members.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerStatus {
    pub reserved: u32,
    pub active: u32,
    pub closing: u32,
    pub ceiling: u32,
    pub admission_monotonic_ns: Vec<u64>,
}

impl LedgerStatus {
    fn validate(&self) -> Result<(), WorkerControlResponseError> {
        if self.ceiling > WORKER_CONTROL_MAX_LEDGER_SESSIONS {
            return Err(WorkerControlResponseError::InvalidLedger(format!(
                "ceiling {} exceeds the {WORKER_CONTROL_MAX_LEDGER_SESSIONS} sessions a ledger reports",
                self.ceiling
            )));
        }
        let held = u64::from(self.reserved) + u64::from(self.active) + u64::from(self.closing);
        if held > u64::from(self.ceiling) {
            return Err(WorkerControlResponseError::InvalidLedger(format!(
                "reserved + active + closing ({held}) exceeds ceiling ({})",
                self.ceiling
            )));
        }
        let admitted = u64::from(self.reserved) + u64::from(self.active);
        if self.admission_monotonic_ns.len() as u64 != admitted {
            return Err(WorkerControlResponseError::InvalidLedger(format!(
                "{} admission timestamps for {admitted} reserved or active sessions",
                self.admission_monotonic_ns.len()
            )));
        }
        if self
            .admission_monotonic_ns
            .windows(2)
            .any(|pair| pair[1] < pair[0])
        {
            return Err(WorkerControlResponseError::InvalidLedger(
                "admission timestamps must be oldest first".into(),
            ));
        }
        Ok(())
    }
}

/// Reference to a verified, staged candidate the agent asks the worker to
/// load and warm.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateRef {
    pub deployment_id: String,
    pub staged_path: String,
    pub bundle_digest: String,
    pub backend_hint: String,
    pub model_class: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub bundle_name: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub bundle_version: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub artifact_relative_path: Option<String>,
}

/// Outgoing agent -> worker request envelope.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerControlRequest {
    pub schema_version: String,
    pub transaction_id: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub correlation_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub candidate_deployment_id: Option<String>,
    pub op: WorkerOp,
    /// The member a runtime operation is for.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some_map_only"
    )]
    pub member: Option<MemberRef>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some_map_only"
    )]
    pub candidate: Option<CandidateRef>,
    /// `quota_assign` only.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some_map_only"
    )]
    pub quota: Option<MemberQuota>,
    /// `pressure_directive` only.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some_map_only"
    )]
    pub pressure: Option<PressureDirective>,
    /// `retire` only: how long existing sessions may finish before they
    /// are cancelled.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub drain_timeout_ms: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub timeout_ms: Option<u64>,
}

impl WorkerControlRequest {
    /// Build a legacy-stage request with the schema version populated.
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
            member: None,
            candidate,
            quota: None,
            pressure: None,
            drain_timeout_ms: None,
            timeout_ms,
        }
    }

    fn runtime(
        op: WorkerOp,
        transaction_id: impl Into<String>,
        correlation_id: impl Into<String>,
        member: MemberRef,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            transaction_id: transaction_id.into(),
            correlation_id: Some(correlation_id.into()),
            candidate_deployment_id: None,
            op,
            member: Some(member),
            candidate: None,
            quota: None,
            pressure: None,
            drain_timeout_ms: None,
            timeout_ms: None,
        }
    }

    /// `admission_fence` for `member`, inside deploy transaction
    /// `transaction_id`.
    #[must_use]
    pub fn admission_fence(
        transaction_id: impl Into<String>,
        correlation_id: impl Into<String>,
        member: MemberRef,
    ) -> Self {
        Self::runtime(
            WorkerOp::AdmissionFence,
            transaction_id,
            correlation_id,
            member,
        )
    }

    /// `activate` for `member`, inside deploy transaction `transaction_id`.
    #[must_use]
    pub fn activate(
        transaction_id: impl Into<String>,
        correlation_id: impl Into<String>,
        member: MemberRef,
    ) -> Self {
        Self::runtime(WorkerOp::Activate, transaction_id, correlation_id, member)
    }

    /// `retire` for `member`, inside deploy transaction `transaction_id`.
    #[must_use]
    pub fn retire(
        transaction_id: impl Into<String>,
        correlation_id: impl Into<String>,
        member: MemberRef,
        drain_timeout_ms: u64,
    ) -> Self {
        let mut request = Self::runtime(WorkerOp::Retire, transaction_id, correlation_id, member);
        request.drain_timeout_ms = Some(drain_timeout_ms);
        request
    }

    /// `quota_assign` for `member`.
    #[must_use]
    pub fn quota_assign(
        correlation_id: impl Into<String>,
        member: MemberRef,
        quota: MemberQuota,
    ) -> Self {
        let mut request = Self::runtime(
            WorkerOp::QuotaAssign,
            RUNTIME_TRANSACTION_ID,
            correlation_id,
            member,
        );
        request.quota = Some(quota);
        request
    }

    /// `ledger_status` for `member`.
    #[must_use]
    pub fn ledger_status(correlation_id: impl Into<String>, member: MemberRef) -> Self {
        Self::runtime(
            WorkerOp::LedgerStatus,
            RUNTIME_TRANSACTION_ID,
            correlation_id,
            member,
        )
    }

    /// `pressure_directive` for `member`.
    #[must_use]
    pub fn pressure_directive(
        correlation_id: impl Into<String>,
        member: MemberRef,
        pressure: PressureDirective,
    ) -> Self {
        let mut request = Self::runtime(
            WorkerOp::PressureDirective,
            RUNTIME_TRANSACTION_ID,
            correlation_id,
            member,
        );
        request.pressure = Some(pressure);
        request
    }

    /// Set the bounded operation timeout.
    #[must_use]
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }

    fn validate_runtime(&self) -> Result<(), WorkerControlRequestError> {
        let correlation_id = self
            .correlation_id
            .as_deref()
            .ok_or(WorkerControlRequestError::MissingCorrelationId(self.op))?;
        validate_correlation_id(correlation_id)
            .map_err(|e| WorkerControlRequestError::InvalidCorrelationId(e.to_string()))?;
        validate_correlation_id(&self.transaction_id)
            .map_err(|e| WorkerControlRequestError::InvalidTransactionId(e.to_string()))?;
        self.member
            .as_ref()
            .ok_or(WorkerControlRequestError::MissingMember(self.op))?
            .validate()?;
        if self.candidate.is_some() || self.candidate_deployment_id.is_some() {
            return Err(WorkerControlRequestError::CandidateOnRuntimeOp(self.op));
        }
        if self.op.is_transactional() == (self.transaction_id == RUNTIME_TRANSACTION_ID) {
            return Err(WorkerControlRequestError::WrongTransactionId(self.op));
        }
        let payloads = [
            ("quota", self.quota.is_some(), WorkerOp::QuotaAssign),
            (
                "pressure",
                self.pressure.is_some(),
                WorkerOp::PressureDirective,
            ),
            (
                "drain_timeout_ms",
                self.drain_timeout_ms.is_some(),
                WorkerOp::Retire,
            ),
        ];
        for (field, present, owner) in payloads {
            if present != (self.op == owner) {
                return Err(WorkerControlRequestError::PayloadMismatch { op: self.op, field });
            }
        }
        if let Some(quota) = self.quota.as_ref() {
            quota
                .validate()
                .map_err(|e| WorkerControlRequestError::InvalidQuota(e.to_string()))?;
            if quota.session_count > WORKER_CONTROL_MAX_LEDGER_SESSIONS {
                return Err(WorkerControlRequestError::InvalidQuota(format!(
                    "session_count {} exceeds the {WORKER_CONTROL_MAX_LEDGER_SESSIONS} sessions a ledger reports",
                    quota.session_count
                )));
            }
        }
        if let Some(pressure) = self.pressure {
            pressure.validate()?;
        }
        if self.drain_timeout_ms == Some(0) {
            return Err(WorkerControlRequestError::ZeroDrainTimeout);
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum WorkerControlRequestError {
    #[error("worker control request transaction_id must be non-empty")]
    EmptyTransactionId,
    #[error("worker control op `{0:?}` requires a candidate payload")]
    MissingCandidate(WorkerOp),
    #[error("legacy worker control op `{0:?}` carries a runtime-operation field")]
    RuntimeFieldOnLegacyOp(WorkerOp),
    #[error("runtime op `{0:?}` requires a correlation_id")]
    MissingCorrelationId(WorkerOp),
    #[error("runtime op correlation_id is invalid: {0}")]
    InvalidCorrelationId(String),
    #[error("runtime op transaction_id is invalid: {0}")]
    InvalidTransactionId(String),
    #[error("timeout_ms must be at least 1")]
    ZeroTimeout,
    #[error("runtime op `{0:?}` requires a member")]
    MissingMember(WorkerOp),
    #[error("member is invalid: {0}")]
    InvalidMember(String),
    #[error("runtime op `{0:?}` must not carry a candidate")]
    CandidateOnRuntimeOp(WorkerOp),
    #[error("runtime op `{0:?}` has the wrong transaction_id: fence, activate and retire name their deploy transaction; quota_assign, ledger_status and pressure_directive use `runtime`")]
    WrongTransactionId(WorkerOp),
    #[error("`{field}` does not belong on op `{op:?}` (or is missing from it)")]
    PayloadMismatch { op: WorkerOp, field: &'static str },
    #[error("quota is invalid: {0}")]
    InvalidQuota(String),
    #[error("pressure directive is invalid: {0}")]
    InvalidPressure(&'static str),
    #[error("drain_timeout_ms must be at least 1")]
    ZeroDrainTimeout,
}

impl ValidatePayload for WorkerControlRequest {
    fn validate_payload(self) -> Result<Self, DecodeError> {
        let invalid = |err: WorkerControlRequestError| DecodeError::InvalidPayload(err.to_string());
        if self.transaction_id.is_empty() {
            return Err(invalid(WorkerControlRequestError::EmptyTransactionId));
        }
        if self.timeout_ms == Some(0) {
            return Err(invalid(WorkerControlRequestError::ZeroTimeout));
        }
        if self.op.is_runtime() {
            self.validate_runtime().map_err(invalid)?;
            return Ok(self);
        }
        if self.member.is_some()
            || self.quota.is_some()
            || self.pressure.is_some()
            || self.drain_timeout_ms.is_some()
        {
            return Err(invalid(WorkerControlRequestError::RuntimeFieldOnLegacyOp(
                self.op,
            )));
        }
        match self.op {
            WorkerOp::Prepare | WorkerOp::CapacityCheck | WorkerOp::Warm | WorkerOp::Promote => {
                if self.candidate.is_none() {
                    return Err(invalid(WorkerControlRequestError::MissingCandidate(
                        self.op,
                    )));
                }
            }
            _ => {}
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
    /// A runtime request named a member this worker does not serve; the
    /// response's `member` names the one it does.
    MemberMismatch,
}

/// Typed error attached to a worker response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
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
#[serde(deny_unknown_fields)]
pub struct WorkerControlResponse {
    pub schema_version: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub transaction_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub correlation_id: Option<String>,
    /// The runtime operation this response answers; absent on legacy
    /// responses.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub op: Option<WorkerOp>,
    /// The member that answered a runtime operation.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some_map_only"
    )]
    pub member: Option<MemberRef>,
    pub status: WorkerStatusOutcome,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub active_deployment_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub candidate_deployment_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub ready: Option<bool>,
    /// `ledger_status` answered `ok`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some_map_only"
    )]
    pub ledger: Option<LedgerStatus>,
    /// The quota in force after an `activate` or `quota_assign` answered
    /// `ok`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some_map_only"
    )]
    pub quota: Option<MemberQuota>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some_map_only"
    )]
    pub error: Option<WorkerError>,
}

impl WorkerControlResponse {
    fn legacy(transaction_id: impl Into<String>, status: WorkerStatusOutcome) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            transaction_id: Some(transaction_id.into()),
            correlation_id: None,
            op: None,
            member: None,
            status,
            active_deployment_id: None,
            candidate_deployment_id: None,
            ready: None,
            ledger: None,
            quota: None,
            error: None,
        }
    }

    #[must_use]
    pub fn ok(transaction_id: impl Into<String>) -> Self {
        Self::legacy(transaction_id, WorkerStatusOutcome::Ok)
    }

    #[must_use]
    pub fn ready(transaction_id: impl Into<String>) -> Self {
        let mut r = Self::ok(transaction_id);
        r.ready = Some(true);
        r
    }

    #[must_use]
    pub fn not_ready(transaction_id: impl Into<String>) -> Self {
        let mut r = Self::legacy(transaction_id, WorkerStatusOutcome::NotReady);
        r.ready = Some(false);
        r
    }

    #[must_use]
    pub fn error(transaction_id: impl Into<String>, error: WorkerError) -> Self {
        let mut r = Self::legacy(transaction_id, WorkerStatusOutcome::Error);
        r.error = Some(error);
        r
    }

    #[must_use]
    pub fn timeout(transaction_id: impl Into<String>, message: impl Into<String>) -> Self {
        let mut r = Self::legacy(transaction_id, WorkerStatusOutcome::Timeout);
        r.error = Some(WorkerError::new(ErrorCode::Timeout, message));
        r
    }

    /// The answer to runtime `request` from the worker serving `responder`,
    /// echoing the request's transaction and correlation ids and operation.
    #[must_use]
    pub fn answer(
        request: &WorkerControlRequest,
        responder: MemberRef,
        status: WorkerStatusOutcome,
    ) -> Self {
        Self {
            correlation_id: request.correlation_id.clone(),
            op: Some(request.op),
            member: Some(responder),
            ..Self::legacy(request.transaction_id.clone(), status)
        }
    }

    /// Attach a `ledger_status` ledger.
    #[must_use]
    pub fn with_ledger(mut self, ledger: LedgerStatus) -> Self {
        self.ledger = Some(ledger);
        self
    }

    /// Attach the quota in force after `activate` or `quota_assign`.
    #[must_use]
    pub fn with_quota(mut self, quota: MemberQuota) -> Self {
        self.quota = Some(quota);
        self
    }

    /// Attach a typed error to a failed answer.
    #[must_use]
    pub fn with_error(mut self, error: WorkerError) -> Self {
        self.error = Some(error);
        self
    }

    /// Check that this response answers runtime `request`: the same
    /// operation, transaction and correlation ids, and the member asked for
    /// exactly when the status is not `member_mismatch`.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerControlResponseError::NotAnAnswer`] naming the first
    /// mismatch.
    pub fn answers(
        &self,
        request: &WorkerControlRequest,
    ) -> Result<(), WorkerControlResponseError> {
        let not = WorkerControlResponseError::NotAnAnswer;
        if !request.op.is_runtime() || self.op != Some(request.op) {
            return Err(not("operation differs"));
        }
        if self.transaction_id.as_deref() != Some(request.transaction_id.as_str()) {
            return Err(not("transaction_id differs"));
        }
        if self.correlation_id != request.correlation_id {
            return Err(not("correlation_id differs"));
        }
        let same_member = self.member.is_some() && self.member == request.member;
        match (
            self.status == WorkerStatusOutcome::MemberMismatch,
            same_member,
        ) {
            (false, false) => Err(not("another member answered")),
            (true, true) => Err(not("member_mismatch from the member asked")),
            _ => Ok(()),
        }
    }

    fn validate(&self) -> Result<(), WorkerControlResponseError> {
        if let Some(op) = self.op {
            if !op.is_runtime() {
                return Err(WorkerControlResponseError::LegacyOp(op));
            }
            let transaction_id = self
                .transaction_id
                .as_deref()
                .ok_or(WorkerControlResponseError::MissingEcho("transaction_id"))?;
            validate_correlation_id(transaction_id).map_err(|e| {
                WorkerControlResponseError::InvalidEcho(format!("transaction_id: {e}"))
            })?;
            if op.is_transactional() == (transaction_id == RUNTIME_TRANSACTION_ID) {
                return Err(WorkerControlResponseError::WrongTransactionId(op));
            }
            let correlation_id = self
                .correlation_id
                .as_deref()
                .ok_or(WorkerControlResponseError::MissingEcho("correlation_id"))?;
            validate_correlation_id(correlation_id).map_err(|e| {
                WorkerControlResponseError::InvalidEcho(format!("correlation_id: {e}"))
            })?;
            self.member
                .as_ref()
                .ok_or(WorkerControlResponseError::MissingMember)?
                .validate()
                .map_err(|e| WorkerControlResponseError::InvalidMember(e.to_string()))?;
        } else if self.member.is_some()
            || self.ledger.is_some()
            || self.quota.is_some()
            || self.status == WorkerStatusOutcome::MemberMismatch
        {
            return Err(WorkerControlResponseError::RuntimeFieldWithoutOp);
        }
        let ok = self.status == WorkerStatusOutcome::Ok;
        let ledger_op = self.op == Some(WorkerOp::LedgerStatus);
        match (&self.ledger, ledger_op && ok) {
            (None, true) => return Err(WorkerControlResponseError::MissingLedger),
            (Some(_), false) => return Err(WorkerControlResponseError::MisplacedField("ledger")),
            (Some(ledger), true) => ledger.validate()?,
            (None, false) => {}
        }
        let quota_op = matches!(self.op, Some(WorkerOp::Activate | WorkerOp::QuotaAssign));
        if quota_op && ok && self.quota.is_none() {
            return Err(WorkerControlResponseError::MissingQuota);
        }
        if let Some(quota) = self.quota.as_ref() {
            if !(quota_op && ok) {
                return Err(WorkerControlResponseError::MisplacedField("quota"));
            }
            quota
                .validate()
                .map_err(|e| WorkerControlResponseError::InvalidQuota(e.to_string()))?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum WorkerControlResponseError {
    #[error("a response may name only a runtime op, not `{0:?}`")]
    LegacyOp(WorkerOp),
    #[error("a runtime response must echo `{0}`")]
    MissingEcho(&'static str),
    #[error("a runtime response echo is invalid: {0}")]
    InvalidEcho(String),
    #[error("a runtime response must name the member that answered")]
    MissingMember,
    #[error("the answering member is invalid: {0}")]
    InvalidMember(String),
    #[error("an ok activate or quota_assign response must carry the quota in force")]
    MissingQuota,
    #[error("the response does not answer the request: {0}")]
    NotAnAnswer(&'static str),
    #[error("runtime response `{0:?}` has the wrong transaction_id")]
    WrongTransactionId(WorkerOp),
    #[error("member, ledger, quota and member_mismatch belong only on a runtime response")]
    RuntimeFieldWithoutOp,
    #[error("an ok ledger_status response must carry the ledger")]
    MissingLedger,
    #[error("`{0}` does not belong on this response")]
    MisplacedField(&'static str),
    #[error("ledger is invalid: {0}")]
    InvalidLedger(String),
    #[error("quota is invalid: {0}")]
    InvalidQuota(String),
}

impl ValidatePayload for WorkerControlResponse {
    fn validate_payload(self) -> Result<Self, DecodeError> {
        self.validate()
            .map_err(|e| DecodeError::InvalidPayload(e.to_string()))?;
        Ok(self)
    }
}

/// Errors from [`encode_frame`].
#[derive(Debug, thiserror::Error)]
pub enum WorkerControlFrameError {
    #[error("serialize worker control frame: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error(
        "worker control frame is {0} bytes, over the {WORKER_CONTROL_MAX_FRAME_BYTES}-byte limit"
    )]
    TooLarge(usize),
}

/// Encode `message` as one runtime control channel frame: compact JSON
/// followed by a newline, at most [`WORKER_CONTROL_MAX_FRAME_BYTES`].
///
/// # Errors
///
/// Returns [`WorkerControlFrameError`] when serialization fails or the
/// frame would exceed the limit.
pub fn encode_frame<T: Serialize>(message: &T) -> Result<Vec<u8>, WorkerControlFrameError> {
    let mut frame = serde_json::to_vec(message)?;
    frame.push(b'\n');
    if frame.len() > WORKER_CONTROL_MAX_FRAME_BYTES {
        return Err(WorkerControlFrameError::TooLarge(frame.len()));
    }
    Ok(frame)
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
