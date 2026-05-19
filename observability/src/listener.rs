// SPDX-License-Identifier: Apache-2.0
//
// V01-E10-F02: Bounded local event listener and normalised health input.
//
// The listener is the single ingestion point for the observability
// service. It accepts:
//
//   - serving-worker heartbeat / state events as `HealthEvent`
//     payloads (`protocol/schemas/health_event.json`),
//   - agent supervision events as `SupervisionEvent` payloads
//     (`protocol/schemas/supervision_event.json`),
//
// and normalises both into a single internal [`HealthInput`] type that
// the heartbeat evaluator and the state aggregator consume.
//
// The listener is bounded: when the configured queue is full the
// oldest pending input is dropped and the [`ListenerCounters::dropped`]
// counter is bumped. The supervisor / serving worker never blocks on a
// slow observability consumer.
//
// V01-E10 ships the in-process channel (`InProcess` transport). The
// Unix-domain-socket path is reserved in config so the V01-E07 external
// heartbeat producer can wire it up without a schema bump.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tensorplate_protocol::error::ErrorCode;
use tensorplate_protocol::health_event::HealthEventKind;
use tensorplate_protocol::supervision_event::{
    SupervisionAgentState, SupervisionEventKind, SupervisionServingState,
};
use tensorplate_protocol::worker_status::ComponentState;
use tensorplate_protocol::{
    decode_with_version_check, DecodeError, HealthEvent, SupervisionEvent, SCHEMA_VERSION,
};

use crate::clock::MonotonicClock;
use crate::config::ListenerConfig;
use crate::error::{ObservabilityError, ObservabilityResult};

/// Source of a normalised health input. Lets the evaluator distinguish
/// agent supervision signals from in-band serving-worker heartbeats so
/// missing-heartbeat detection works without agent cooperation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum InputSource {
    /// `serving_worker` heartbeat / state event.
    ServingWorker,
    /// `agent` supervision event.
    AgentSupervisor,
    /// Test fixtures or internal periodic emitters.
    Internal,
}

impl InputSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            InputSource::ServingWorker => "serving_worker",
            InputSource::AgentSupervisor => "agent_supervisor",
            InputSource::Internal => "internal",
        }
    }
}

/// Discrete normalised event kind. The evaluator and aggregator only see
/// this shape so they don't need to special-case wire formats.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum InputKind {
    Heartbeat,
    Ready,
    Degraded,
    Failed,
    MissedDeadline,
    Overload,
    WorkerExit,
    WorkerNotReady,
    CrashLoop,
    Recovery,
}

impl InputKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            InputKind::Heartbeat => "heartbeat",
            InputKind::Ready => "ready",
            InputKind::Degraded => "degraded",
            InputKind::Failed => "failed",
            InputKind::MissedDeadline => "missed_deadline",
            InputKind::Overload => "overload",
            InputKind::WorkerExit => "worker_exit",
            InputKind::WorkerNotReady => "worker_not_ready",
            InputKind::CrashLoop => "crash_loop",
            InputKind::Recovery => "recovery",
        }
    }
}

/// Normalised input the evaluator and aggregator consume.
#[derive(Clone, Debug)]
pub struct HealthInput {
    pub source: InputSource,
    pub kind: InputKind,
    /// Monitor-side monotonic timestamp recorded when the listener
    /// accepted the event. Always set from the configured
    /// [`MonotonicClock`] so test fixtures get deterministic timing.
    pub received_at: Instant,
    /// Optional sequence number from the producer. The supervision
    /// event stream carries this; the serving worker's heartbeat
    /// stream does not.
    pub sequence: Option<u64>,
    /// Optional agent run-state from a supervision event.
    pub agent_state: Option<ComponentState>,
    /// Optional serving run-state from a supervision or health event.
    pub serving_state: Option<ComponentState>,
    pub active_deployment: String,
    pub backend: String,
    pub queue_depth: u64,
    pub missed_deadline_rate: f64,
    pub error_code: Option<ErrorCode>,
}

