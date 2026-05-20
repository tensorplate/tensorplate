// SPDX-License-Identifier: Apache-2.0
//
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

// V01-E12-F04: Local metrics registry and bounded export.
//
// The registry tracks counters, gauges, and histograms identified by
// `(name, labels)` pairs and exports them as wire-format
// [`MetricEvent`] payloads. Recording a sample never blocks on a slow
// sink: counters and gauges hold an atomic value, histograms hold a
// short fixed-size bucket array under a single mutex.
//
// Label cardinality is enforced at registration time: only the keys
// from [`tensorplate_protocol::metric_event::ALLOWED_METRIC_LABEL_KEYS`]
// are accepted, and values are bounded by
// [`tensorplate_protocol::MAX_METRIC_LABEL_BYTES`]. A registration that
// violates the policy is rejected with a typed error and a bounded
// counter rather than silently expanding the label space.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use tensorplate_protocol::metric_event::ALLOWED_METRIC_LABEL_KEYS;
use tensorplate_protocol::{
    MetricEvent, MetricKind, MetricLabels, MetricSample, MetricUnit, MAX_METRIC_LABEL_BYTES,
    SCHEMA_VERSION,
};

use crate::clock::MonotonicClock;
use crate::error::{ObservabilityError, ObservabilityResult};

/// Maximum number of distinct `(name, labels)` series the v0.1 registry
/// will accept. Producers exceeding this receive a typed error rather
/// than silently expanding the registry.
pub const MAX_METRIC_SERIES: usize = 256;

/// Maximum number of histogram bucket upper bounds. Matches the
/// protocol-level constraint.
pub const MAX_HISTOGRAM_BUCKETS: usize = 32;

/// Export sink configuration. The v0.1 baseline supports a file sink,
/// a stdout sink, and an in-memory scrape sink. None of the sinks
/// require a hosted platform connection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum MetricSinkConfig {
    /// Drop metrics on the floor. The registry still records samples;
    /// `take_snapshot` is available for tests.
    Noop,
    /// Append wire-format JSON lines to the configured file. Writes are
    /// best-effort; failures bump a bounded counter.
    File { path: PathBuf },
    /// Write wire-format JSON lines to standard output.
    Stdout,
    /// Hold the most recent snapshot in memory; consumers read it
    /// through [`MetricsRegistry::take_snapshot`].
    InMemory,
}

impl Default for MetricSinkConfig {
    fn default() -> Self {
        Self::InMemory
    }
}

/// Behaviour when the sink cannot accept a sample (queue full, write
/// error). The default is `Drop`: counters bump and the producer
/// continues, mirroring the safe-state sink pattern.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SinkBackpressurePolicy {
    /// Drop the sample and increment a typed counter.
    #[default]
    Drop,
    /// Drop the sample and bump the counter, and also emit a bounded
    /// warning through the log emitter.
    DropWithWarning,
}

/// V01-E12-F04 metrics export config.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MetricsExportConfig {
    /// Cap on the number of `(name, labels)` series the registry
    /// accepts.
    #[serde(default = "default_max_series")]
    pub max_series: u32,
    /// Cap on the number of samples buffered for the bounded queue.
    #[serde(default = "default_export_queue_capacity")]
    pub queue_capacity: u32,
    #[serde(default)]
    pub sink: MetricSinkConfig,
    #[serde(default)]
    pub backpressure: SinkBackpressurePolicy,
}

impl Default for MetricsExportConfig {
    fn default() -> Self {
        Self {
            max_series: default_max_series(),
            queue_capacity: default_export_queue_capacity(),
            sink: MetricSinkConfig::default(),
            backpressure: SinkBackpressurePolicy::default(),
        }
    }
}

const fn default_max_series() -> u32 {
    256
}
const fn default_export_queue_capacity() -> u32 {
    1_024
}

/// Bounded counters surfaced through the snapshot.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct MetricsCounters {
    pub series_registered: u64,
    pub series_rejected_unknown_label: u64,
    pub series_rejected_bounded_label: u64,
    pub series_rejected_full: u64,
    pub samples_recorded: u64,
    pub samples_dropped_queue_full: u64,
    pub sink_write_errors: u64,
}

