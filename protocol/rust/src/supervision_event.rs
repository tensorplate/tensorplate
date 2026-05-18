// SPDX-License-Identifier: Apache-2.0
//
// V01-E09-F05-T01: Rust mirror of `protocol/schemas/supervision_event.json`.
//
// Supervision events are emitted by the agent worker supervisor at every
// state transition and consumed by the V01-E10 observability service plus
// V01-E11 status/log views. The wire schema is union-stable (additions
// append); the C++ runtime does not produce these events.

use serde::{Deserialize, Serialize};

use crate::error::ErrorCode;
use crate::{DecodeError, ValidatePayload, SCHEMA_VERSION};

/// Discrete supervision-event kind. Post-v0.1.0 additions append; existing
/// names never change meaning.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisionEventKind {
    WorkerStarted,
    WorkerReady,
    WorkerExit,
    WorkerNotReady,
    RestartScheduled,
    WorkerDegraded,
    WorkerFailed,
    CrashLoopEntered,
    WorkerStopping,
    WorkerStopped,
}

/// Coarse agent run-state shared with `agent_control` (kept duplicated here
/// so observability consumers can decode supervision events without pulling
/// in the entire `AgentStatus` schema).
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisionAgentState {
    Ready,
    Degraded,
    Failed,
    #[default]
    Unknown,
}

/// Stable supervision-state name shared with the V01-E10 observability
/// consumer and V01-E11 status command. Matches `SupervisionPhase` in
/// `tensorplate_agent::supervision`.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisionServingState {
    #[default]
    NoActiveDeployment,
    Starting,
    Running,
    Ready,
    Degraded,
    Failed,
    Stopping,
    Stopped,
    AwaitingRestart,
    CrashLoop,
}

/// Mirror of `protocol/schemas/supervision_event.json`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SupervisionEvent {
    pub schema_version: String,
    pub kind: SupervisionEventKind,
    pub sequence: u64,
    pub monotonic_timestamp_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_state: Option<SupervisionAgentState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serving_state: Option<SupervisionServingState>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub active_deployment: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub backend: String,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub restart_count: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub missed_heartbeat_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_restart_delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_signal: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_ready: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<ErrorCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Soft cap on the `message` field. Producers truncate; consumers may reject
/// values larger than this through their own bounded buffers, but this
/// crate does not enforce a hard upper bound to keep the schema permissive
/// for diagnostic strings observed in the wild.
pub const MAX_SUPERVISION_MESSAGE_BYTES: usize = 512;

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

impl SupervisionEvent {
    /// Build a supervision event with the schema version populated and the
    /// payload trimmed to the supervisor's diagnostic envelope. Producers
    /// should construct via this constructor; the durable schema field is
    /// fixed to [`SCHEMA_VERSION`].
    #[must_use]
    pub fn new(kind: SupervisionEventKind, sequence: u64, monotonic_timestamp_ns: u64) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            kind,
            sequence,
            monotonic_timestamp_ns,
            agent_state: None,
            serving_state: None,
            active_deployment: String::new(),
            backend: String::new(),
            restart_count: 0,
            missed_heartbeat_count: 0,
            next_restart_delay_ms: None,
            exit_code: None,
            exit_signal: None,
            after_ready: None,
            error_code: None,
            message: None,
        }
    }

    /// Truncate `message` to [`MAX_SUPERVISION_MESSAGE_BYTES`] UTF-8 bytes.
    /// Producers call this at the boundary so a misbehaving exit string
    /// cannot grow the on-disk supervision-event log without bound.
    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        let mut text = message.into();
        if text.len() > MAX_SUPERVISION_MESSAGE_BYTES {
            // Slice on the nearest UTF-8 boundary at or below the cap. This
            // mirrors `String::truncate` semantics without panicking on
            // multi-byte characters.
            let mut end = MAX_SUPERVISION_MESSAGE_BYTES;
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        self.message = Some(text);
        self
    }
}