impl HealthInput {
    /// Build a synthetic heartbeat input. Used by the in-process
    /// minimal serving-worker heartbeat source (V01-E10-F02) when no
    /// external producer is wired up.
    pub fn heartbeat(source: InputSource, received_at: Instant) -> Self {
        Self {
            source,
            kind: InputKind::Heartbeat,
            received_at,
            sequence: None,
            agent_state: None,
            serving_state: None,
            active_deployment: String::new(),
            backend: String::new(),
            queue_depth: 0,
            missed_deadline_rate: 0.0,
            error_code: None,
        }
    }
}

/// Counters surfaced through the bounded diagnostics store.
#[derive(Debug, Default)]
pub struct ListenerCounters {
    pub accepted: AtomicU64,
    pub dropped: AtomicU64,
    pub malformed: AtomicU64,
    pub duplicates: AtomicU64,
    pub out_of_order: AtomicU64,
    pub unknown_version: AtomicU64,
}

impl ListenerCounters {
    #[must_use]
    pub fn snapshot(&self) -> ListenerCountersSnapshot {
        ListenerCountersSnapshot {
            accepted: self.accepted.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            malformed: self.malformed.load(Ordering::Relaxed),
            duplicates: self.duplicates.load(Ordering::Relaxed),
            out_of_order: self.out_of_order.load(Ordering::Relaxed),
            unknown_version: self.unknown_version.load(Ordering::Relaxed),
        }
    }
}

/// Cheap copy of the listener counters captured by the snapshot writer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ListenerCountersSnapshot {
    pub accepted: u64,
    pub dropped: u64,
    pub malformed: u64,
    pub duplicates: u64,
    pub out_of_order: u64,
    pub unknown_version: u64,
}

/// Bounded local listener. Producers push events via the typed
/// `submit_*` helpers; consumers drain inputs through `try_drain`.
///
/// The listener never spawns a thread on its own. The composition root
/// drives draining inside the service's tick loop so tests can drive
/// the whole pipeline deterministically.
pub struct EventListener {
    capacity: usize,
    queue: Mutex<VecDeque<HealthInput>>,
    last_sequence: Mutex<u64>,
    pub counters: Arc<ListenerCounters>,
    clock: Arc<dyn MonotonicClock>,
}

impl EventListener {
    #[must_use]
    pub fn new(cfg: &ListenerConfig, clock: Arc<dyn MonotonicClock>) -> Self {
        let cap = cfg.queue_capacity.max(1) as usize;
        Self {
            capacity: cap,
            queue: Mutex::new(VecDeque::with_capacity(cap)),
            last_sequence: Mutex::new(0),
            counters: Arc::new(ListenerCounters::default()),
            clock,
        }
    }

    /// Submit a wire-form [`HealthEvent`]. Used by the serving worker's
    /// in-process heartbeat source and by integration tests.
    ///
    /// # Errors
    ///
    /// Returns [`ObservabilityError::InvalidEvent`] for unknown schema
    /// versions; malformed payloads are tallied through the bounded
    /// counters and surfaced through the diagnostics store.
    pub fn submit_health_event(&self, event: &HealthEvent) -> ObservabilityResult<()> {
        if event.schema_version != SCHEMA_VERSION {
            self.counters
                .unknown_version
                .fetch_add(1, Ordering::Relaxed);
            return Err(ObservabilityError::InvalidEvent(format!(
                "unsupported HealthEvent.schema_version `{}` (expected `{}`)",
                event.schema_version, SCHEMA_VERSION
            )));
        }
        let kind = match event.kind {
            HealthEventKind::Heartbeat => InputKind::Heartbeat,
            HealthEventKind::Ready => InputKind::Ready,
            HealthEventKind::Degraded => InputKind::Degraded,
            HealthEventKind::Failed | HealthEventKind::NoHeartbeat => InputKind::Failed,
            HealthEventKind::MissedDeadline => InputKind::MissedDeadline,
            HealthEventKind::Overload => InputKind::Overload,
        };
        let serving_state = match event.kind {
            HealthEventKind::Ready => Some(ComponentState::Ready),
            HealthEventKind::Degraded | HealthEventKind::Overload => Some(ComponentState::Degraded),
            HealthEventKind::Failed | HealthEventKind::NoHeartbeat => Some(ComponentState::Failed),
            _ => None,
        };
        let input = HealthInput {
            source: InputSource::ServingWorker,
            kind,
            received_at: self.clock.now(),
            sequence: None,
            agent_state: None,
            serving_state,
            active_deployment: String::new(),
            backend: String::new(),
            queue_depth: 0,
            missed_deadline_rate: 0.0,
            error_code: event.error_code,
        };
        self.push(input);
        Ok(())
    }

