// SPDX-License-Identifier: Apache-2.0
//
// V01-E10-F01-T02: Observability service composition root.
//
// The `Service` type wires together every V01-E10 component:
//
//   - the bounded local listener (F02)
//   - the monotonic heartbeat evaluator (F03)
//   - the health-state aggregator + safe-state sink (F04)
//   - the optional ROS 2 health publisher stub (F05)
//   - the status snapshot writer with bounded diagnostics (F06)
//
// The service is `tick`-driven, mirroring the V01-E09 supervisor: the
// composition root advances state deterministically and the binary's
// main loop calls `tick` at a configured cadence. Tests inject a
// `FakeClock` and an in-memory listener so the whole pipeline is
// deterministic without any background thread.

use std::sync::Arc;
use std::time::Duration;

use tensorplate_protocol::error::ErrorCode;

use crate::clock::{MonotonicClock, SystemMonotonicClock};
use crate::config::{ObservabilityConfig, SafeStateSinkKind};
use crate::error::ObservabilityResult;
use crate::heartbeat::HeartbeatEvaluator;
use crate::listener::{EventListener, HealthInput, InputKind, InputSource};
use crate::ros2::Ros2HealthPublisher;
use crate::sink::{sink_from_config, SafeStateSink};
use crate::snapshot::{
    PublisherStatus, RecentError, RecentTransition, SinkStatus, SnapshotWriter, StatusSnapshot,
};
use crate::state::{Aggregator, SafeStateEvent};

/// Composition root. Hold one of these per observability process.
pub struct Service {
    cfg: ObservabilityConfig,
    clock: Arc<dyn MonotonicClock>,
    listener: Arc<EventListener>,
    heartbeat: Arc<HeartbeatEvaluator>,
    aggregator: Arc<Aggregator>,
    safe_state_sink: Arc<dyn SafeStateSink>,
    snapshot: Arc<SnapshotWriter>,
    ros2: Option<Arc<Ros2HealthPublisher>>,
    primary_source: InputSource,
    safe_state_sink_kind: SafeStateSinkKind,
}

impl Service {
    /// Build the service from a validated config. The composition root
    /// constructs each component eagerly so startup failures surface as
    /// typed errors before any listener binds or any file is opened.
    ///
    /// # Errors
    ///
    /// Returns [`ObservabilityError::Config`] when the config has not
    /// been validated, or [`ObservabilityError::Internal`] when an
    /// internal invariant is violated.
    pub fn new(cfg: ObservabilityConfig) -> ObservabilityResult<Self> {
        let cfg = cfg.validate()?;
        Self::with_clock(cfg, Arc::new(SystemMonotonicClock))
    }

    /// Build the service with a caller-supplied monotonic clock. Used
    /// by tests with `FakeClock`.
    ///
    /// # Errors
    ///
    /// See [`Service::new`].
    pub fn with_clock(
        cfg: ObservabilityConfig,
        clock: Arc<dyn MonotonicClock>,
    ) -> ObservabilityResult<Self> {
        let cfg = cfg.validate()?;
        let listener = Arc::new(EventListener::new(&cfg.listener, clock.clone()));
        let heartbeat = Arc::new(HeartbeatEvaluator::new(
            cfg.heartbeat.clone(),
            clock.clone(),
        ));
        let primary_source = match cfg.primary_source.as_str() {
            "agent_supervisor" => InputSource::AgentSupervisor,
            "internal" => InputSource::Internal,
            _ => InputSource::ServingWorker,
        };
        heartbeat.register_source(primary_source);
        let aggregator = Arc::new(Aggregator::new(cfg.heartbeat.clone(), primary_source));
        let safe_state_sink: Arc<dyn SafeStateSink> = Arc::from(sink_from_config(&cfg.safe_state));
        let snapshot = Arc::new(SnapshotWriter::new(&cfg.snapshot));
        let ros2 = Ros2HealthPublisher::from_config(&cfg.ros2_health).map(Arc::new);
        let safe_state_sink_kind = cfg.safe_state.kind;
        Ok(Self {
            cfg,
            clock,
            listener,
            heartbeat,
            aggregator,
            safe_state_sink,
            snapshot,
            ros2,
            primary_source,
            safe_state_sink_kind,
        })
    }

    pub fn config(&self) -> &ObservabilityConfig {
        &self.cfg
    }

    /// Listener handle exposed for test fixtures and for the in-process
    /// minimal serving-worker heartbeat source.
    pub fn listener(&self) -> Arc<EventListener> {
        self.listener.clone()
    }

    /// Heartbeat evaluator handle exposed for tests.
    pub fn heartbeat(&self) -> Arc<HeartbeatEvaluator> {
        self.heartbeat.clone()
    }

