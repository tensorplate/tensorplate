// SPDX-License-Identifier: Apache-2.0
//
// V01-E10-F06: Minimal local status snapshot and bounded diagnostics.
//
// The snapshot exposes the v0.1.0 required status fields: `agent_state`,
// `serving_state`, `observability_state`, `active_deployment`, `backend`,
// `missed_heartbeat_count`, `missed_deadline_rate`, `queue_depth`, and
// `last_error_code`, plus the bookkeeping the V01-E11 CLI and release validation
// validation harness need (`last_event_sequence`, `last_heartbeat_age_ms`,
// safe-state sink status, ROS 2 publisher status).
//
// The snapshot is read-only from the consumer's point of view. The
// writer either keeps it in memory or replaces a file atomically; in
// both cases partial reads cannot expose half-written records.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

fn usize_to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

use serde::{Deserialize, Serialize};

use tensorplate_protocol::error::ErrorCode;
use tensorplate_protocol::failure_reason::FailureReason;
use tensorplate_protocol::worker_status::ComponentState;
use tensorplate_protocol::ControlLoopSummary;

use crate::config::StatusSnapshotConfig;
use crate::error::{ObservabilityError, ObservabilityResult};
use crate::listener::ListenerCountersSnapshot;
use crate::metrics::MetricsCounters;
use crate::retention::RetentionCounters;
use crate::state::AggregateState;

/// V01-E10-F06 status snapshot. Mirrors
/// `protocol/schemas/observability_status.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub schema_version: String,
    pub observability_state: String,
    pub agent_state: String,
    pub serving_state: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub active_deployment: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub backend: String,
    pub missed_heartbeat_count: u64,
    pub missed_deadline_rate: f64,
    pub queue_depth: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_code: Option<ErrorCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_sequence: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_age_ms: Option<u64>,
    pub safe_state_sink: SinkStatus,
    pub ros2_publisher: PublisherStatus,
    pub listener: ListenerStatus,
    pub diagnostics: BoundedDiagnostics,
    /// V01-E12-F06 retention sink health.
    #[serde(default, skip_serializing_if = "DiagnosticsSinkStatus::is_default")]
    pub diagnostics_sink: DiagnosticsSinkStatus,
    /// V01-E12-F04 metrics registry health.
    #[serde(default, skip_serializing_if = "MetricsExportStatus::is_default")]
    pub metrics_export: MetricsExportStatus,
    /// V01-E12-F05 control-loop summary. Present only when the serving
    /// worker emits control-loop events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_loop: Option<ControlLoopStatus>,
    /// V01-E12-F02 last correlation id observed by the snapshot
    /// writer. Operators use it to join `tensorplate logs` output
    /// against the current status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_correlation_id: Option<String>,
    /// V01-E12-F03 last operator-visible failure reason recorded by
    /// the snapshot writer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure_reason: Option<FailureReason>,
}

/// Safe-state sink status surfaced through the snapshot.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SinkStatus {
    pub enabled: bool,
    pub dropped: u64,
    pub errors: u64,
}

/// ROS 2 publisher status surfaced through the snapshot.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PublisherStatus {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub topic: String,
    pub published: u64,
    pub errors: u64,
}

/// Listener bookkeeping surfaced through the snapshot.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ListenerStatus {
    pub accepted: u64,
    pub dropped: u64,
    pub malformed: u64,
    pub duplicates: u64,
    pub out_of_order: u64,
    pub unknown_version: u64,
}

impl From<ListenerCountersSnapshot> for ListenerStatus {
    fn from(c: ListenerCountersSnapshot) -> Self {
        Self {
            accepted: c.accepted,
            dropped: c.dropped,
            malformed: c.malformed,
            duplicates: c.duplicates,
            out_of_order: c.out_of_order,
            unknown_version: c.unknown_version,
        }
    }
}

/// Bounded diagnostics ring surfaced through the snapshot. The ring
/// retains the most-recent transitions and sink/publisher errors.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BoundedDiagnostics {
    pub capacity: u32,
    pub recent_transitions: Vec<RecentTransition>,
    pub recent_errors: Vec<RecentError>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecentTransition {
    pub previous_state: String,
    pub state: String,
    pub reason: String,
    pub monotonic_age_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecentError {
    pub component: String,
    pub code: ErrorCode,
    pub message: String,
}

/// V01-E12-F06 retention sink status.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticsSinkStatus {
    pub enqueued: u64,
    pub dropped_queue_full: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub dropped_redacted: u64,
    pub file_write_errors: u64,
    pub file_rotations: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub drains: u64,
}

