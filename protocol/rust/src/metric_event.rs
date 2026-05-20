// SPDX-License-Identifier: Apache-2.0
//
// V01-E12-F04: Rust mirror of `protocol/schemas/metric_event.json`.
//
// `MetricEvent` is the local-only metric sample envelope used by the
// observability metrics registry to feed counters, gauges, and
// histograms to the file/stdout/scrape sinks. Label sets are bounded
// to keep cardinality finite on constrained devices.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::correlation_id::validate_correlation_id;
use crate::{DecodeError, ValidatePayload, SCHEMA_VERSION};

/// Maximum bytes for a single metric label key or value.
pub const MAX_METRIC_LABEL_BYTES: usize = 64;

/// Maximum length of the `bucket_upper_bounds` array. Mirrors the
/// JSON schema constraint.
pub const MAX_HISTOGRAM_BUCKETS: usize = 32;

/// Allowed v0.1 metric label keys. Producers that emit other keys
/// have their samples rejected by the registry rather than expanding
/// the metric label cardinality.
pub const ALLOWED_METRIC_LABEL_KEYS: &[&str] = &[
    "endpoint",
    "model_class",
    "model_name",
    "backend",
    "component",
    "status",
];

/// Aggregation kind.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    Counter,
    Gauge,
    Histogram,
}

/// Explicit unit. Producers MUST NOT use the same metric name with
/// different units.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricUnit {
    Count,
    Milliseconds,
    Seconds,
    Hertz,
    Percent,
    Bytes,
    Ratio,
}

impl MetricUnit {
    /// Lowercase wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Milliseconds => "milliseconds",
            Self::Seconds => "seconds",
            Self::Hertz => "hertz",
            Self::Percent => "percent",
            Self::Bytes => "bytes",
            Self::Ratio => "ratio",
        }
    }
}

/// Bounded label map. Wraps a [`BTreeMap`] so iteration order is
/// deterministic across producers and consumers.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MetricLabels(pub BTreeMap<String, String>);

impl MetricLabels {
    #[must_use]
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Insert a label, returning `false` when the key is rejected by
    /// the allowed-keys list or the value exceeds the bounded length.
    /// The caller can use this signal to surface a typed registry
    /// error.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) -> bool {
        let key = key.into();
        let value = value.into();
        if !ALLOWED_METRIC_LABEL_KEYS.iter().any(|k| *k == key) {
            return false;
        }
        if value.len() > MAX_METRIC_LABEL_BYTES {
            return false;
        }
        self.0.insert(key, value);
        true
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Sample payload. Counter / gauge samples set `value`; histogram
/// samples set the bucket arrays plus `count` and `sum`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricSample {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket_upper_bounds: Option<Vec<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket_counts: Option<Vec<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sum: Option<f64>,
}

impl Eq for MetricSample {}

impl MetricSample {
    /// Counter or gauge scalar.
    #[must_use]
    pub fn scalar(value: f64) -> Self {
        Self {
            value: Some(value),
            bucket_upper_bounds: None,
            bucket_counts: None,
            count: None,
            sum: None,
        }
    }

    /// Histogram snapshot. `bucket_counts` length must equal
    /// `bucket_upper_bounds.len() + 1` (the trailing bucket is the
    /// implicit `+Inf`).
    #[must_use]
    pub fn histogram(
        bucket_upper_bounds: Vec<f64>,
        bucket_counts: Vec<u64>,
        count: u64,
        sum: f64,
    ) -> Self {
        Self {
            value: None,
            bucket_upper_bounds: Some(bucket_upper_bounds),
            bucket_counts: Some(bucket_counts),
            count: Some(count),
            sum: Some(sum),
        }
    }
}

/// Wire payload for `protocol/schemas/metric_event.json`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MetricEvent {
    pub schema_version: String,
    pub name: String,
    pub kind: MetricKind,
    pub unit: MetricUnit,
    pub monotonic_timestamp_ns: u64,
    #[serde(default, skip_serializing_if = "MetricLabels::is_empty")]
    pub labels: MetricLabels,
    pub sample: MetricSample,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}

impl MetricEvent {
    /// Build an event with no labels and no correlation id.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        kind: MetricKind,
        unit: MetricUnit,
        monotonic_timestamp_ns: u64,
        sample: MetricSample,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            name: name.into(),
            kind,
            unit,
            monotonic_timestamp_ns,
            labels: MetricLabels::new(),
            sample,
            correlation_id: None,
        }
    }
}

