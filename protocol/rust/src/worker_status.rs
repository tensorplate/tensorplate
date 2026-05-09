// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F07-T03: Rust mirror of `protocol/schemas/worker_status.json`.

use serde::{Deserialize, Serialize};

use crate::error::ErrorCode;
use crate::SCHEMA_VERSION;

/// Coarse component state shared by `agent_state`, `serving_state`, and
/// `observability_state`. The values map directly to the V01-E10 ROS 2
/// health-topic level (`ready` -> OK, `degraded` -> WARN, `failed` ->
/// ERROR, `unknown` -> STALE).
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentState {
    Ready,
    Degraded,
    Failed,
    #[default]
    Unknown,
}

/// Mirror of `protocol/schemas/worker_status.json`. Carries the exact
/// fields the V01-E10 ROS 2 health publisher requires.
///
/// Only `PartialEq` is derived because `missed_deadline_rate` is `f64`
/// (NaN-unsafe under `Eq`). Equality compares structurally; tests that
/// compare statuses use `assert_eq!`, which only requires `PartialEq`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkerStatus {
    pub schema_version: String,
    pub agent_state: ComponentState,
    pub serving_state: ComponentState,
    pub observability_state: ComponentState,

    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub active_deployment: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub backend: String,

    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub missed_heartbeat_count: u64,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub missed_deadline_rate: f64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub queue_depth: u64,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_code: Option<ErrorCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_message: Option<String>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

#[allow(clippy::trivially_copy_pass_by_ref, clippy::float_cmp)]
fn is_zero_f64(v: &f64) -> bool {
    *v == 0.0
}

/// Validation errors raised by [`WorkerStatus::new`].
#[derive(Debug, thiserror::Error)]
pub enum WorkerStatusError {
    #[error("WorkerStatus.missed_deadline_rate must be in [0, 1]")]
    MissedDeadlineRateOutOfRange,
}

impl WorkerStatus {
    /// Build and validate a [`WorkerStatus`].
    ///
    /// # Errors
    ///
    /// See [`WorkerStatusError`].
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        agent_state: ComponentState,
        serving_state: ComponentState,
        observability_state: ComponentState,
        active_deployment: impl Into<String>,
        backend: impl Into<String>,
        missed_heartbeat_count: u64,
        missed_deadline_rate: f64,
        queue_depth: u64,
        last_error: Option<(ErrorCode, String)>,
    ) -> Result<Self, WorkerStatusError> {
        if !(0.0..=1.0).contains(&missed_deadline_rate) || missed_deadline_rate.is_nan() {
            return Err(WorkerStatusError::MissedDeadlineRateOutOfRange);
        }
        let (last_error_code, last_error_message) = match last_error {
            Some((c, m)) => (Some(c), Some(m)),
            None => (None, None),
        };
        Ok(Self {
            schema_version: SCHEMA_VERSION.to_string(),
            agent_state,
            serving_state,
            observability_state,
            active_deployment: active_deployment.into(),
            backend: backend.into(),
            missed_heartbeat_count,
            missed_deadline_rate,
            queue_depth,
            last_error_code,
            last_error_message,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{ComponentState, WorkerStatus, WorkerStatusError, SCHEMA_VERSION};
    use crate::decode_with_version_check;
    use crate::error::ErrorCode;

    #[test]
    fn ready_status_round_trips() {
        let s = WorkerStatus::new(
            ComponentState::Ready,
            ComponentState::Ready,
            ComponentState::Ready,
            "deploy-1",
            "tensorrt",
            0,
            0.0,
            0,
            None,
        )
        .expect("valid");
        let json = serde_json::to_string(&s).expect("serialize");
        let back: WorkerStatus = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(s, back);
        assert_eq!(back.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn degraded_status_with_last_error_round_trips() {
        let s = WorkerStatus::new(
            ComponentState::Ready,
            ComponentState::Degraded,
            ComponentState::Ready,
            "deploy-1",
            "python_pytorch",
            3,
            0.05,
            7,
            Some((ErrorCode::Timeout, "deadline exceeded for chunk-12".into())),
        )
        .expect("valid");
        let json = serde_json::to_string(&s).expect("serialize");
        let back: WorkerStatus = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(s, back);
        assert_eq!(back.last_error_code, Some(ErrorCode::Timeout));
    }

    #[test]
    fn rejects_out_of_range_missed_deadline_rate() {
        for bad in [-0.01, 1.01, f64::NAN] {
            let r = WorkerStatus::new(
                ComponentState::Ready,
                ComponentState::Ready,
                ComponentState::Ready,
                "",
                "",
                0,
                bad,
                0,
                None,
            );
            assert!(
                matches!(r, Err(WorkerStatusError::MissedDeadlineRateOutOfRange)),
                "expected rejection for {bad}"
            );
        }
    }

    #[test]
    fn version_check_decoder_rejects_old_schema() {
        let json = r#"{"schema_version":"99.99","agent_state":"ready","serving_state":"ready","observability_state":"ready"}"#;
        let err = decode_with_version_check::<WorkerStatus>(json).expect_err("rejected");
        assert!(matches!(
            err,
            crate::DecodeError::UnsupportedSchemaVersion { .. }
        ));
    }
}
