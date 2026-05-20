// SPDX-License-Identifier: Apache-2.0
//
// V01-E12-F05: Rust mirror of `protocol/schemas/control_loop_metrics.json`.
//
// `ControlLoopEvent` carries the rolling 60s window summary used by the
// V01-E15 SmolVLA validation harness and the V01-E11 CLI status
// projection. Formulas are pinned to the v0.1 roadmap; the aggregator
// lives in `tensorplate-observability` (see `observability::control_loop`).

use serde::{Deserialize, Serialize};

use crate::correlation_id::validate_correlation_id;
use crate::{DecodeError, ValidatePayload, SCHEMA_VERSION};

/// Maximum bytes for any single label value. Mirrors the JSON schema.
pub const MAX_CONTROL_LOOP_LABEL_BYTES: usize = 64;

/// Bounded label set (V01-E12-F05). Only the four labels below are
/// allowed; producers must drop or normalise out-of-policy values
/// before emission.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ControlLoopLabels {
    pub endpoint: String,
    pub model_class: String,
    pub model_name: String,
    pub backend: String,
}

impl ControlLoopLabels {
    /// Construct, truncating each value to [`MAX_CONTROL_LOOP_LABEL_BYTES`].
    #[must_use]
    pub fn new(
        endpoint: impl Into<String>,
        model_class: impl Into<String>,
        model_name: impl Into<String>,
        backend: impl Into<String>,
    ) -> Self {
        Self {
            endpoint: truncate_label(endpoint.into()),
            model_class: truncate_label(model_class.into()),
            model_name: truncate_label(model_name.into()),
            backend: truncate_label(backend.into()),
        }
    }
}

fn truncate_label(mut s: String) -> String {
    if s.len() <= MAX_CONTROL_LOOP_LABEL_BYTES {
        return s;
    }
    let mut cut = MAX_CONTROL_LOOP_LABEL_BYTES;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s
}

/// Rolling-window aggregate summary. Fields default to `None` so a
/// window with too few samples still produces a valid event (the
/// consumer treats missing fields as `unavailable`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ControlLoopSummary {
    pub samples: u64,
    pub missed_deadlines: u64,
    pub missed_deadline_rate: f64,
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
}

impl Eq for ControlLoopSummary {}

/// Wire payload for `protocol/schemas/control_loop_metrics.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ControlLoopEvent {
    pub schema_version: String,
    pub monotonic_timestamp_ns: u64,
    #[serde(default = "default_window_seconds")]
    pub rolling_window_seconds: u32,
    pub control_frequency_hz: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_period_ms: Option<f64>,
    pub labels: ControlLoopLabels,
    pub summary: ControlLoopSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}

impl Eq for ControlLoopEvent {}

fn default_window_seconds() -> u32 {
    60
}

impl ControlLoopEvent {
    /// Build an event with the canonical `target_period_ms` derived
    /// from `control_frequency_hz`.
    #[must_use]
    pub fn new(
        monotonic_timestamp_ns: u64,
        control_frequency_hz: f64,
        labels: ControlLoopLabels,
        summary: ControlLoopSummary,
    ) -> Self {
        let target_period_ms = if control_frequency_hz > 0.0 {
            Some(1000.0 / control_frequency_hz)
        } else {
            None
        };
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            monotonic_timestamp_ns,
            rolling_window_seconds: default_window_seconds(),
            control_frequency_hz,
            target_period_ms,
            labels,
            summary,
            correlation_id: None,
        }
    }
}

fn validate_non_negative(name: &str, value: Option<f64>) -> Result<(), DecodeError> {
    if matches!(value, Some(v) if v < 0.0 || !v.is_finite()) {
        return Err(DecodeError::InvalidPayload(format!(
            "ControlLoopSummary.{name} must be finite and >= 0"
        )));
    }
    Ok(())
}

