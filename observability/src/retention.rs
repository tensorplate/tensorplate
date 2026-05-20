// SPDX-License-Identifier: Apache-2.0
//
#![allow(clippy::cast_possible_truncation)]

// V01-E12-F06: Bounded diagnostics retention and non-blocking sinks.
//
// The retention module owns the bounded log queue, file rotation, and
// redaction guard for v0.1.0. Producers (agent, serving worker,
// observability, sidecar adapter) enqueue [`LogEvent`] payloads
// through the [`crate::log_emitter::LogEmitter`]; this module is the
// sink side. The serving path never blocks on a slow sink: a full
// queue drops the oldest entry and increments a bounded counter.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use tensorplate_protocol::{LogEvent, ValidatePayload};

use crate::error::{ObservabilityError, ObservabilityResult};

/// Maximum bytes the bounded queue retains before applying the drop
/// policy. Producers exceeding this cap have older entries evicted; a
/// counter records the eviction so the operator sees it through the
/// status projection.
pub const MAX_QUEUE_CAPACITY: usize = 8_192;

/// Drop policy applied when the bounded queue is full or the sink
/// fails. The default is `DropOldest`, mirroring the safe-state sink
/// pattern: the producer always wins, the sink absorbs the loss.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionDropPolicy {
    /// Drop the oldest event when the queue is full.
    #[default]
    DropOldest,
    /// Drop the incoming event when the queue is full.
    DropIncoming,
}

/// Diagnostics retention config.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RetentionConfig {
    /// Maximum number of log events buffered for the bounded queue.
    /// Capped at [`MAX_QUEUE_CAPACITY`] regardless of the input.
    #[serde(default = "default_queue_capacity")]
    pub queue_capacity: u32,
    /// Drop policy. See [`RetentionDropPolicy`].
    #[serde(default)]
    pub drop_policy: RetentionDropPolicy,
    /// Optional file path. When set, retained events are appended as
    /// JSON lines. The file is rotated when its size exceeds
    /// `rotate_bytes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<PathBuf>,
    /// Rotation threshold in bytes. Files at or above this size are
    /// renamed to `<file>.1` (overwriting the previous rotation) before
    /// a new write opens a fresh file.
    #[serde(default = "default_rotate_bytes")]
    pub rotate_bytes: u64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            queue_capacity: default_queue_capacity(),
            drop_policy: RetentionDropPolicy::default(),
            file_path: None,
            rotate_bytes: default_rotate_bytes(),
        }
    }
}

const fn default_queue_capacity() -> u32 {
    1_024
}
const fn default_rotate_bytes() -> u64 {
    1024 * 1024 // 1 MiB
}

/// Bounded counters surfaced through the snapshot.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RetentionCounters {
    /// Events enqueued through [`DiagnosticsRetention::enqueue`].
    pub enqueued: u64,
    /// Events dropped because the queue was full.
    pub dropped_queue_full: u64,
    /// Events dropped because the producer violated the bounded-context
    /// or redaction rules. Inputs are dropped at the emitter, not here,
    /// but the sink defends against direct callers as well.
    pub dropped_redacted: u64,
    /// File rotations performed.
    pub file_rotations: u64,
    /// File write errors observed.
    pub file_write_errors: u64,
    /// Drain calls that successfully shipped at least one event.
    pub drains: u64,
}

/// V01-E12-F06 retention store.
pub struct DiagnosticsRetention {
    config: RetentionConfig,
    state: Mutex<RetentionState>,
}

struct RetentionState {
    queue: VecDeque<LogEvent>,
    counters: RetentionCounters,
    file_written: u64,
}

impl DiagnosticsRetention {
    #[must_use]
    pub fn new(config: RetentionConfig) -> Self {
        let cap = config
            .queue_capacity
            .min(u32::try_from(MAX_QUEUE_CAPACITY).unwrap_or(u32::MAX)) as usize;
        Self {
            config,
            state: Mutex::new(RetentionState {
                queue: VecDeque::with_capacity(cap),
                counters: RetentionCounters::default(),
                file_written: 0,
            }),
        }
    }