impl ValidatePayload for SupervisionEvent {
    fn validate_payload(self) -> Result<Self, DecodeError> {
        if let Some(ref m) = self.message {
            if m.len() > MAX_SUPERVISION_MESSAGE_BYTES * 4 {
                return Err(DecodeError::InvalidPayload(format!(
                    "SupervisionEvent.message must be <= {} bytes",
                    MAX_SUPERVISION_MESSAGE_BYTES * 4
                )));
            }
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{
        SupervisionAgentState, SupervisionEvent, SupervisionEventKind, SupervisionServingState,
        MAX_SUPERVISION_MESSAGE_BYTES, SCHEMA_VERSION,
    };
    use crate::decode_with_version_check;
    use crate::error::ErrorCode;

    #[test]
    fn worker_started_round_trips() {
        let e = SupervisionEvent::new(SupervisionEventKind::WorkerStarted, 1, 42);
        let json = serde_json::to_string(&e).expect("serialize");
        let back: SupervisionEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(e, back);
        assert_eq!(back.kind, SupervisionEventKind::WorkerStarted);
        assert_eq!(back.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn restart_scheduled_with_delay_round_trips() {
        let mut e = SupervisionEvent::new(SupervisionEventKind::RestartScheduled, 5, 1_000_000);
        e.serving_state = Some(SupervisionServingState::AwaitingRestart);
        e.agent_state = Some(SupervisionAgentState::Degraded);
        e.restart_count = 2;
        e.next_restart_delay_ms = Some(8_000);
        e.error_code = Some(ErrorCode::NotReady);
        e = e.with_message("warm timed out at 30s");
        let json = serde_json::to_string(&e).expect("serialize");
        let back: SupervisionEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(e, back);
        assert_eq!(back.next_restart_delay_ms, Some(8_000));
    }

    #[test]
    fn with_message_truncates_long_strings_at_utf8_boundary() {
        let long = "x".repeat(MAX_SUPERVISION_MESSAGE_BYTES * 2);
        let e = SupervisionEvent::new(SupervisionEventKind::WorkerExit, 0, 0).with_message(long);
        assert_eq!(
            e.message.expect("message").len(),
            MAX_SUPERVISION_MESSAGE_BYTES
        );
    }

    #[test]
    fn known_kinds_serialize_as_snake_case() {
        for (k, name) in [
            (SupervisionEventKind::WorkerStarted, "worker_started"),
            (SupervisionEventKind::WorkerReady, "worker_ready"),
            (SupervisionEventKind::WorkerExit, "worker_exit"),
            (SupervisionEventKind::WorkerNotReady, "worker_not_ready"),
            (SupervisionEventKind::RestartScheduled, "restart_scheduled"),
            (SupervisionEventKind::WorkerDegraded, "worker_degraded"),
            (SupervisionEventKind::WorkerFailed, "worker_failed"),
            (SupervisionEventKind::CrashLoopEntered, "crash_loop_entered"),
            (SupervisionEventKind::WorkerStopping, "worker_stopping"),
            (SupervisionEventKind::WorkerStopped, "worker_stopped"),
        ] {
            let json = serde_json::to_string(&k).expect("serialize");
            assert_eq!(json, format!("\"{name}\""));
        }
    }

    #[test]
    fn known_serving_states_serialize_as_snake_case() {
        for (s, name) in [
            (
                SupervisionServingState::NoActiveDeployment,
                "no_active_deployment",
            ),
            (SupervisionServingState::Starting, "starting"),
            (SupervisionServingState::Running, "running"),
            (SupervisionServingState::Ready, "ready"),
            (SupervisionServingState::Degraded, "degraded"),
            (SupervisionServingState::Failed, "failed"),
            (SupervisionServingState::Stopping, "stopping"),
            (SupervisionServingState::Stopped, "stopped"),
            (SupervisionServingState::AwaitingRestart, "awaiting_restart"),
            (SupervisionServingState::CrashLoop, "crash_loop"),
        ] {
            let json = serde_json::to_string(&s).expect("serialize");
            assert_eq!(json, format!("\"{name}\""));
        }
    }

    #[test]
    fn version_check_decoder_rejects_old_schema() {
        let json = r#"{"schema_version":"99.99","kind":"worker_started","sequence":0,"monotonic_timestamp_ns":0}"#;
        let err = decode_with_version_check::<SupervisionEvent>(json).expect_err("rejected");
        assert!(matches!(
            err,
            crate::DecodeError::UnsupportedSchemaVersion { .. }
        ));
    }
}