    /// Submit a wire-form [`SupervisionEvent`] produced by the agent's
    /// V01-E09 supervisor. Out-of-order or duplicate sequences are
    /// tallied through bounded counters; the listener still accepts the
    /// event so downstream state remains responsive.
    ///
    /// # Errors
    ///
    /// Returns [`ObservabilityError::InvalidEvent`] for unknown schema
    /// versions.
    pub fn submit_supervision_event(&self, event: &SupervisionEvent) -> ObservabilityResult<()> {
        if event.schema_version != SCHEMA_VERSION {
            self.counters
                .unknown_version
                .fetch_add(1, Ordering::Relaxed);
            return Err(ObservabilityError::InvalidEvent(format!(
                "unsupported SupervisionEvent.schema_version `{}` (expected `{}`)",
                event.schema_version, SCHEMA_VERSION
            )));
        }
        {
            #[allow(clippy::expect_used)]
            let mut last = self
                .last_sequence
                .lock()
                .expect("listener sequence mutex poisoned");
            if event.sequence == *last && event.sequence != 0 {
                self.counters.duplicates.fetch_add(1, Ordering::Relaxed);
            } else if event.sequence < *last {
                self.counters.out_of_order.fetch_add(1, Ordering::Relaxed);
            } else {
                *last = event.sequence;
            }
        }
        let input = HealthInput {
            source: InputSource::AgentSupervisor,
            kind: classify_supervision(event.kind),
            received_at: self.clock.now(),
            sequence: Some(event.sequence),
            agent_state: event.agent_state.map(map_agent_state),
            serving_state: event.serving_state.map(map_serving_state),
            active_deployment: event.active_deployment.clone(),
            backend: event.backend.clone(),
            queue_depth: 0,
            missed_deadline_rate: 0.0,
            error_code: event.error_code,
        };
        self.push(input);
        Ok(())
    }

    /// Submit a JSON-encoded payload from an external producer. The
    /// payload must declare its `kind`-bearing top-level shape — the
    /// listener tries the `HealthEvent` decoder first and falls back to
    /// `SupervisionEvent`.
    ///
    /// # Errors
    ///
    /// Returns [`ObservabilityError::InvalidEvent`] if neither decoder
    /// accepts the payload, after bumping the malformed or
    /// unknown-version counter as appropriate.
    pub fn submit_json(&self, raw: &str) -> ObservabilityResult<()> {
        match decode_with_version_check::<HealthEvent>(raw) {
            Ok(event) => return self.submit_health_event(&event),
            Err(DecodeError::UnsupportedSchemaVersion { got, expected }) => {
                self.counters
                    .unknown_version
                    .fetch_add(1, Ordering::Relaxed);
                return Err(ObservabilityError::InvalidEvent(format!(
                    "unsupported schema_version `{got}` (expected `{expected}`)"
                )));
            }
            Err(_) => {}
        }
        match decode_with_version_check::<SupervisionEvent>(raw) {
            Ok(event) => self.submit_supervision_event(&event),
            Err(DecodeError::UnsupportedSchemaVersion { got, expected }) => {
                self.counters
                    .unknown_version
                    .fetch_add(1, Ordering::Relaxed);
                Err(ObservabilityError::InvalidEvent(format!(
                    "unsupported schema_version `{got}` (expected `{expected}`)"
                )))
            }
            Err(err) => {
                self.counters.malformed.fetch_add(1, Ordering::Relaxed);
                Err(ObservabilityError::InvalidEvent(format!(
                    "malformed event payload: {err}"
                )))
            }
        }
    }