impl DiagnosticsSinkStatus {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

impl From<RetentionCounters> for DiagnosticsSinkStatus {
    fn from(c: RetentionCounters) -> Self {
        Self {
            enqueued: c.enqueued,
            dropped_queue_full: c.dropped_queue_full,
            dropped_redacted: c.dropped_redacted,
            file_write_errors: c.file_write_errors,
            file_rotations: c.file_rotations,
            drains: c.drains,
        }
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

/// V01-E12-F04 metrics registry health.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsExportStatus {
    pub series_registered: u64,
    pub samples_recorded: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub series_rejected_unknown_label: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub series_rejected_bounded_label: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub series_rejected_full: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub samples_dropped_queue_full: u64,
    pub sink_write_errors: u64,
}

impl MetricsExportStatus {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

impl From<MetricsCounters> for MetricsExportStatus {
    fn from(c: MetricsCounters) -> Self {
        Self {
            series_registered: c.series_registered,
            samples_recorded: c.samples_recorded,
            series_rejected_unknown_label: c.series_rejected_unknown_label,
            series_rejected_bounded_label: c.series_rejected_bounded_label,
            series_rejected_full: c.series_rejected_full,
            samples_dropped_queue_full: c.samples_dropped_queue_full,
            sink_write_errors: c.sink_write_errors,
        }
    }
}

/// V01-E12-F05 control-loop summary projected through the snapshot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ControlLoopStatus {
    pub control_frequency_hz: f64,
    #[serde(default = "default_window_seconds")]
    pub rolling_window_seconds: u32,
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
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model_class: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub backend: String,
}

fn default_window_seconds() -> u32 {
    60
}

impl ControlLoopStatus {
    /// Build a status row from a [`ControlLoopSummary`] plus the
    /// configured frequency, window, and label set.
    #[must_use]
    pub fn from_summary(
        control_frequency_hz: f64,
        rolling_window_seconds: u32,
        endpoint: impl Into<String>,
        model_class: impl Into<String>,
        model_name: impl Into<String>,
        backend: impl Into<String>,
        summary: &ControlLoopSummary,
    ) -> Self {
        Self {
            control_frequency_hz,
            rolling_window_seconds,
            samples: summary.samples,
            missed_deadlines: summary.missed_deadlines,
            missed_deadline_rate: summary.missed_deadline_rate,
            jitter_p50_ms: summary.jitter_p50_ms,
            jitter_p95_ms: summary.jitter_p95_ms,
            jitter_p99_ms: summary.jitter_p99_ms,
            jitter_max_ms: summary.jitter_max_ms,
            mean_frequency_hz: summary.mean_frequency_hz,
            frequency_stddev_hz: summary.frequency_stddev_hz,
            frequency_error_pct: summary.frequency_error_pct,
            endpoint: endpoint.into(),
            model_class: model_class.into(),
            model_name: model_name.into(),
            backend: backend.into(),
        }
    }
}

/// In-memory snapshot writer. Use [`SnapshotWriter::with_path`] to
/// also persist to disk via atomic-replace.
pub struct SnapshotWriter {
    inner: Mutex<SnapshotInner>,
    diagnostics_capacity: usize,
    path: Option<PathBuf>,
}

struct SnapshotInner {
    current: StatusSnapshot,
    transitions: VecDeque<RecentTransition>,
    errors: VecDeque<RecentError>,
}

impl SnapshotWriter {
    /// Construct a writer using the configured snapshot mode. File-mode
    /// writers also remember the path so `update` can perform an
    /// atomic file replacement.
    #[must_use]
    pub fn new(cfg: &StatusSnapshotConfig) -> Self {
        let capacity = cfg.diagnostics_capacity.max(1) as usize;
        let path = match cfg.kind {
            crate::config::StatusSnapshotKind::InMemory => None,
            crate::config::StatusSnapshotKind::File => cfg.path.clone(),
        };
        Self {
            inner: Mutex::new(SnapshotInner {
                current: empty_snapshot(capacity),
                transitions: VecDeque::with_capacity(capacity),
                errors: VecDeque::with_capacity(capacity),
            }),
            diagnostics_capacity: capacity,
            path,
        }
    }

