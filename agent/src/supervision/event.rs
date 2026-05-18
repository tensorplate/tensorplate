// SPDX-License-Identifier: Apache-2.0
//
// V01-E09-F05: Bounded supervision-event emitter.
//
// Supervision events are produced at every supervisor state transition.
// Emission is non-blocking: when the configured queue is full the oldest
// pending event is dropped and a bounded drop counter is bumped. The
// supervisor never blocks restart, stop, or deploy progress on a slow or
// missing consumer.
//
// In v0.1.0 the only sink shipped is an in-memory ring buffer used by:
//   - the V01-E10 observability service (consumed in a future commit)
//   - V01-E11 status / log views
//   - failure-injection tests (V01-E09-F07)
//
// The emitter is composition-root-installable through [`SupervisionEventSink`].

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant;

use tensorplate_protocol::error::ErrorCode;
use tensorplate_protocol::supervision_event::{
    SupervisionAgentState, SupervisionEvent, SupervisionEventKind, SupervisionServingState,
};

use super::config::EventSinkConfig;
use super::policy::FailureClass;

/// Diagnostic payload assembled by the supervisor before it hands an
/// event to the sink. Implementations may serialize this directly or
/// project it to wire-form [`SupervisionEvent`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SupervisionEventPayload {
    pub kind: SupervisionEventKind,
    pub sequence: u64,
    pub timestamp: Instant,
    pub agent_state: SupervisionAgentState,
    pub serving_state: SupervisionServingState,
    pub active_deployment: String,
    pub backend: String,
    pub restart_count: u32,
    pub next_restart_delay_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<i32>,
    pub after_ready: Option<bool>,
    pub error_code: Option<ErrorCode>,
    pub message: Option<String>,
    pub failure_class: Option<FailureClass>,
}

impl SupervisionEventPayload {
    /// Convert the payload to the wire-level [`SupervisionEvent`]. The
    /// monotonic timestamp encodes nanoseconds since the supervisor's
    /// `boot_instant` (`anchor`).
    #[must_use]
    pub fn to_event(&self, anchor: Instant) -> SupervisionEvent {
        let mono_ns = u64::try_from(self.timestamp.saturating_duration_since(anchor).as_nanos())
            .unwrap_or(u64::MAX);
        let mut ev = SupervisionEvent::new(self.kind, self.sequence, mono_ns);
        ev.agent_state = Some(self.agent_state);
        ev.serving_state = Some(self.serving_state);
        ev.active_deployment.clone_from(&self.active_deployment);
        ev.backend.clone_from(&self.backend);
        ev.restart_count = u64::from(self.restart_count);
        ev.next_restart_delay_ms = self.next_restart_delay_ms;
        ev.exit_code = self.exit_code;
        ev.exit_signal = self.exit_signal;
        ev.after_ready = self.after_ready;
        ev.error_code = self.error_code;
        if let Some(msg) = self.message.as_ref() {
            ev = ev.with_message(msg.clone());
        }
        ev
    }
}

/// Sink trait for supervision events. Implementations must be `Send +
/// Sync` and MUST NOT block the caller.
pub trait SupervisionEventSink: Send + Sync {
    fn emit(&self, payload: &SupervisionEventPayload);
}

/// Bounded ring-buffer sink. The default sink installed by the
/// supervisor; the V01-E10 observability service drains it
/// asynchronously.
pub struct RingEventSink {
    capacity: usize,
    inner: Mutex<RingState>,
}

#[derive(Default)]
struct RingState {
    queue: VecDeque<SupervisionEventPayload>,
    dropped: u64,
}

impl RingEventSink {
    /// Build a sink with the supervisor's queue capacity. A capacity of
    /// zero is rejected at config validation; this constructor still
    /// clamps to 1 defensively.
    #[must_use]
    pub fn new(cfg: &EventSinkConfig) -> Self {
        let cap = cfg.queue_capacity.max(1) as usize;
        Self {
            capacity: cap,
            inner: Mutex::new(RingState::default()),
        }
    }

    /// Drain queued events for delivery by an async consumer. Tests use
    /// this to assert the emission order; the V01-E10 service uses it to
    /// publish events.
    #[allow(clippy::expect_used)]
    #[must_use]
    pub fn drain(&self) -> Vec<SupervisionEventPayload> {
        let mut state = self.inner.lock().expect("event sink poisoned");
        state.queue.drain(..).collect()
    }

    /// Read the count of events dropped because the queue was full.
    #[allow(clippy::expect_used)]
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.inner.lock().expect("event sink poisoned").dropped
    }

    /// Capacity the sink was configured with.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

impl SupervisionEventSink for RingEventSink {
    fn emit(&self, payload: &SupervisionEventPayload) {
        let Ok(mut state) = self.inner.lock() else {
            // Mutex poisoning means the previous holder panicked. We
            // deliberately swallow this so the supervisor never blocks
            // or panics on a slow consumer.
            return;
        };
        if state.queue.len() == self.capacity {
            state.queue.pop_front();
            state.dropped = state.dropped.saturating_add(1);
        }
        state.queue.push_back(payload.clone());
    }
}

/// No-op sink used by the recovery path and unit tests that do not care
/// about emitted events.
#[derive(Default)]
pub struct NoopEventSink;

impl SupervisionEventSink for NoopEventSink {
    fn emit(&self, _payload: &SupervisionEventPayload) {}
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::default_trait_access)]

    use super::super::config::EventSinkConfig;
    use super::{
        NoopEventSink, RingEventSink, SupervisionAgentState, SupervisionEventKind,
        SupervisionEventPayload, SupervisionEventSink, SupervisionServingState,
    };
    use std::time::Instant;

    fn payload(seq: u64) -> SupervisionEventPayload {
        SupervisionEventPayload {
            kind: SupervisionEventKind::WorkerStarted,
            sequence: seq,
            timestamp: Instant::now(),
            agent_state: SupervisionAgentState::Ready,
            serving_state: SupervisionServingState::Starting,
            active_deployment: "d-1".into(),
            backend: "mock".into(),
            restart_count: 0,
            next_restart_delay_ms: None,
            exit_code: None,
            exit_signal: None,
            after_ready: None,
            error_code: None,
            message: None,
            failure_class: None,
        }
    }

    #[test]
    fn ring_sink_drops_oldest_when_full() {
        let sink = RingEventSink::new(&EventSinkConfig {
            queue_capacity: 2,
            uds_path: None,
        });
        sink.emit(&payload(1));
        sink.emit(&payload(2));
        sink.emit(&payload(3));
        let drained = sink.drain();
        let seqs: Vec<u64> = drained.iter().map(|p| p.sequence).collect();
        assert_eq!(seqs, vec![2, 3]);
        assert_eq!(sink.dropped(), 1);
    }

    #[test]
    fn ring_sink_preserves_insertion_order() {
        let sink = RingEventSink::new(&EventSinkConfig::default());
        for i in 1..=4 {
            sink.emit(&payload(i));
        }
        let drained = sink.drain();
        let seqs: Vec<u64> = drained.iter().map(|p| p.sequence).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4]);
    }

    #[test]
    fn noop_sink_is_silent() {
        let sink = NoopEventSink;
        sink.emit(&payload(1));
    }

    #[test]
    fn payload_to_event_anchors_to_instant() {
        let anchor = Instant::now();
        let mut p = payload(7);
        p.timestamp = anchor + std::time::Duration::from_millis(5);
        let ev = p.to_event(anchor);
        assert!(ev.monotonic_timestamp_ns >= 5_000_000);
        assert_eq!(ev.sequence, 7);
    }
}