    /// Push a pre-built normalised input. Used by the in-process
    /// minimal heartbeat source when there is no wire-form payload to
    /// validate.
    pub fn submit_input(&self, input: HealthInput) {
        self.push(input);
    }

    fn push(&self, input: HealthInput) {
        #[allow(clippy::expect_used)]
        let mut queue = self.queue.lock().expect("listener queue poisoned");
        if queue.len() == self.capacity {
            queue.pop_front();
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
        }
        queue.push_back(input);
        self.counters.accepted.fetch_add(1, Ordering::Relaxed);
    }

    /// Drain all queued inputs. The aggregator calls this once per
    /// tick.
    pub fn try_drain(&self) -> Vec<HealthInput> {
        #[allow(clippy::expect_used)]
        let mut queue = self.queue.lock().expect("listener queue poisoned");
        queue.drain(..).collect()
    }

    /// Number of pending inputs. Useful for tests and the snapshot.
    pub fn pending_len(&self) -> usize {
        #[allow(clippy::expect_used)]
        self.queue.lock().expect("listener queue poisoned").len()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

fn classify_supervision(kind: SupervisionEventKind) -> InputKind {
    match kind {
        SupervisionEventKind::WorkerStarted | SupervisionEventKind::WorkerReady => {
            InputKind::Recovery
        }
        SupervisionEventKind::WorkerExit | SupervisionEventKind::WorkerStopped => {
            InputKind::WorkerExit
        }
        SupervisionEventKind::WorkerNotReady => InputKind::WorkerNotReady,
        SupervisionEventKind::WorkerDegraded => InputKind::Degraded,
        SupervisionEventKind::WorkerFailed => InputKind::Failed,
        SupervisionEventKind::CrashLoopEntered => InputKind::CrashLoop,
        SupervisionEventKind::WorkerStopping | SupervisionEventKind::RestartScheduled => {
            InputKind::Degraded
        }
    }
}

fn map_agent_state(state: SupervisionAgentState) -> ComponentState {
    match state {
        SupervisionAgentState::Ready => ComponentState::Ready,
        SupervisionAgentState::Degraded => ComponentState::Degraded,
        SupervisionAgentState::Failed => ComponentState::Failed,
        SupervisionAgentState::Unknown => ComponentState::Unknown,
    }
}

fn map_serving_state(state: SupervisionServingState) -> ComponentState {
    match state {
        SupervisionServingState::Ready | SupervisionServingState::Running => ComponentState::Ready,
        SupervisionServingState::Degraded
        | SupervisionServingState::Starting
        | SupervisionServingState::AwaitingRestart => ComponentState::Degraded,
        SupervisionServingState::Failed | SupervisionServingState::CrashLoop => {
            ComponentState::Failed
        }
        SupervisionServingState::NoActiveDeployment
        | SupervisionServingState::Stopping
        | SupervisionServingState::Stopped => ComponentState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{EventListener, InputKind, InputSource};
    use crate::clock::FakeClock;
    use crate::config::ListenerConfig;
    use std::sync::Arc;
    use tensorplate_protocol::supervision_event::{SupervisionEvent, SupervisionEventKind};
    use tensorplate_protocol::{HealthEvent, SCHEMA_VERSION};

    fn listener_with_capacity(cap: u32) -> EventListener {
        let cfg = ListenerConfig {
            queue_capacity: cap,
            ..ListenerConfig::default()
        };
        EventListener::new(&cfg, Arc::new(FakeClock::new()))
    }

    #[test]
    fn submit_heartbeat_increments_accepted() {
        let l = listener_with_capacity(4);
        l.submit_health_event(&HealthEvent::heartbeat(0)).unwrap();
        let counters = l.counters.snapshot();
        assert_eq!(counters.accepted, 1);
        assert_eq!(counters.dropped, 0);
        let drained = l.try_drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].kind, InputKind::Heartbeat);
        assert_eq!(drained[0].source, InputSource::ServingWorker);
    }

    #[test]
    fn submit_supervision_event_normalises_kind() {
        let l = listener_with_capacity(4);
        let ev = SupervisionEvent::new(SupervisionEventKind::WorkerFailed, 1, 0);
        l.submit_supervision_event(&ev).unwrap();
        let drained = l.try_drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].kind, InputKind::Failed);
        assert_eq!(drained[0].source, InputSource::AgentSupervisor);
        assert_eq!(drained[0].sequence, Some(1));
    }

    #[test]
    fn duplicate_sequence_is_counted() {
        let l = listener_with_capacity(4);
        let a = SupervisionEvent::new(SupervisionEventKind::WorkerReady, 7, 0);
        let b = SupervisionEvent::new(SupervisionEventKind::WorkerReady, 7, 1);
        l.submit_supervision_event(&a).unwrap();
        l.submit_supervision_event(&b).unwrap();
        assert_eq!(l.counters.snapshot().duplicates, 1);
    }

    #[test]
    fn out_of_order_sequence_is_counted() {
        let l = listener_with_capacity(4);
        let a = SupervisionEvent::new(SupervisionEventKind::WorkerReady, 7, 0);
        let b = SupervisionEvent::new(SupervisionEventKind::WorkerReady, 3, 1);
        l.submit_supervision_event(&a).unwrap();
        l.submit_supervision_event(&b).unwrap();
        assert_eq!(l.counters.snapshot().out_of_order, 1);
    }

    #[test]
    fn unknown_version_is_rejected_typed() {
        let l = listener_with_capacity(4);
        let mut ev = HealthEvent::heartbeat(0);
        ev.schema_version = "99.99".into();
        let err = l.submit_health_event(&ev).unwrap_err();
        assert!(matches!(
            err,
            crate::error::ObservabilityError::InvalidEvent(_)
        ));
        assert_eq!(l.counters.snapshot().unknown_version, 1);
    }

    #[test]
    fn bounded_queue_drops_oldest_when_full() {
        let l = listener_with_capacity(2);
        for _ in 0..4 {
            l.submit_health_event(&HealthEvent::heartbeat(0)).unwrap();
        }
        let counters = l.counters.snapshot();
        assert_eq!(counters.accepted, 4);
        assert_eq!(counters.dropped, 2);
        let drained = l.try_drain();
        assert_eq!(drained.len(), 2);
    }

    #[test]
    fn submit_json_accepts_health_event() {
        let l = listener_with_capacity(4);
        let raw = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","kind":"ready","monotonic_timestamp_ns":0}}"#
        );
        l.submit_json(&raw).unwrap();
        assert_eq!(l.counters.snapshot().accepted, 1);
    }

    #[test]
    fn submit_json_accepts_supervision_event() {
        let l = listener_with_capacity(4);
        let raw = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","kind":"worker_failed","sequence":1,"monotonic_timestamp_ns":0}}"#
        );
        l.submit_json(&raw).unwrap();
        assert_eq!(l.counters.snapshot().accepted, 1);
    }

    #[test]
    fn submit_json_malformed_payload_increments_counter() {
        let l = listener_with_capacity(4);
        let err = l.submit_json("not json").unwrap_err();
        assert!(matches!(
            err,
            crate::error::ObservabilityError::InvalidEvent(_)
        ));
        assert_eq!(l.counters.snapshot().malformed, 1);
    }

    #[test]
    fn submit_json_unknown_version_is_typed() {
        let l = listener_with_capacity(4);
        let raw = r#"{"schema_version":"99.99","kind":"heartbeat","monotonic_timestamp_ns":0}"#;
        let err = l.submit_json(raw).unwrap_err();
        assert!(matches!(
            err,
            crate::error::ObservabilityError::InvalidEvent(_)
        ));
        assert_eq!(l.counters.snapshot().unknown_version, 1);
    }
}
