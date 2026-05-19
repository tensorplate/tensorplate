// SPDX-License-Identifier: Apache-2.0
//
// V01-E10-F04: Local safe-state sinks.
//
// The aggregator hands every safe-state event to a [`SafeStateSink`].
// Sinks MUST NOT block the aggregator on a slow consumer:
//
//   - the in-memory ring keeps the most recent `capacity` events and
//     bumps a typed drop counter when the queue is full,
//   - the file sink appends JSON lines; failures bump a typed counter
//     and surface through the bounded diagnostics store.
//
// The aggregator never panics on a missing sink; tests can install a
// `NoopSafeStateSink` to assert behaviour without observing event
// payloads.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use tensorplate_protocol::error::ErrorCode;

use crate::config::{SafeStateSinkConfig, SafeStateSinkKind};
use crate::state::{ObservabilityState, SafeStateEvent, SafeStateReason};

/// Wire-form safe-state event the file sink writes and the in-memory
/// sink exposes to tests.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireSafeStateEvent {
    pub schema_version: String,
    pub state: String,
    pub previous_state: String,
    pub reason: String,
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
    pub monotonic_age_ms: u64,
}

impl From<&SafeStateEvent> for WireSafeStateEvent {
    fn from(event: &SafeStateEvent) -> Self {
        Self {
            schema_version: tensorplate_protocol::SCHEMA_VERSION.to_string(),
            state: event.state.as_str().to_string(),
            previous_state: event.previous_state.as_str().to_string(),
            reason: event.reason.as_str().to_string(),
            agent_state: component_state_label(event.agent_state),
            serving_state: component_state_label(event.serving_state),
            active_deployment: event.active_deployment.clone(),
            backend: event.backend.clone(),
            missed_heartbeat_count: event.missed_heartbeat_count,
            missed_deadline_rate: event.missed_deadline_rate,
            queue_depth: event.queue_depth,
            last_error_code: event.last_error_code,
            monotonic_age_ms: event.monotonic_age_ms,
        }
    }
}

fn component_state_label(state: tensorplate_protocol::worker_status::ComponentState) -> String {
    match state {
        tensorplate_protocol::worker_status::ComponentState::Ready => "ready".into(),
        tensorplate_protocol::worker_status::ComponentState::Degraded => "degraded".into(),
        tensorplate_protocol::worker_status::ComponentState::Failed => "failed".into(),
        tensorplate_protocol::worker_status::ComponentState::Unknown => "unknown".into(),
    }
}

/// Sink trait for safe-state events. Implementations are `Send + Sync`
/// and MUST NOT block.
pub trait SafeStateSink: Send + Sync {
    fn emit(&self, event: &SafeStateEvent);
    /// Number of events that were dropped because the sink could not
    /// keep up. Used by the diagnostics store.
    fn dropped(&self) -> u64;
    /// Number of write errors recorded by the sink (file IO failures,
    /// etc.). Used by the diagnostics store.
    fn errors(&self) -> u64;
}

/// In-memory ring buffer sink. Composition root's default.
pub struct InMemorySafeStateSink {
    capacity: usize,
    inner: Mutex<RingState>,
    dropped: AtomicU64,
}

#[derive(Default)]
struct RingState {
    queue: VecDeque<SafeStateEvent>,
}

impl InMemorySafeStateSink {
    #[must_use]
    pub fn new(cfg: &SafeStateSinkConfig) -> Self {
        let cap = cfg.queue_capacity.max(1) as usize;
        Self {
            capacity: cap,
            inner: Mutex::new(RingState::default()),
            dropped: AtomicU64::new(0),
        }
    }

    /// Drain queued events for delivery by an async consumer.
    #[allow(clippy::expect_used)]
    pub fn drain(&self) -> Vec<SafeStateEvent> {
        self.inner
            .lock()
            .expect("in-memory safe-state sink poisoned")
            .queue
            .drain(..)
            .collect()
    }

    /// Number of events currently buffered.
    pub fn pending_len(&self) -> usize {
        #[allow(clippy::expect_used)]
        self.inner
            .lock()
            .expect("in-memory safe-state sink poisoned")
            .queue
            .len()
    }
}

impl SafeStateSink for InMemorySafeStateSink {
    fn emit(&self, event: &SafeStateEvent) {
        let Ok(mut state) = self.inner.lock() else {
            return;
        };
        if state.queue.len() == self.capacity {
            state.queue.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        state.queue.push_back(event.clone());
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn errors(&self) -> u64 {
        0
    }
}

/// File-backed sink. Appends JSON lines; failures are counted and
/// never block emission.
pub struct FileSafeStateSink {
    path: PathBuf,
    dropped: AtomicU64,
    errors: AtomicU64,
}

impl FileSafeStateSink {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            dropped: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        }
    }
}