    /// Aggregator handle exposed for tests.
    pub fn aggregator(&self) -> Arc<Aggregator> {
        self.aggregator.clone()
    }

    /// Snapshot writer handle exposed for tests and the binary entry
    /// point.
    pub fn snapshot_writer(&self) -> Arc<SnapshotWriter> {
        self.snapshot.clone()
    }

    /// Safe-state sink handle. Tests downcast as appropriate to assert
    /// emitted events.
    pub fn safe_state_sink(&self) -> Arc<dyn SafeStateSink> {
        self.safe_state_sink.clone()
    }

    /// Optional ROS 2 publisher handle.
    pub fn ros2_publisher(&self) -> Option<Arc<Ros2HealthPublisher>> {
        self.ros2.clone()
    }

    /// Configured primary heartbeat source.
    pub fn primary_source(&self) -> InputSource {
        self.primary_source
    }

    /// Minimal in-process heartbeat emitter. Tests and deliberately
    /// internal deployments can feed the listener directly with a
    /// synthetic heartbeat input, but the production binary only calls
    /// this when `primary_source=internal` so the default
    /// `serving_worker` source cannot be masked by self-heartbeats.
    pub fn emit_internal_heartbeat(&self) {
        let input = HealthInput::heartbeat(self.primary_source, self.clock.now());
        self.listener.submit_input(input);
    }

    /// Drive one tick of the pipeline: drain the listener, recompute
    /// heartbeat freshness, update the aggregator, emit safe-state
    /// events, and refresh the snapshot. The binary main loop calls
    /// this on every wakeup; the integration harness calls it directly
    /// after advancing the fake clock.
    ///
    /// Returns the safe-state events that were emitted during this
    /// tick so callers (tests, the V01-E11 CLI) can assert progress
    /// without scraping the in-memory sink.
    pub fn tick(&self) -> Vec<SafeStateEvent> {
        let inputs = self.listener.try_drain();
        // Fold heartbeats into the evaluator before we ask it to
        // recompute every source's freshness. We do this here, not in
        // the listener, so the heartbeat source / freshness coupling
        // lives in one place.
        for input in &inputs {
            if matches!(input.kind, InputKind::Heartbeat) {
                self.heartbeat
                    .observe_heartbeat(input.source, input.received_at);
            }
        }
        let heartbeat = self.heartbeat.evaluate();
        let now = self.clock.now();
        let periodic = self.cfg.safe_state.periodic_ms.map(Duration::from_millis);
        let events = self.aggregator.apply(&inputs, &heartbeat, now, periodic);
        for event in &events {
            self.safe_state_sink.emit(event);
            self.snapshot.record_transition(RecentTransition {
                previous_state: event.previous_state.as_str().into(),
                state: event.state.as_str().into(),
                reason: event.reason.as_str().into(),
                monotonic_age_ms: event.monotonic_age_ms,
            });
            if let Some(code) = event.last_error_code {
                self.snapshot.record_error(RecentError {
                    component: "aggregator".into(),
                    code,
                    message: format!(
                        "transition {} -> {} ({})",
                        event.previous_state.as_str(),
                        event.state.as_str(),
                        event.reason.as_str()
                    ),
                });
            }
        }
        if let Some(ros2) = self.ros2.as_ref() {
            let state = self.aggregator.current();
            let _ = ros2.publish_if_due(&state, now);
        }
        let listener_counters = self.listener.counters.snapshot();
        if listener_counters.unknown_version != 0 || listener_counters.malformed != 0 {
            self.snapshot.record_error(RecentError {
                component: "listener".into(),
                code: ErrorCode::Unsupported,
                message: format!(
                    "unknown_version={} malformed={}",
                    listener_counters.unknown_version, listener_counters.malformed
                ),
            });
        }
        let state = self.aggregator.current();
        let sink_status = SinkStatus {
            enabled: true,
            dropped: self.safe_state_sink.dropped(),
            errors: self.safe_state_sink.errors(),
        };
        let publisher_status = match self.ros2.as_ref() {
            Some(p) => PublisherStatus {
                enabled: true,
                topic: p.topic.clone(),
                published: p.published(),
                errors: p.errors(),
            },
            None => PublisherStatus::default(),
        };
        self.snapshot.update(
            &state,
            listener_counters.into(),
            sink_status,
            publisher_status,
        );
        events
    }

    /// Persist the current snapshot to disk if file-backed.
    ///
    /// # Errors
    ///
    /// Returns [`ObservabilityError::SnapshotSink`] for IO failures.
    pub fn flush_snapshot(&self) -> ObservabilityResult<()> {
        self.snapshot.flush()
    }

    /// Read the most recent snapshot.
    pub fn snapshot(&self) -> StatusSnapshot {
        self.snapshot.current()
    }

