// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F07-T04: Rust mirror of `protocol/schemas/health_event.json`.

use serde::{Deserialize, Serialize};

use crate::error::ErrorCode;
use crate::SCHEMA_VERSION;

/// Discrete health-event kind. The set is union-stable: post-v0.1.0
/// additions append rather than rename.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthEventKind {
    Heartbeat,
    Ready,
    Degraded,
    Failed,
    NoHeartbeat,
    MissedDeadline,
    Overload,
}

/// Reserved control-loop telemetry fields populated during VLA validation
/// runs (V01-E12). Expressed as plain f64 so JSON round-tripping is
/// lossless.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ControlLoopMetrics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jitter_p50_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jitter_p95_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jitter_p99_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jitter_max_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mean_frequency_hz: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_stddev_hz: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_error_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolling_window_seconds: Option<u32>,
}

impl Eq for ControlLoopMetrics {}

impl ControlLoopMetrics {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// Mirror of `protocol/schemas/health_event.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HealthEvent {
    pub schema_version: String,
    pub kind: HealthEventKind,
    /// Receiver-monotonic timestamp in nanoseconds. Sampled from
    /// `std::time::Instant` (Rust) or `std::chrono::steady_clock`
    /// (C++). NEVER wall-clock.
    pub monotonic_timestamp_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(default, skip_serializing_if = "ControlLoopMetrics::is_empty")]
    pub control_loop_metrics: ControlLoopMetrics,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<ErrorCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl Eq for HealthEvent {}

impl HealthEvent {
    /// Build a heartbeat event at the given monotonic timestamp.
    #[must_use]
    pub fn heartbeat(monotonic_timestamp_ns: u64) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            kind: HealthEventKind::Heartbeat,
            monotonic_timestamp_ns,
            correlation_id: None,
            control_loop_metrics: ControlLoopMetrics::default(),
            error_code: None,
            message: None,
        }
    }

    /// Build a ready / degraded / failed state-transition event.
    #[must_use]
    pub fn state(kind: HealthEventKind, monotonic_timestamp_ns: u64) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            kind,
            monotonic_timestamp_ns,
            correlation_id: None,
            control_loop_metrics: ControlLoopMetrics::default(),
            error_code: None,
            message: None,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{ControlLoopMetrics, HealthEvent, HealthEventKind, SCHEMA_VERSION};
    use crate::decode_with_version_check;
    use crate::error::ErrorCode;

    #[test]
    fn heartbeat_round_trips() {
        let e = HealthEvent::heartbeat(1_700_000_000_000);
        let json = serde_json::to_string(&e).expect("serialize");
        let back: HealthEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(e, back);
        assert_eq!(back.kind, HealthEventKind::Heartbeat);
    }

    #[test]
    fn missed_deadline_with_full_control_loop_metrics_round_trips() {
        let metrics = ControlLoopMetrics {
            jitter_p50_ms: Some(1.2),
            jitter_p95_ms: Some(3.4),
            jitter_p99_ms: Some(5.6),
            jitter_max_ms: Some(8.0),
            mean_frequency_hz: Some(30.0),
            frequency_stddev_hz: Some(0.6),
            frequency_error_pct: Some(2.0),
            rolling_window_seconds: Some(60),
        };
        let e = HealthEvent {
            schema_version: SCHEMA_VERSION.to_string(),
            kind: HealthEventKind::MissedDeadline,
            monotonic_timestamp_ns: 9_999,
            correlation_id: Some("req-3".into()),
            control_loop_metrics: metrics,
            error_code: Some(ErrorCode::Timeout),
            message: Some("stage missed".into()),
        };
        let json = serde_json::to_string(&e).expect("serialize");
        let back: HealthEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(e, back);
        assert_eq!(back.control_loop_metrics, metrics);
    }

    #[test]
    fn known_event_kinds_serialize_as_snake_case() {
        for (k, name) in [
            (HealthEventKind::Heartbeat, "heartbeat"),
            (HealthEventKind::Ready, "ready"),
            (HealthEventKind::Degraded, "degraded"),
            (HealthEventKind::Failed, "failed"),
            (HealthEventKind::NoHeartbeat, "no_heartbeat"),
            (HealthEventKind::MissedDeadline, "missed_deadline"),
            (HealthEventKind::Overload, "overload"),
        ] {
            let json = serde_json::to_string(&k).expect("serialize");
            assert_eq!(json, format!("\"{name}\""));
        }
    }

    #[test]
    fn version_check_decoder_rejects_old_schema() {
        let json = r#"{"schema_version":"0.0","kind":"heartbeat","monotonic_timestamp_ns":1}"#;
        let err = decode_with_version_check::<HealthEvent>(json).expect_err("rejected");
        assert!(matches!(
            err,
            crate::DecodeError::UnsupportedSchemaVersion { .. }
        ));
    }
}