impl SafeStateSink for FileSafeStateSink {
    fn emit(&self, event: &SafeStateEvent) {
        let wire = WireSafeStateEvent::from(event);
        let Ok(line) = serde_json::to_string(&wire) else {
            self.errors.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let Ok(mut opened) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        else {
            self.errors.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if writeln!(opened, "{line}").is_err() {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }
}

/// No-op sink used by tests that only inspect aggregator state.
#[derive(Default)]
pub struct NoopSafeStateSink;

impl SafeStateSink for NoopSafeStateSink {
    fn emit(&self, _event: &SafeStateEvent) {}
    fn dropped(&self) -> u64 {
        0
    }
    fn errors(&self) -> u64 {
        0
    }
}

/// Build the sink instance described by `cfg`. The composition root
/// uses this so test fixtures can override individual fields without
/// reimplementing the dispatch.
pub fn sink_from_config(cfg: &SafeStateSinkConfig) -> Box<dyn SafeStateSink> {
    match cfg.kind {
        SafeStateSinkKind::InMemory => Box::new(InMemorySafeStateSink::new(cfg)),
        SafeStateSinkKind::File => {
            let path = cfg
                .path
                .clone()
                .unwrap_or_else(|| PathBuf::from("/tmp/tensorplate-observability.jsonl"));
            Box::new(FileSafeStateSink::new(path))
        }
    }
}

/// Convenience: serialize the safe-state event as a JSON value for
/// snapshot assertions.
#[must_use]
pub fn to_json_value(event: &SafeStateEvent) -> Value {
    serde_json::to_value(WireSafeStateEvent::from(event)).unwrap_or(Value::Null)
}

/// Convenience accessor for the V01-E10 wire shape used by the safe-
/// state event without exposing internal types.
#[must_use]
pub fn to_wire(event: &SafeStateEvent) -> WireSafeStateEvent {
    WireSafeStateEvent::from(event)
}

#[allow(dead_code)]
#[must_use]
pub fn observability_state_label(s: ObservabilityState) -> &'static str {
    s.as_str()
}

#[allow(dead_code)]
#[must_use]
pub fn safe_state_reason_label(r: SafeStateReason) -> &'static str {
    r.as_str()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{
        FileSafeStateSink, InMemorySafeStateSink, NoopSafeStateSink, SafeStateSink,
        WireSafeStateEvent,
    };
    use crate::config::SafeStateSinkConfig;
    use crate::state::{ObservabilityState, SafeStateEvent, SafeStateReason};
    use tensorplate_protocol::worker_status::ComponentState;

    fn event() -> SafeStateEvent {
        SafeStateEvent {
            state: ObservabilityState::Failed,
            previous_state: ObservabilityState::Ready,
            reason: SafeStateReason::ServingFailed,
            agent_state: ComponentState::Ready,
            serving_state: ComponentState::Failed,
            active_deployment: "deploy-1".into(),
            backend: "mock".into(),
            missed_heartbeat_count: 0,
            missed_deadline_rate: 0.0,
            queue_depth: 0,
            last_error_code: None,
            monotonic_age_ms: 5,
        }
    }

    #[test]
    fn in_memory_sink_records_event() {
        let cfg = SafeStateSinkConfig::default();
        let sink = InMemorySafeStateSink::new(&cfg);
        sink.emit(&event());
        assert_eq!(sink.pending_len(), 1);
        let drained = sink.drain();
        assert_eq!(drained.len(), 1);
    }

    #[test]
    fn in_memory_sink_drops_oldest_when_full() {
        let cfg = SafeStateSinkConfig {
            queue_capacity: 2,
            ..SafeStateSinkConfig::default()
        };
        let sink = InMemorySafeStateSink::new(&cfg);
        for _ in 0..5 {
            sink.emit(&event());
        }
        assert_eq!(sink.pending_len(), 2);
        assert_eq!(sink.dropped(), 3);
    }

    #[test]
    fn file_sink_appends_json_lines() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let sink = FileSafeStateSink::new(tmp.path().to_path_buf());
        sink.emit(&event());
        sink.emit(&event());
        let body = std::fs::read_to_string(tmp.path()).expect("read");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        let _decoded: WireSafeStateEvent = serde_json::from_str(lines[0]).expect("json");
        assert_eq!(sink.errors(), 0);
    }

    #[test]
    fn file_sink_records_io_errors() {
        let sink = FileSafeStateSink::new(std::path::PathBuf::from(
            "/this/path/should/not/exist/safe.jsonl",
        ));
        sink.emit(&event());
        assert!(sink.errors() >= 1);
    }

    #[test]
    fn noop_sink_is_silent() {
        NoopSafeStateSink.emit(&event());
    }

    #[test]
    fn wire_event_includes_required_fields() {
        let wire = WireSafeStateEvent::from(&event());
        assert_eq!(wire.state, "failed");
        assert_eq!(wire.previous_state, "ready");
        assert_eq!(wire.reason, "serving_failed");
        assert_eq!(wire.agent_state, "ready");
        assert_eq!(wire.serving_state, "failed");
        assert_eq!(wire.schema_version, tensorplate_protocol::SCHEMA_VERSION);
    }
}