    /// Update the snapshot using the latest aggregator and listener
    /// state. Pure mutation; persistence is opt-in through [`flush`].
    pub fn update(
        &self,
        state: &AggregateState,
        listener: ListenerStatus,
        sink: SinkStatus,
        publisher: PublisherStatus,
    ) {
        #[allow(clippy::expect_used)]
        let mut inner = self.inner.lock().expect("snapshot writer poisoned");
        inner.current.observability_state = state.state.as_str().into();
        inner.current.agent_state = component_state_label(state.agent_state);
        inner.current.serving_state = component_state_label(state.serving_state);
        inner
            .current
            .active_deployment
            .clone_from(&state.active_deployment);
        inner.current.backend.clone_from(&state.backend);
        inner.current.missed_heartbeat_count = state.missed_heartbeat_count;
        inner.current.missed_deadline_rate = state.missed_deadline_rate;
        inner.current.queue_depth = state.queue_depth;
        inner.current.last_error_code = state.last_error_code;
        inner.current.last_event_sequence = state.last_event_sequence;
        inner.current.last_heartbeat_age_ms = state.last_heartbeat_age_ms;
        inner.current.listener = listener;
        inner.current.safe_state_sink = sink;
        inner.current.ros2_publisher = publisher;
        // Re-project the bounded diagnostics on every update so
        // consumers reading the snapshot atomically see consistent
        // recent-transitions / recent-errors lists.
        inner.current.diagnostics = BoundedDiagnostics {
            capacity: usize_to_u32(self.diagnostics_capacity),
            recent_transitions: inner.transitions.iter().cloned().collect(),
            recent_errors: inner.errors.iter().cloned().collect(),
        };
    }

    /// V01-E12-F07: Update the V01-E12 diagnostics-sink, metrics, and
    /// control-loop projections. `control_loop` is `None` when the
    /// serving worker does not emit control-loop events for the
    /// current deployment.
    pub fn update_v12(
        &self,
        diagnostics_sink: DiagnosticsSinkStatus,
        metrics_export: MetricsExportStatus,
        control_loop: Option<ControlLoopStatus>,
    ) {
        #[allow(clippy::expect_used)]
        let mut inner = self.inner.lock().expect("snapshot writer poisoned");
        inner.current.diagnostics_sink = diagnostics_sink;
        inner.current.metrics_export = metrics_export;
        inner.current.control_loop = control_loop;
    }

    /// V01-E12-F02 / F03: Record the most-recent correlation id and
    /// (optional) failure reason on the snapshot. Operators use these
    /// to thread CLI output against logs/metrics.
    pub fn update_last_failure(
        &self,
        correlation_id: Option<String>,
        failure_reason: Option<FailureReason>,
    ) {
        #[allow(clippy::expect_used)]
        let mut inner = self.inner.lock().expect("snapshot writer poisoned");
        inner.current.last_correlation_id = correlation_id;
        inner.current.last_failure_reason = failure_reason;
    }

    /// Append a transition to the bounded diagnostics ring.
    pub fn record_transition(&self, transition: RecentTransition) {
        #[allow(clippy::expect_used)]
        let mut inner = self.inner.lock().expect("snapshot writer poisoned");
        let cap = self.diagnostics_capacity;
        if inner.transitions.len() == cap {
            inner.transitions.pop_front();
        }
        inner.transitions.push_back(transition);
    }

    /// Append an error to the bounded diagnostics ring.
    pub fn record_error(&self, error: RecentError) {
        #[allow(clippy::expect_used)]
        let mut inner = self.inner.lock().expect("snapshot writer poisoned");
        let cap = self.diagnostics_capacity;
        if inner.errors.len() == cap {
            inner.errors.pop_front();
        }
        inner.errors.push_back(error);
    }

    /// Return the most-recent snapshot. Consumers always read a
    /// complete record; the in-memory case clones; the file case is
    /// guaranteed by the atomic rename.
    pub fn current(&self) -> StatusSnapshot {
        #[allow(clippy::expect_used)]
        self.inner
            .lock()
            .expect("snapshot writer poisoned")
            .current
            .clone()
    }

    /// Persist the current snapshot to disk via atomic replace. Returns
    /// `Ok(())` when no path is configured (in-memory mode).
    ///
    /// # Errors
    ///
    /// Returns [`ObservabilityError::SnapshotSink`] for IO or
    /// serialization failures.
    pub fn flush(&self) -> ObservabilityResult<()> {
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        let snapshot = self.current();
        let body = serde_json::to_string_pretty(&snapshot)
            .map_err(|e| ObservabilityError::SnapshotSink(format!("serialize: {e}")))?;
        write_atomic(&path, body.as_bytes())
    }