    /// Counters snapshot for the status projection.
    pub fn counters(&self) -> RetentionCounters {
        #[allow(clippy::expect_used)]
        self.state
            .lock()
            .expect("retention poisoned")
            .counters
            .clone()
    }

    /// Number of events currently buffered.
    pub fn buffered(&self) -> usize {
        #[allow(clippy::expect_used)]
        self.state.lock().expect("retention poisoned").queue.len()
    }

    /// Enqueue a log event. Returns `true` when the event was buffered,
    /// `false` when it was dropped under the configured policy. The
    /// producer never blocks; counters bump regardless.
    pub fn enqueue(&self, event: LogEvent) -> bool {
        let Ok(event) = event.validate_payload() else {
            if let Ok(mut state) = self.state.lock() {
                state.counters.dropped_redacted = state.counters.dropped_redacted.saturating_add(1);
            }
            return false;
        };
        #[allow(clippy::expect_used)]
        let mut state = self.state.lock().expect("retention poisoned");
        state.counters.enqueued += 1;
        let cap = self.effective_capacity();
        if state.queue.len() < cap {
            state.queue.push_back(event);
            return true;
        }
        state.counters.dropped_queue_full += 1;
        match self.config.drop_policy {
            RetentionDropPolicy::DropOldest => {
                state.queue.pop_front();
                state.queue.push_back(event);
                true
            }
            RetentionDropPolicy::DropIncoming => false,
        }
    }

    /// Drain the queue, returning the buffered events in arrival order.
    pub fn drain(&self) -> Vec<LogEvent> {
        #[allow(clippy::expect_used)]
        let mut state = self.state.lock().expect("retention poisoned");
        let events: Vec<LogEvent> = state.queue.drain(..).collect();
        if !events.is_empty() {
            state.counters.drains += 1;
        }
        events
    }

    /// Flush the queue to the configured file sink (if any). The file
    /// is rotated to `<file>.1` when the post-write size would exceed
    /// `rotate_bytes`.
    ///
    /// # Errors
    ///
    /// [`ObservabilityError::SnapshotSink`] when the file cannot be
    /// opened or written. The counter bumps before the error returns.
    pub fn flush_to_file(&self) -> ObservabilityResult<()> {
        let Some(path) = self.config.file_path.clone() else {
            return Ok(());
        };
        let events = self.drain();
        if events.is_empty() {
            return Ok(());
        }
        let mut body = Vec::with_capacity(events.len() * 128);
        for event in &events {
            let line = serde_json::to_vec(event).map_err(|e| {
                ObservabilityError::SnapshotSink(format!("serialise log event: {e}"))
            })?;
            body.extend_from_slice(&line);
            body.push(b'\n');
        }
        // Rotation check: rotate if the projected post-write size would
        // exceed the threshold.
        let existing_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let projected = existing_size.saturating_add(body.len() as u64);
        if projected > self.config.rotate_bytes {
            self.rotate(&path)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| {
                self.bump_file_write_error();
                ObservabilityError::SnapshotSink(format!("open {}: {e}", path.display()))
            })?;
        if let Err(e) = file.write_all(&body) {
            self.bump_file_write_error();
            return Err(ObservabilityError::SnapshotSink(format!(
                "write log lines: {e}"
            )));
        }
        #[allow(clippy::expect_used)]
        {
            let mut state = self.state.lock().expect("retention poisoned");
            state.file_written = state.file_written.saturating_add(body.len() as u64);
        }
        Ok(())
    }

    fn effective_capacity(&self) -> usize {
        let raw = self.config.queue_capacity as usize;
        raw.min(MAX_QUEUE_CAPACITY).max(1)
    }

    fn rotate(&self, path: &std::path::Path) -> ObservabilityResult<()> {
        let rotated = path.with_extension("1");
        // Move the existing file to `<file>.1`, replacing any previous
        // rotation. Missing-source is fine; the next write opens a new
        // file.
        if std::fs::metadata(path).is_ok() {
            std::fs::rename(path, &rotated).map_err(|e| {
                ObservabilityError::SnapshotSink(format!(
                    "rotate {} -> {}: {e}",
                    path.display(),
                    rotated.display()
                ))
            })?;
        }
        #[allow(clippy::expect_used)]
        {
            self.state
                .lock()
                .expect("retention poisoned")
                .counters
                .file_rotations += 1;
        }
        Ok(())
    }