    /// Drain the in-memory safe-state sink. Returns `None` when a
    /// file-backed sink is configured.
    pub fn drain_in_memory_sink(&self) -> Option<Vec<SafeStateEvent>> {
        if !matches!(self.safe_state_sink_kind, SafeStateSinkKind::InMemory) {
            return None;
        }
        // We constructed the sink through `sink_from_config`, which
        // returns a boxed trait object. The owning Arc cannot be
        // downcast directly, so we keep the trait surface and ask the
        // caller to construct services with `with_sinks` when they
        // need direct access.
        None
    }

    /// Test-only constructor that lets fixtures inject specific sink
    /// instances. Production code uses [`Service::new`] /
    /// [`Service::with_clock`].
    #[doc(hidden)]
    pub fn with_components(
        cfg: ObservabilityConfig,
        clock: Arc<dyn MonotonicClock>,
        safe_state_sink: Arc<dyn SafeStateSink>,
        ros2: Option<Arc<Ros2HealthPublisher>>,
    ) -> ObservabilityResult<Self> {
        let cfg = cfg.validate()?;
        let listener = Arc::new(EventListener::new(&cfg.listener, clock.clone()));
        let heartbeat = Arc::new(HeartbeatEvaluator::new(
            cfg.heartbeat.clone(),
            clock.clone(),
        ));
        let primary_source = match cfg.primary_source.as_str() {
            "agent_supervisor" => InputSource::AgentSupervisor,
            "internal" => InputSource::Internal,
            _ => InputSource::ServingWorker,
        };
        heartbeat.register_source(primary_source);
        let aggregator = Arc::new(Aggregator::new(cfg.heartbeat.clone(), primary_source));
        let snapshot = Arc::new(SnapshotWriter::new(&cfg.snapshot));
        let safe_state_sink_kind = cfg.safe_state.kind;
        Ok(Self {
            cfg,
            clock,
            listener,
            heartbeat,
            aggregator,
            safe_state_sink,
            snapshot,
            ros2,
            primary_source,
            safe_state_sink_kind,
        })
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        // Best-effort snapshot persistence on shutdown. The composition
        // root never blocks shutdown on the snapshot; failures are
        // logged and ignored.
        if let Err(err) = self.snapshot.flush() {
            #[allow(clippy::print_stderr)]
            {
                eprintln!("tensorplate-observability: snapshot flush failed at shutdown: {err}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::Service;
    use crate::clock::FakeClock;
    use crate::config::{
        HeartbeatPolicy, ListenerConfig, ObservabilityConfig, Ros2HealthConfig,
        SafeStateSinkConfig, StatusSnapshotConfig,
    };
    use crate::sink::InMemorySafeStateSink;
    use crate::state::ObservabilityState;
    use std::sync::Arc;
    use std::time::Duration;
    use tensorplate_protocol::HealthEvent;

    fn fast_policy() -> HeartbeatPolicy {
        HeartbeatPolicy {
            expected_interval_ms: 50,
            grace_ms: 10,
            missed_threshold: 2,
            recovery_heartbeats: 1,
        }
    }

    fn config() -> ObservabilityConfig {
        ObservabilityConfig {
            heartbeat: fast_policy(),
            ..ObservabilityConfig::default()
        }
    }

    fn service_with_sinks(clock: Arc<FakeClock>) -> (Service, Arc<InMemorySafeStateSink>) {
        let cfg = config();
        let sink = Arc::new(InMemorySafeStateSink::new(&cfg.safe_state));
        let svc = Service::with_components(cfg, clock, sink.clone(), None).expect("service");
        (svc, sink)
    }

    #[test]
    fn fresh_heartbeat_keeps_state_ready() {
        let clock = Arc::new(FakeClock::new());
        let (svc, _sink) = service_with_sinks(clock.clone());
        svc.emit_internal_heartbeat();
        svc.tick();
        assert_eq!(svc.snapshot().observability_state, "ready");
    }

    #[test]
    fn missing_heartbeat_transitions_to_no_heartbeat_without_agent_input() {
        let clock = Arc::new(FakeClock::new());
        let (svc, sink) = service_with_sinks(clock.clone());
        svc.emit_internal_heartbeat();
        svc.tick();
        clock.advance(Duration::from_millis(500));
        svc.tick();
        let snap = svc.snapshot();
        assert_eq!(snap.observability_state, "no_heartbeat");
        // V01-E10 acceptance: no agent input required.
        let drained = sink.drain();
        assert!(drained.iter().any(|e| {
            matches!(e.state, ObservabilityState::NoHeartbeat)
                || matches!(e.state, ObservabilityState::Degraded)
        }));
    }

    #[test]
    fn registered_source_without_initial_heartbeat_transitions_to_no_heartbeat() {
        let clock = Arc::new(FakeClock::new());
        let (svc, sink) = service_with_sinks(clock.clone());
        svc.tick();
        assert_eq!(svc.snapshot().observability_state, "degraded");
        clock.advance(Duration::from_millis(500));
        svc.tick();
        assert_eq!(svc.snapshot().observability_state, "no_heartbeat");
        assert!(sink
            .drain()
            .iter()
            .any(|e| matches!(e.state, ObservabilityState::NoHeartbeat)));
    }

    #[test]
    fn safe_state_sink_records_transitions() {
        let clock = Arc::new(FakeClock::new());
        let (svc, sink) = service_with_sinks(clock.clone());
        svc.emit_internal_heartbeat();
        svc.tick();
        // Push a `failed` event.
        svc.listener()
            .submit_health_event(&HealthEvent::state(
                tensorplate_protocol::health_event::HealthEventKind::Failed,
                0,
            ))
            .expect("submit");
        svc.tick();
        let drained = sink.drain();
        assert!(!drained.is_empty());
        assert!(drained
            .iter()
            .any(|e| matches!(e.state, ObservabilityState::Failed)));
        assert_eq!(svc.snapshot().observability_state, "failed");
    }

    #[test]
    fn config_invalid_fails_startup() {
        let mut cfg = ObservabilityConfig::default();
        cfg.heartbeat.missed_threshold = 0;
        assert!(Service::new(cfg).is_err());
    }

    #[test]
    fn ros2_publisher_is_constructed_when_enabled() {
        let clock = Arc::new(FakeClock::new());
        let cfg = ObservabilityConfig {
            heartbeat: fast_policy(),
            ros2_health: Ros2HealthConfig {
                enabled: true,
                ..Ros2HealthConfig::default()
            },
            ..ObservabilityConfig::default()
        };
        let svc = Service::with_clock(cfg, clock.clone()).expect("svc");
        assert!(svc.ros2_publisher().is_some());
        svc.emit_internal_heartbeat();
        svc.tick();
        let snap = svc.snapshot();
        assert!(snap.ros2_publisher.enabled);
        assert!(snap.ros2_publisher.published >= 1);
    }

    #[test]
    fn snapshot_includes_listener_counters() {
        let clock = Arc::new(FakeClock::new());
        let (svc, _sink) = service_with_sinks(clock.clone());
        let _ = svc
            .listener()
            .submit_json("not json")
            .expect_err("malformed");
        svc.tick();
        let snap = svc.snapshot();
        assert!(snap.listener.malformed >= 1);
    }

    #[test]
    fn flush_snapshot_is_noop_for_in_memory_writer() {
        let clock = Arc::new(FakeClock::new());
        let (svc, _sink) = service_with_sinks(clock.clone());
        svc.emit_internal_heartbeat();
        svc.tick();
        svc.flush_snapshot().expect("noop ok");
    }

    #[test]
    fn periodic_safe_state_emission_continues_until_recovery() {
        let clock = Arc::new(FakeClock::new());
        let cfg = ObservabilityConfig {
            heartbeat: fast_policy(),
            safe_state: SafeStateSinkConfig {
                periodic_ms: Some(20),
                ..SafeStateSinkConfig::default()
            },
            ..ObservabilityConfig::default()
        };
        let sink = Arc::new(InMemorySafeStateSink::new(&cfg.safe_state));
        let svc = Service::with_components(cfg, clock.clone(), sink.clone(), None).expect("svc");
        svc.emit_internal_heartbeat();
        svc.tick();
        clock.advance(Duration::from_millis(500));
        svc.tick();
        clock.advance(Duration::from_millis(30));
        svc.tick();
        let drained = sink.drain();
        // At least one transition event + one periodic event.
        assert!(drained.len() >= 2);
    }

    #[test]
    fn snapshot_writer_can_be_file_backed() {
        let clock = Arc::new(FakeClock::new());
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("status.json");
        let cfg = ObservabilityConfig {
            heartbeat: fast_policy(),
            snapshot: StatusSnapshotConfig {
                kind: crate::config::StatusSnapshotKind::File,
                path: Some(path.clone()),
                diagnostics_capacity: 8,
            },
            ..ObservabilityConfig::default()
        };
        let svc = Service::with_clock(cfg, clock.clone()).expect("svc");
        svc.emit_internal_heartbeat();
        svc.tick();
        svc.flush_snapshot().expect("flush");
        let body = std::fs::read_to_string(&path).expect("read");
        let parsed: crate::snapshot::StatusSnapshot = serde_json::from_str(&body).expect("parse");
        assert_eq!(parsed.observability_state, "ready");
        let _ = ListenerConfig::default();
    }
}