impl ValidatePayload for ControlLoopEvent {
    fn validate_payload(self) -> Result<Self, DecodeError> {
        if !self.control_frequency_hz.is_finite() || self.control_frequency_hz <= 0.0 {
            return Err(DecodeError::InvalidPayload(
                "control_frequency_hz must be finite and > 0".into(),
            ));
        }
        if self.rolling_window_seconds == 0 {
            return Err(DecodeError::InvalidPayload(
                "rolling_window_seconds must be >= 1".into(),
            ));
        }
        for label in [
            &self.labels.endpoint,
            &self.labels.model_class,
            &self.labels.model_name,
            &self.labels.backend,
        ] {
            if label.len() > MAX_CONTROL_LOOP_LABEL_BYTES {
                return Err(DecodeError::InvalidPayload(format!(
                    "ControlLoopLabels value exceeds {MAX_CONTROL_LOOP_LABEL_BYTES} bytes"
                )));
            }
        }
        if !(0.0..=1.0).contains(&self.summary.missed_deadline_rate)
            || self.summary.missed_deadline_rate.is_nan()
        {
            return Err(DecodeError::InvalidPayload(
                "summary.missed_deadline_rate must be in [0, 1]".into(),
            ));
        }
        if self.summary.missed_deadlines > self.summary.samples {
            return Err(DecodeError::InvalidPayload(
                "summary.missed_deadlines cannot exceed summary.samples".into(),
            ));
        }
        validate_non_negative("jitter_p50_ms", self.summary.jitter_p50_ms)?;
        validate_non_negative("jitter_p95_ms", self.summary.jitter_p95_ms)?;
        validate_non_negative("jitter_p99_ms", self.summary.jitter_p99_ms)?;
        validate_non_negative("jitter_max_ms", self.summary.jitter_max_ms)?;
        validate_non_negative("mean_frequency_hz", self.summary.mean_frequency_hz)?;
        validate_non_negative("frequency_stddev_hz", self.summary.frequency_stddev_hz)?;
        validate_non_negative("frequency_error_pct", self.summary.frequency_error_pct)?;
        if let Some(id) = &self.correlation_id {
            validate_correlation_id(id)?;
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{
        ControlLoopEvent, ControlLoopLabels, ControlLoopSummary, MAX_CONTROL_LOOP_LABEL_BYTES,
    };
    use crate::{decode_with_version_check, DecodeError, SCHEMA_VERSION};

    fn ok_event() -> ControlLoopEvent {
        ControlLoopEvent::new(
            1,
            30.0,
            ControlLoopLabels::new("/v1/act", "vla", "smolvla-tiny", "tensorrt"),
            ControlLoopSummary {
                samples: 120,
                missed_deadlines: 3,
                missed_deadline_rate: 0.025,
                jitter_p50_ms: Some(1.0),
                jitter_p95_ms: Some(2.5),
                jitter_p99_ms: Some(4.0),
                jitter_max_ms: Some(6.0),
                mean_frequency_hz: Some(29.9),
                frequency_stddev_hz: Some(0.4),
                frequency_error_pct: Some(0.33),
            },
        )
    }

    #[test]
    fn event_round_trips() {
        let event = ok_event();
        let json = serde_json::to_string(&event).expect("ser");
        let back: ControlLoopEvent =
            decode_with_version_check::<ControlLoopEvent>(&json).expect("decode");
        assert_eq!(back, event);
    }

    #[test]
    fn target_period_ms_is_derived() {
        let event = ok_event();
        let derived = (1000.0_f64 / 30.0).abs();
        let observed = event.target_period_ms.expect("derived");
        assert!((observed - derived).abs() < 1e-6);
    }

    #[test]
    fn decode_rejects_non_positive_frequency() {
        let json = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","monotonic_timestamp_ns":1,"control_frequency_hz":0.0,"labels":{{"endpoint":"/v1/act","model_class":"vla","model_name":"m","backend":"mock"}},"summary":{{"samples":0,"missed_deadlines":0,"missed_deadline_rate":0.0}}}}"#
        );
        let err = decode_with_version_check::<ControlLoopEvent>(&json).expect_err("rejected");
        assert!(matches!(err, DecodeError::InvalidPayload(_)));
    }

    #[test]
    fn decode_rejects_missed_deadlines_above_samples() {
        let json = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","monotonic_timestamp_ns":1,"control_frequency_hz":30.0,"labels":{{"endpoint":"/v1/act","model_class":"vla","model_name":"m","backend":"mock"}},"summary":{{"samples":1,"missed_deadlines":2,"missed_deadline_rate":0.5}}}}"#
        );
        let err = decode_with_version_check::<ControlLoopEvent>(&json).expect_err("rejected");
        assert!(matches!(err, DecodeError::InvalidPayload(_)));
    }

    #[test]
    fn labels_truncate_to_bound() {
        let labels = ControlLoopLabels::new(
            "x".repeat(MAX_CONTROL_LOOP_LABEL_BYTES + 16),
            "vla",
            "m",
            "mock",
        );
        assert!(labels.endpoint.len() <= MAX_CONTROL_LOOP_LABEL_BYTES);
    }
}