    fn bump_file_write_error(&self) {
        #[allow(clippy::expect_used)]
        let mut state = self.state.lock().expect("retention poisoned");
        state.counters.file_write_errors += 1;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{DiagnosticsRetention, RetentionConfig, RetentionDropPolicy};
    use std::collections::BTreeMap;
    use tensorplate_protocol::{LogComponent, LogContextValue, LogEvent, LogLevel};

    fn make_event(seq: u64) -> LogEvent {
        LogEvent::new(
            LogComponent::ServingWorker,
            "request.accepted",
            LogLevel::Info,
            seq,
        )
    }

    #[test]
    fn buffered_events_drain_in_order() {
        let retention = DiagnosticsRetention::new(RetentionConfig::default());
        for i in 0..3 {
            retention.enqueue(make_event(i));
        }
        let drained = retention.drain();
        assert_eq!(drained.len(), 3);
        assert_eq!(drained[0].monotonic_timestamp_ns, 0);
        assert_eq!(drained[2].monotonic_timestamp_ns, 2);
        assert_eq!(retention.counters().enqueued, 3);
        assert_eq!(retention.counters().drains, 1);
    }

    #[test]
    fn drop_oldest_keeps_most_recent_under_pressure() {
        let config = RetentionConfig {
            queue_capacity: 2,
            ..RetentionConfig::default()
        };
        let retention = DiagnosticsRetention::new(config);
        for i in 0..5 {
            retention.enqueue(make_event(i));
        }
        let drained = retention.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].monotonic_timestamp_ns, 3);
        assert_eq!(drained[1].monotonic_timestamp_ns, 4);
        assert_eq!(retention.counters().dropped_queue_full, 3);
    }

    #[test]
    fn drop_incoming_keeps_existing_under_pressure() {
        let config = RetentionConfig {
            queue_capacity: 2,
            drop_policy: RetentionDropPolicy::DropIncoming,
            ..RetentionConfig::default()
        };
        let retention = DiagnosticsRetention::new(config);
        retention.enqueue(make_event(0));
        retention.enqueue(make_event(1));
        assert!(!retention.enqueue(make_event(2)));
        let drained = retention.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].monotonic_timestamp_ns, 0);
    }

    #[test]
    fn direct_invalid_event_is_dropped_and_counted() {
        let retention = DiagnosticsRetention::new(RetentionConfig::default());
        let mut context = BTreeMap::new();
        context.insert("bad key".into(), LogContextValue::String("v".into()));
        let event = LogEvent {
            context,
            ..make_event(1)
        };
        assert!(!retention.enqueue(event));
        assert_eq!(retention.counters().dropped_redacted, 1);
        assert!(retention.drain().is_empty());
    }

    #[test]
    fn file_sink_writes_json_lines() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("logs.jsonl");
        let config = RetentionConfig {
            file_path: Some(path.clone()),
            ..RetentionConfig::default()
        };
        let retention = DiagnosticsRetention::new(config);
        for i in 0..4 {
            retention.enqueue(make_event(i));
        }
        retention.flush_to_file().expect("flush");
        let body = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(retention.buffered() == 0);
    }

    #[test]
    fn rotation_runs_when_threshold_exceeded() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("logs.jsonl");
        let config = RetentionConfig {
            file_path: Some(path.clone()),
            rotate_bytes: 32, // tiny so the first flush rotates
            ..RetentionConfig::default()
        };
        let retention = DiagnosticsRetention::new(config);
        for i in 0..4 {
            retention.enqueue(make_event(i));
        }
        retention.flush_to_file().expect("flush");
        for i in 0..4 {
            retention.enqueue(make_event(i + 10));
        }
        retention.flush_to_file().expect("flush");
        assert!(retention.counters().file_rotations >= 1);
        let rotated = path.with_extension("1");
        assert!(rotated.exists(), "rotated file should exist");
    }
}