#[derive(Clone, Debug)]
struct SeriesKey {
    name: String,
    kind: MetricKind,
    unit: MetricUnit,
    labels: MetricLabels,
}

#[derive(Debug)]
enum SeriesValue {
    Counter(AtomicU64),
    Gauge(Mutex<f64>),
    Histogram(Mutex<HistogramSeries>),
}

#[derive(Clone, Debug)]
struct HistogramSeries {
    bounds: Vec<f64>,
    counts: Vec<u64>,
    count: u64,
    sum: f64,
}

impl HistogramSeries {
    fn new(bounds: Vec<f64>) -> Self {
        let len = bounds.len() + 1;
        Self {
            bounds,
            counts: vec![0; len],
            count: 0,
            sum: 0.0,
        }
    }

    fn record(&mut self, value: f64) {
        let idx = self
            .bounds
            .iter()
            .position(|b| value <= *b)
            .unwrap_or(self.bounds.len());
        for bucket in &mut self.counts[idx..] {
            *bucket = bucket.saturating_add(1);
        }
        self.count = self.count.saturating_add(1);
        self.sum += value;
    }
}

/// Handle returned by `register_*` to record samples without re-hashing
/// the series name.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SeriesId(usize);

/// V01-E12-F04 metrics registry.
pub struct MetricsRegistry {
    epoch: Instant,
    inner: Mutex<RegistryInner>,
    counters: Mutex<MetricsCounters>,
    config: MetricsExportConfig,
}

struct RegistryInner {
    series: Vec<(SeriesKey, SeriesValue)>,
}

impl MetricsRegistry {
    /// Build a new registry. `epoch` is the monotonic reference used to
    /// fill the `monotonic_timestamp_ns` field of exported samples.
    #[must_use]
    pub fn new(config: MetricsExportConfig, clock: &dyn MonotonicClock) -> Self {
        Self {
            epoch: clock.now(),
            inner: Mutex::new(RegistryInner { series: Vec::new() }),
            counters: Mutex::new(MetricsCounters::default()),
            config,
        }
    }

    /// Borrowed view of the running counters.
    pub fn counters(&self) -> MetricsCounters {
        match self.counters.lock() {
            Ok(g) => g.clone(),
            Err(_) => MetricsCounters::default(),
        }
    }

    /// Register a counter. Returns the series id; subsequent calls with
    /// the same `(name, labels)` return the existing id.
    ///
    /// # Errors
    ///
    /// [`ObservabilityError::InvalidEvent`] when the label set violates
    /// the bounded-label policy. [`ObservabilityError::Internal`] when
    /// the registry has reached [`MetricsExportConfig::max_series`].
    pub fn register_counter(
        &self,
        name: &str,
        unit: MetricUnit,
        labels: MetricLabels,
    ) -> ObservabilityResult<SeriesId> {
        self.register(name, MetricKind::Counter, unit, labels, None)
    }

    /// Register a gauge.
    pub fn register_gauge(
        &self,
        name: &str,
        unit: MetricUnit,
        labels: MetricLabels,
    ) -> ObservabilityResult<SeriesId> {
        self.register(name, MetricKind::Gauge, unit, labels, None)
    }

    /// Register a histogram with the supplied strictly-increasing bucket
    /// upper bounds. The implicit `+Inf` bucket is always added.
    pub fn register_histogram(
        &self,
        name: &str,
        unit: MetricUnit,
        labels: MetricLabels,
        bounds: Vec<f64>,
    ) -> ObservabilityResult<SeriesId> {
        if bounds.is_empty() || bounds.len() > MAX_HISTOGRAM_BUCKETS {
            return Err(ObservabilityError::InvalidEvent(format!(
                "histogram bucket count must be 1..={MAX_HISTOGRAM_BUCKETS}"
            )));
        }
        let mut prev = f64::NEG_INFINITY;
        for b in &bounds {
            if !b.is_finite() || *b <= prev {
                return Err(ObservabilityError::InvalidEvent(
                    "histogram buckets must be strictly increasing and finite".into(),
                ));
            }
            prev = *b;
        }
        self.register(name, MetricKind::Histogram, unit, labels, Some(bounds))
    }