    /// Capacity of the bounded diagnostics ring.
    pub fn diagnostics_capacity(&self) -> u32 {
        usize_to_u32(self.diagnostics_capacity)
    }
}

fn empty_snapshot(capacity: usize) -> StatusSnapshot {
    StatusSnapshot {
        schema_version: tensorplate_protocol::SCHEMA_VERSION.to_string(),
        observability_state: "ready".into(),
        agent_state: "unknown".into(),
        serving_state: "unknown".into(),
        active_deployment: String::new(),
        backend: String::new(),
        missed_heartbeat_count: 0,
        missed_deadline_rate: 0.0,
        queue_depth: 0,
        last_error_code: None,
        last_event_sequence: None,
        last_heartbeat_age_ms: None,
        safe_state_sink: SinkStatus::default(),
        ros2_publisher: PublisherStatus::default(),
        listener: ListenerStatus::default(),
        diagnostics: BoundedDiagnostics {
            capacity: usize_to_u32(capacity),
            recent_transitions: Vec::new(),
            recent_errors: Vec::new(),
        },
        diagnostics_sink: DiagnosticsSinkStatus::default(),
        metrics_export: MetricsExportStatus::default(),
        control_loop: None,
        last_correlation_id: None,
        last_failure_reason: None,
    }
}

fn write_atomic(path: &Path, body: &[u8]) -> ObservabilityResult<()> {
    let parent = path.parent().ok_or_else(|| {
        ObservabilityError::SnapshotSink(format!("no parent for {}", path.display()))
    })?;
    std::fs::create_dir_all(parent).map_err(|e| {
        ObservabilityError::SnapshotSink(format!("mkdir {}: {e}", parent.display()))
    })?;
    let tmp = path.with_extension("partial");
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .map_err(|e| {
                ObservabilityError::SnapshotSink(format!("open {}: {e}", tmp.display()))
            })?;
        f.write_all(body).map_err(|e| {
            ObservabilityError::SnapshotSink(format!("write {}: {e}", tmp.display()))
        })?;
        f.sync_all().map_err(|e| {
            ObservabilityError::SnapshotSink(format!("fsync {}: {e}", tmp.display()))
        })?;
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        ObservabilityError::SnapshotSink(format!(
            "rename {} -> {}: {e}",
            tmp.display(),
            path.display()
        ))
    })?;
    Ok(())
}