fn validate_metric_name(name: &str) -> Result<(), DecodeError> {
    if name.is_empty() || name.len() > 64 {
        return Err(DecodeError::InvalidPayload(
            "MetricEvent.name must be 1..=64 bytes".into(),
        ));
    }
    if !name.starts_with("tp_") {
        return Err(DecodeError::InvalidPayload(
            "MetricEvent.name must start with `tp_`".into(),
        ));
    }
    let body = &name[3..];
    if body.is_empty() {
        return Err(DecodeError::InvalidPayload(
            "MetricEvent.name must have a body after `tp_`".into(),
        ));
    }
    let first = body.as_bytes()[0];
    if !first.is_ascii_lowercase() {
        return Err(DecodeError::InvalidPayload(
            "MetricEvent.name body must start with a lowercase letter".into(),
        ));
    }
    if !body
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(DecodeError::InvalidPayload(
            "MetricEvent.name body must match [a-z0-9_]+".into(),
        ));
    }
    Ok(())
}

fn validate_labels(labels: &MetricLabels) -> Result<(), DecodeError> {
    for (key, value) in &labels.0 {
        if !ALLOWED_METRIC_LABEL_KEYS.iter().any(|k| *k == key.as_str()) {
            return Err(DecodeError::InvalidPayload(format!(
                "MetricEvent.labels key `{key}` is not in the allowed v0.1 set"
            )));
        }
        if value.len() > MAX_METRIC_LABEL_BYTES {
            return Err(DecodeError::InvalidPayload(format!(
                "MetricEvent.labels value for `{key}` exceeds {MAX_METRIC_LABEL_BYTES} bytes"
            )));
        }
    }
    Ok(())
}

fn validate_sample(kind: MetricKind, sample: &MetricSample) -> Result<(), DecodeError> {
    match kind {
        MetricKind::Counter | MetricKind::Gauge => {
            let Some(value) = sample.value else {
                return Err(DecodeError::InvalidPayload(
                    "MetricEvent.sample.value required for counter/gauge".into(),
                ));
            };
            if !value.is_finite() {
                return Err(DecodeError::InvalidPayload(
                    "MetricEvent.sample.value must be finite".into(),
                ));
            }
            if matches!(kind, MetricKind::Counter) && value < 0.0 {
                return Err(DecodeError::InvalidPayload(
                    "MetricEvent.sample.value for counter must be >= 0".into(),
                ));
            }
            if sample.bucket_upper_bounds.is_some()
                || sample.bucket_counts.is_some()
                || sample.count.is_some()
                || sample.sum.is_some()
            {
                return Err(DecodeError::InvalidPayload(
                    "histogram fields not allowed for counter/gauge".into(),
                ));
            }
        }
        MetricKind::Histogram => {
            let bounds = sample.bucket_upper_bounds.as_ref().ok_or_else(|| {
                DecodeError::InvalidPayload(
                    "MetricEvent.sample.bucket_upper_bounds required for histogram".into(),
                )
            })?;
            let counts = sample.bucket_counts.as_ref().ok_or_else(|| {
                DecodeError::InvalidPayload(
                    "MetricEvent.sample.bucket_counts required for histogram".into(),
                )
            })?;
            if bounds.is_empty() || bounds.len() > MAX_HISTOGRAM_BUCKETS {
                return Err(DecodeError::InvalidPayload(format!(
                    "histogram bucket_upper_bounds must be 1..={MAX_HISTOGRAM_BUCKETS}"
                )));
            }
            if counts.len() != bounds.len() + 1 {
                return Err(DecodeError::InvalidPayload(
                    "histogram bucket_counts length must equal bucket_upper_bounds.len()+1".into(),
                ));
            }
            let mut prev = f64::NEG_INFINITY;
            for b in bounds {
                if !b.is_finite() || *b <= prev {
                    return Err(DecodeError::InvalidPayload(
                        "histogram bucket_upper_bounds must be strictly increasing and finite"
                            .into(),
                    ));
                }
                prev = *b;
            }
            let Some(count) = sample.count else {
                return Err(DecodeError::InvalidPayload(
                    "histogram MetricEvent.sample.count required".into(),
                ));
            };
            let Some(sum) = sample.sum else {
                return Err(DecodeError::InvalidPayload(
                    "histogram MetricEvent.sample.sum required".into(),
                ));
            };
            if !sum.is_finite() {
                return Err(DecodeError::InvalidPayload(
                    "histogram MetricEvent.sample.sum must be finite".into(),
                ));
            }
            let total: u64 = counts.iter().sum();
            if total != count {
                return Err(DecodeError::InvalidPayload(
                    "histogram bucket_counts sum must equal MetricEvent.sample.count".into(),
                ));
            }
            if sample.value.is_some() {
                return Err(DecodeError::InvalidPayload(
                    "histogram MetricEvent.sample.value must be absent".into(),
                ));
            }
        }
    }
    Ok(())
}