    fn register(
        &self,
        name: &str,
        kind: MetricKind,
        unit: MetricUnit,
        labels: MetricLabels,
        bounds: Option<Vec<f64>>,
    ) -> ObservabilityResult<SeriesId> {
        self.check_metric_name(name)?;
        self.check_labels(&labels)?;
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| ObservabilityError::Internal("metrics registry poisoned".into()))?;
        if let Some(idx) = inner.series.iter().position(|(k, _)| {
            k.name == name && k.unit == unit && k.labels == labels && k.kind == kind
        }) {
            return Ok(SeriesId(idx));
        }
        if inner.series.len() >= self.config.max_series as usize
            || inner.series.len() >= MAX_METRIC_SERIES
        {
            self.bump_rejected_full();
            return Err(ObservabilityError::Internal(format!(
                "metrics registry full ({} series)",
                inner.series.len()
            )));
        }
        let value = match kind {
            MetricKind::Counter => SeriesValue::Counter(AtomicU64::new(0)),
            MetricKind::Gauge => SeriesValue::Gauge(Mutex::new(0.0)),
            MetricKind::Histogram => {
                let b = bounds.ok_or_else(|| {
                    ObservabilityError::InvalidEvent("histogram requires bounds".into())
                })?;
                SeriesValue::Histogram(Mutex::new(HistogramSeries::new(b)))
            }
        };
        let idx = inner.series.len();
        inner.series.push((
            SeriesKey {
                name: name.to_string(),
                kind,
                unit,
                labels,
            },
            value,
        ));
        if let Ok(mut c) = self.counters.lock() {
            c.series_registered += 1;
        }
        Ok(SeriesId(idx))
    }

    #[allow(clippy::unused_self)]
    fn check_metric_name(&self, name: &str) -> ObservabilityResult<()> {
        if name.is_empty() || name.len() > 64 {
            return Err(ObservabilityError::InvalidEvent(
                "metric name must be 1..=64 bytes".into(),
            ));
        }
        if !name.starts_with("tp_") {
            return Err(ObservabilityError::InvalidEvent(
                "metric name must start with `tp_`".into(),
            ));
        }
        let body = &name[3..];
        if body.is_empty() {
            return Err(ObservabilityError::InvalidEvent(
                "metric name body is empty".into(),
            ));
        }
        if !body.as_bytes()[0].is_ascii_lowercase()
            || !body
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err(ObservabilityError::InvalidEvent(
                "metric name body must match [a-z][a-z0-9_]*".into(),
            ));
        }
        Ok(())
    }

    fn check_labels(&self, labels: &MetricLabels) -> ObservabilityResult<()> {
        for (k, v) in &labels.0 {
            if !ALLOWED_METRIC_LABEL_KEYS
                .iter()
                .any(|allowed| *allowed == k)
            {
                if let Ok(mut c) = self.counters.lock() {
                    c.series_rejected_unknown_label += 1;
                }
                return Err(ObservabilityError::InvalidEvent(format!(
                    "label key `{k}` is not in the bounded v0.1 set"
                )));
            }
            if v.len() > MAX_METRIC_LABEL_BYTES {
                if let Ok(mut c) = self.counters.lock() {
                    c.series_rejected_bounded_label += 1;
                }
                return Err(ObservabilityError::InvalidEvent(format!(
                    "label value for `{k}` exceeds {MAX_METRIC_LABEL_BYTES} bytes"
                )));
            }
        }
        Ok(())
    }

    fn bump_rejected_full(&self) {
        if let Ok(mut c) = self.counters.lock() {
            c.series_rejected_full += 1;
        }
    }

    fn bump_samples_recorded(&self) {
        if let Ok(mut c) = self.counters.lock() {
            c.samples_recorded += 1;
        }
    }

    /// Increment a counter by `delta`. `delta` is converted via
    /// saturating cast.
    ///
    /// # Errors
    ///
    /// [`ObservabilityError::Internal`] when the series id is invalid
    /// or refers to a gauge / histogram.
    pub fn inc_counter(&self, id: SeriesId, delta: u64) -> ObservabilityResult<()> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| ObservabilityError::Internal("metrics registry poisoned".into()))?;
        let (_, value) = inner
            .series
            .get(id.0)
            .ok_or_else(|| ObservabilityError::Internal("series id out of range".into()))?;
        match value {
            SeriesValue::Counter(a) => {
                a.fetch_add(delta, Ordering::Relaxed);
                drop(inner);
                self.bump_samples_recorded();
                Ok(())
            }
            _ => Err(ObservabilityError::Internal(
                "series is not a counter".into(),
            )),
        }
    }

    /// Set a gauge to `value`.
    pub fn set_gauge(&self, id: SeriesId, value: f64) -> ObservabilityResult<()> {
        if !value.is_finite() {
            return Err(ObservabilityError::InvalidEvent(
                "gauge value must be finite".into(),
            ));
        }
        let inner = self
            .inner
            .lock()
            .map_err(|_| ObservabilityError::Internal("metrics registry poisoned".into()))?;
        let (_, slot) = inner
            .series
            .get(id.0)
            .ok_or_else(|| ObservabilityError::Internal("series id out of range".into()))?;
        if let SeriesValue::Gauge(m) = slot {
            let mut guard = m
                .lock()
                .map_err(|_| ObservabilityError::Internal("gauge poisoned".into()))?;
            *guard = value;
            drop(guard);
            drop(inner);
            self.bump_samples_recorded();
            Ok(())
        } else {
            Err(ObservabilityError::Internal("series is not a gauge".into()))
        }
    }

    /// Observe a histogram sample.
    pub fn observe(&self, id: SeriesId, value: f64) -> ObservabilityResult<()> {
        if !value.is_finite() {
            return Err(ObservabilityError::InvalidEvent(
                "histogram observation must be finite".into(),
            ));
        }
        let inner = self
            .inner
            .lock()
            .map_err(|_| ObservabilityError::Internal("metrics registry poisoned".into()))?;
        let (_, slot) = inner
            .series
            .get(id.0)
            .ok_or_else(|| ObservabilityError::Internal("series id out of range".into()))?;
        match slot {
            SeriesValue::Histogram(m) => {
                let mut guard = m
                    .lock()
                    .map_err(|_| ObservabilityError::Internal("histogram poisoned".into()))?;
                guard.record(value);
                drop(guard);
                drop(inner);
                self.bump_samples_recorded();
                Ok(())
            }
            _ => Err(ObservabilityError::Internal(
                "series is not a histogram".into(),
            )),
        }
    }

    /// Snapshot the registry as a vector of wire-format
    /// [`MetricEvent`] payloads. The timestamp on each event is
    /// `now - epoch` in nanoseconds, sampled from the supplied clock.
    pub fn take_snapshot(&self, clock: &dyn MonotonicClock) -> Vec<MetricEvent> {
        let ts = clock
            .now()
            .saturating_duration_since(self.epoch)
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64;
        let Ok(inner) = self.inner.lock() else {
            return Vec::new();
        };
        let mut events = Vec::with_capacity(inner.series.len());
        for (key, value) in &inner.series {
            let sample = match value {
                SeriesValue::Counter(a) => MetricSample::scalar(a.load(Ordering::Relaxed) as f64),
                SeriesValue::Gauge(m) => match m.lock() {
                    Ok(g) => MetricSample::scalar(*g),
                    Err(_) => continue,
                },
                SeriesValue::Histogram(m) => match m.lock() {
                    Ok(g) => {
                        let h = g.clone();
                        MetricSample::histogram(h.bounds, h.counts, h.count, h.sum)
                    }
                    Err(_) => continue,
                },
            };
            events.push(MetricEvent {
                schema_version: SCHEMA_VERSION.to_string(),
                name: key.name.clone(),
                kind: key.kind,
                unit: key.unit,
                monotonic_timestamp_ns: ts,
                labels: key.labels.clone(),
                sample,
                correlation_id: None,
            });
        }
        events
    }

    /// Number of registered series.
    pub fn series_count(&self) -> usize {
        self.inner.lock().map(|i| i.series.len()).unwrap_or(0)
    }

    /// Export the current snapshot through the configured sink.
    ///
    /// # Errors
    ///
    /// [`ObservabilityError::SnapshotSink`] when a file sink write
    /// fails; the registry bumps `sink_write_errors` and continues.
    pub fn export(&self, clock: &dyn MonotonicClock) -> ObservabilityResult<()> {
        let snapshot = self.take_snapshot(clock);
        match &self.config.sink {
            MetricSinkConfig::Noop | MetricSinkConfig::InMemory => Ok(()),
            MetricSinkConfig::Stdout => self.write_lines(snapshot, |line| {
                println!("{line}");
                Ok(())
            }),
            MetricSinkConfig::File { path } => {
                use std::io::Write;
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .map_err(|e| {
                        self.bump_sink_write_error();
                        ObservabilityError::SnapshotSink(format!("open {}: {e}", path.display()))
                    })?;
                self.write_lines(snapshot, |line| {
                    writeln!(file, "{line}").map_err(|e| {
                        ObservabilityError::SnapshotSink(format!("write metric line: {e}"))
                    })
                })
            }
        }
    }

    fn bump_sink_write_error(&self) {
        if let Ok(mut c) = self.counters.lock() {
            c.sink_write_errors += 1;
        }
    }

    fn write_lines<F>(&self, events: Vec<MetricEvent>, mut emit: F) -> ObservabilityResult<()>
    where
        F: FnMut(&str) -> ObservabilityResult<()>,
    {
        for event in events {
            let line = serde_json::to_string(&event)
                .map_err(|e| ObservabilityError::SnapshotSink(format!("serialise metric: {e}")))?;
            if let Err(e) = emit(&line) {
                self.bump_sink_write_error();
                return Err(e);
            }
        }
        Ok(())
    }
}