fn component_state_label(s: ComponentState) -> String {
    match s {
        ComponentState::Ready => "ready".into(),
        ComponentState::Degraded => "degraded".into(),
        ComponentState::Failed => "failed".into(),
        ComponentState::Unknown => "unknown".into(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{
        ControlLoopStatus, DiagnosticsSinkStatus, ListenerStatus, MetricsExportStatus,
        PublisherStatus, RecentError, RecentTransition, SinkStatus, SnapshotWriter, StatusSnapshot,
    };
    use crate::config::{StatusSnapshotConfig, StatusSnapshotKind};
    use crate::listener::ListenerCountersSnapshot;
    use crate::state::{AggregateState, ObservabilityState};
    use tensorplate_protocol::error::ErrorCode;
    use tensorplate_protocol::failure_reason::FailureReason;
    use tensorplate_protocol::worker_status::ComponentState;
    use tensorplate_protocol::ControlLoopSummary;

    fn state() -> AggregateState {
        AggregateState {
            state: ObservabilityState::Failed,
            previous_state: ObservabilityState::Ready,
            agent_state: ComponentState::Ready,
            serving_state: ComponentState::Failed,
            active_deployment: "deploy-1".into(),
            backend: "mock".into(),
            missed_heartbeat_count: 4,
            missed_deadline_rate: 0.0,
            queue_depth: 1,
            last_error_code: Some(ErrorCode::Internal),
            last_transition_at: None,
            last_event_sequence: Some(11),
            last_heartbeat_age_ms: Some(250),
            last_periodic_emit_at: None,
        }
    }

    #[test]
    fn in_memory_snapshot_round_trips() {
        let writer = SnapshotWriter::new(&StatusSnapshotConfig::default());
        let listener: ListenerStatus = ListenerCountersSnapshot {
            accepted: 5,
            ..ListenerCountersSnapshot::default()
        }
        .into();
        writer.update(
            &state(),
            listener,
            SinkStatus {
                enabled: true,
                dropped: 0,
                errors: 0,
            },
            PublisherStatus::default(),
        );
        let s = writer.current();
        assert_eq!(s.observability_state, "failed");
        assert_eq!(s.serving_state, "failed");
        assert_eq!(s.missed_heartbeat_count, 4);
        assert_eq!(s.last_error_code, Some(ErrorCode::Internal));
        assert_eq!(s.last_event_sequence, Some(11));
        assert_eq!(s.last_heartbeat_age_ms, Some(250));
        assert_eq!(s.diagnostics.capacity, 64);
    }

    #[test]
    fn diagnostics_ring_is_bounded() {
        let cfg = StatusSnapshotConfig {
            diagnostics_capacity: 3,
            ..StatusSnapshotConfig::default()
        };
        let writer = SnapshotWriter::new(&cfg);
        for i in 0..5 {
            writer.record_transition(RecentTransition {
                previous_state: "ready".into(),
                state: "degraded".into(),
                reason: "serving_degraded".into(),
                monotonic_age_ms: i,
            });
        }
        for i in 0..5 {
            writer.record_error(RecentError {
                component: "snapshot".into(),
                code: ErrorCode::Internal,
                message: format!("err{i}"),
            });
        }
        writer.update(
            &state(),
            ListenerStatus::default(),
            SinkStatus::default(),
            PublisherStatus::default(),
        );
        let s = writer.current();
        assert_eq!(s.diagnostics.recent_transitions.len(), 3);
        assert_eq!(s.diagnostics.recent_errors.len(), 3);
    }

    #[test]
    fn v12_projection_fields_round_trip_through_file_snapshot() {
        let tmpdir = tempfile::tempdir().expect("tempdir");
        let path = tmpdir.path().join("status.json");
        let writer = SnapshotWriter::new(&StatusSnapshotConfig {
            kind: StatusSnapshotKind::File,
            path: Some(path.clone()),
            diagnostics_capacity: 8,
        });
        writer.update(
            &state(),
            ListenerStatus::default(),
            SinkStatus::default(),
            PublisherStatus::default(),
        );
        let diagnostics_sink = DiagnosticsSinkStatus {
            enqueued: 12,
            dropped_queue_full: 1,
            file_rotations: 0,
            ..DiagnosticsSinkStatus::default()
        };
        let metrics_export = MetricsExportStatus {
            series_registered: 4,
            samples_recorded: 21,
            ..MetricsExportStatus::default()
        };
        let control = ControlLoopStatus::from_summary(
            30.0,
            60,
            "/v1/act",
            "vla",
            "smolvla-tiny",
            "tensorrt",
            &ControlLoopSummary {
                samples: 100,
                missed_deadlines: 2,
                missed_deadline_rate: 0.02,
                jitter_p50_ms: Some(0.5),
                jitter_p95_ms: Some(1.2),
                jitter_p99_ms: Some(2.0),
                jitter_max_ms: Some(3.1),
                mean_frequency_hz: Some(29.9),
                frequency_stddev_hz: Some(0.2),
                frequency_error_pct: Some(0.33),
            },
        );
        writer.update_v12(
            diagnostics_sink.clone(),
            metrics_export.clone(),
            Some(control.clone()),
        );
        writer.update_last_failure(
            Some("deploy-42".into()),
            Some(FailureReason::WorkerCrashLoop),
        );
        writer.flush().expect("flush");
        let body = std::fs::read_to_string(&path).expect("read");
        let parsed: StatusSnapshot = serde_json::from_str(&body).expect("parse");
        assert_eq!(parsed.diagnostics_sink, diagnostics_sink);
        assert_eq!(parsed.metrics_export, metrics_export);
        assert_eq!(parsed.control_loop.as_ref(), Some(&control));
        assert_eq!(parsed.last_correlation_id.as_deref(), Some("deploy-42"));
        assert_eq!(
            parsed.last_failure_reason,
            Some(FailureReason::WorkerCrashLoop)
        );
    }

    #[test]
    fn file_snapshot_atomic_replace_avoids_partial_reads() {
        let tmpdir = tempfile::tempdir().expect("tempdir");
        let path = tmpdir.path().join("status.json");
        let writer = SnapshotWriter::new(&StatusSnapshotConfig {
            kind: StatusSnapshotKind::File,
            path: Some(path.clone()),
            diagnostics_capacity: 8,
        });
        writer.update(
            &state(),
            ListenerStatus::default(),
            SinkStatus::default(),
            PublisherStatus::default(),
        );
        writer.flush().expect("flush");
        // Re-open: file is a complete, parseable record.
        let body = std::fs::read_to_string(&path).expect("read");
        let parsed: StatusSnapshot = serde_json::from_str(&body).expect("parse");
        assert_eq!(parsed.observability_state, "failed");
        // No `.partial` file lingers after flush.
        assert!(!path.with_extension("partial").exists());
    }
}