impl ValidatePayload for MetricEvent {
    fn validate_payload(self) -> Result<Self, DecodeError> {
        validate_metric_name(&self.name)?;
        validate_labels(&self.labels)?;
        validate_sample(self.kind, &self.sample)?;
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
        MetricEvent, MetricKind, MetricLabels, MetricSample, MetricUnit, MAX_METRIC_LABEL_BYTES,
    };
    use crate::{decode_with_version_check, DecodeError, SCHEMA_VERSION};

    #[test]
    fn counter_event_round_trips() {
        let mut labels = MetricLabels::new();
        assert!(labels.insert("backend", "tensorrt"));
        let event = MetricEvent {
            schema_version: SCHEMA_VERSION.to_string(),
            name: "tp_infer_requests_total".into(),
            kind: MetricKind::Counter,
            unit: MetricUnit::Count,
            monotonic_timestamp_ns: 1,
            labels,
            sample: MetricSample::scalar(7.0),
            correlation_id: None,
        };
        let json = serde_json::to_string(&event).expect("ser");
        let back: MetricEvent = serde_json::from_str(&json).expect("de");
        assert_eq!(back, event);
    }

    #[test]
    fn histogram_round_trips_and_validates() {
        let event = MetricEvent::new(
            "tp_infer_latency_ms",
            MetricKind::Histogram,
            MetricUnit::Milliseconds,
            42,
            MetricSample::histogram(vec![5.0, 10.0, 25.0], vec![1, 2, 3, 4], 10, 75.0),
        );
        let json = serde_json::to_string(&event).expect("ser");
        let back: MetricEvent = decode_with_version_check::<MetricEvent>(&json).expect("decode");
        assert_eq!(back, event);
    }

    #[test]
    fn label_insertion_rejects_unknown_key() {
        let mut labels = MetricLabels::new();
        assert!(!labels.insert("custom_label", "v"));
        assert!(labels.is_empty());
    }

    #[test]
    fn label_insertion_rejects_overlong_value() {
        let mut labels = MetricLabels::new();
        assert!(!labels.insert("backend", "x".repeat(MAX_METRIC_LABEL_BYTES + 1)));
    }

    #[test]
    fn decode_rejects_name_without_tp_prefix() {
        let json = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"infer_total","kind":"counter","unit":"count","monotonic_timestamp_ns":1,"sample":{{"value":1.0}}}}"#
        );
        let err = decode_with_version_check::<MetricEvent>(&json).expect_err("rejected");
        assert!(matches!(err, DecodeError::InvalidPayload(_)));
    }

    #[test]
    fn decode_rejects_negative_counter() {
        let json = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"tp_x","kind":"counter","unit":"count","monotonic_timestamp_ns":1,"sample":{{"value":-1.0}}}}"#
        );
        let err = decode_with_version_check::<MetricEvent>(&json).expect_err("rejected");
        assert!(matches!(err, DecodeError::InvalidPayload(_)));
    }

    #[test]
    fn decode_rejects_histogram_with_mismatched_count() {
        // bucket_counts sum (1+2+3+0 = 6) != count (5).
        let json = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"tp_x_ms","kind":"histogram","unit":"milliseconds","monotonic_timestamp_ns":1,"sample":{{"bucket_upper_bounds":[1.0,2.0,3.0],"bucket_counts":[1,2,3,0],"count":5,"sum":6.0}}}}"#
        );
        let err = decode_with_version_check::<MetricEvent>(&json).expect_err("rejected");
        assert!(matches!(err, DecodeError::InvalidPayload(_)));
    }

    #[test]
    fn decode_rejects_unknown_label_key() {
        let json = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"tp_x","kind":"gauge","unit":"count","monotonic_timestamp_ns":1,"labels":{{"bad":"v"}},"sample":{{"value":1.0}}}}"#
        );
        let err = decode_with_version_check::<MetricEvent>(&json).expect_err("rejected");
        assert!(matches!(err, DecodeError::InvalidPayload(_)));
    }
}