/// Convenience builder for canonical v0.1 latency histograms (millisecond
/// units). The buckets are spaced for inference latency on the Jetson
/// Orin Nano 8GB.
#[must_use]
pub fn default_latency_buckets_ms() -> Vec<f64> {
    vec![
        1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0,
    ]
}

/// Convenience labels with the bounded `endpoint`/`backend` pair set.
#[must_use]
pub fn endpoint_backend_labels(endpoint: &str, backend: &str) -> MetricLabels {
    let mut labels = MetricLabels::new();
    let _ = labels.insert("endpoint", endpoint.to_string());
    let _ = labels.insert("backend", backend.to_string());
    labels
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{
        default_latency_buckets_ms, endpoint_backend_labels, MetricSinkConfig, MetricsExportConfig,
        MetricsRegistry,
    };
    use crate::clock::FakeClock;
    use std::time::Duration;
    use tensorplate_protocol::{MetricKind, MetricLabels, MetricUnit};

    fn setup() -> (MetricsRegistry, FakeClock) {
        let clock = FakeClock::new();
        let registry = MetricsRegistry::new(MetricsExportConfig::default(), &clock);
        (registry, clock)
    }

    #[test]
    fn counter_increments_and_snapshots() {
        let (registry, clock) = setup();
        let labels = endpoint_backend_labels("/v1/infer", "mock");
        let id = registry
            .register_counter("tp_infer_total", MetricUnit::Count, labels)
            .expect("register");
        registry.inc_counter(id, 3).expect("inc");
        registry.inc_counter(id, 1).expect("inc");
        clock.advance(Duration::from_millis(10));
        let snapshot = registry.take_snapshot(&clock);
        assert_eq!(snapshot.len(), 1);
        let event = &snapshot[0];
        assert_eq!(event.name, "tp_infer_total");
        assert_eq!(event.kind, MetricKind::Counter);
        assert_eq!(event.sample.value, Some(4.0));
        assert!(event.monotonic_timestamp_ns >= 10_000_000);
    }

    #[test]
    fn registration_rejects_unknown_label() {
        let (registry, _) = setup();
        let mut labels = MetricLabels::new();
        labels.0.insert("bogus".into(), "v".into());
        let err = registry
            .register_counter("tp_x", MetricUnit::Count, labels)
            .expect_err("rejected");
        assert!(err.to_string().contains("not in the bounded"));
        assert_eq!(registry.counters().series_rejected_unknown_label, 1);
    }

    #[test]
    fn registration_rejects_overlong_label_value() {
        let (registry, _) = setup();
        let mut labels = MetricLabels::new();
        labels.0.insert("backend".into(), "x".repeat(65));
        let err = registry
            .register_counter("tp_x", MetricUnit::Count, labels)
            .expect_err("rejected");
        assert!(err.to_string().contains("exceeds"));
        assert_eq!(registry.counters().series_rejected_bounded_label, 1);
    }

    #[test]
    fn registration_rejects_overflowing_series() {
        let config = MetricsExportConfig {
            max_series: 2,
            ..MetricsExportConfig::default()
        };
        let clock = FakeClock::new();
        let registry = MetricsRegistry::new(config, &clock);
        for i in 0..2 {
            let labels = endpoint_backend_labels("/v1/x", &format!("backend{i}"));
            registry
                .register_counter(&format!("tp_n{i}"), MetricUnit::Count, labels)
                .expect("register");
        }
        let labels = endpoint_backend_labels("/v1/x", "extra");
        let err = registry
            .register_counter("tp_overflow", MetricUnit::Count, labels)
            .expect_err("rejected");
        assert!(err.to_string().contains("registry full"));
        assert_eq!(registry.counters().series_rejected_full, 1);
    }

    #[test]
    fn gauge_set_overrides_previous_value() {
        let (registry, clock) = setup();
        let labels = endpoint_backend_labels("/v1/infer", "mock");
        let id = registry
            .register_gauge("tp_queue_depth", MetricUnit::Count, labels)
            .expect("register");
        registry.set_gauge(id, 4.0).expect("set");
        registry.set_gauge(id, 7.5).expect("set");
        let snapshot = registry.take_snapshot(&clock);
        assert_eq!(snapshot[0].sample.value, Some(7.5));
        assert_eq!(snapshot[0].kind, MetricKind::Gauge);
    }

    #[test]
    fn histogram_observations_fill_buckets() {
        let (registry, clock) = setup();
        let labels = endpoint_backend_labels("/v1/infer", "mock");
        let id = registry
            .register_histogram(
                "tp_infer_latency_ms",
                MetricUnit::Milliseconds,
                labels,
                default_latency_buckets_ms(),
            )
            .expect("register");
        for v in [2.0, 7.0, 20.0, 200.0] {
            registry.observe(id, v).expect("observe");
        }
        let snapshot = registry.take_snapshot(&clock);
        let sample = &snapshot[0].sample;
        let counts = sample.bucket_counts.as_ref().expect("counts");
        // Cumulative Prometheus-style counts:
        // <=1:0, <=5:1, <=10:2, <=25:3, <=50:3, <=100:3,
        // <=250:4, and every larger bucket including +Inf:4.
        let expected = vec![0, 1, 2, 3, 3, 3, 4, 4, 4, 4, 4];
        assert_eq!(counts, &expected);
        assert_eq!(sample.count, Some(4));
        assert!((sample.sum.unwrap() - 229.0).abs() < 1e-9);
    }

    #[test]
    fn idempotent_registration_returns_same_id() {
        let (registry, _) = setup();
        let labels = endpoint_backend_labels("/v1/infer", "mock");
        let id1 = registry
            .register_counter("tp_total", MetricUnit::Count, labels.clone())
            .expect("first");
        let id2 = registry
            .register_counter("tp_total", MetricUnit::Count, labels)
            .expect("second");
        assert_eq!(id1, id2);
    }

    #[test]
    fn file_sink_appends_json_lines() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("metrics.jsonl");
        let config = MetricsExportConfig {
            sink: MetricSinkConfig::File { path: path.clone() },
            ..MetricsExportConfig::default()
        };
        let clock = FakeClock::new();
        let registry = MetricsRegistry::new(config, &clock);
        let labels = endpoint_backend_labels("/v1/infer", "mock");
        let id = registry
            .register_counter("tp_x", MetricUnit::Count, labels)
            .expect("register");
        registry.inc_counter(id, 2).expect("inc");
        registry.export(&clock).expect("export");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(body.lines().count() >= 1);
        assert!(body.contains("tp_x"));
    }
}
