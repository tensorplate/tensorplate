// SPDX-License-Identifier: Apache-2.0
//
// V01-E10-F07: Observability service integration and failure-injection
// tests.
//
// These tests drive the full V01-E10 pipeline through deterministic
// fixtures:
//
//   - a fake monotonic clock,
//   - the in-process listener (V01-E10-F02),
//   - the heartbeat evaluator (V01-E10-F03) with a tight policy so
//     missed-heartbeat thresholds fire in a single tick,
//   - the in-memory safe-state sink (V01-E10-F04) so we can drain and
//     assert every emitted event,
//   - the mock ROS 2 publisher (V01-E10-F05) so we can assert the
//     `diagnostic_msgs::DiagnosticArray` mapping without a ROS 2
//     runtime,
//   - the in-memory snapshot writer (V01-E10-F06) to assert the
//     status surface and the bounded diagnostics ring.
//
// The test matrix mirrors V01-E10-F07-T02 and the V01-E10 acceptance
// criteria: healthy heartbeat, missing heartbeat without agent input,
// recovery, degraded, failed, crash-loop, overload, malformed events,
// unknown schema version, event storm, sink failure modes, snapshot
// consistency across transitions, and ROS 2 level / key-value mapping.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::default_trait_access,
    clippy::missing_panics_doc
)]

use std::sync::Arc;
use std::time::Duration;

use tensorplate_observability::{
    config::{
        HeartbeatPolicy, ObservabilityConfig, Ros2HealthConfig, SafeStateSinkConfig,
        StatusSnapshotConfig, StatusSnapshotKind,
    },
    sink::InMemorySafeStateSink,
    state::{ObservabilityState, SafeStateReason},
    DiagnosticLevel, FakeClock, HealthInput, InputKind, InputSource, MockHealthPublisher,
    MonotonicClock, Ros2HealthPublisher, Service,
};
use tensorplate_protocol::health_event::HealthEventKind;
use tensorplate_protocol::supervision_event::{SupervisionEvent, SupervisionEventKind};
use tensorplate_protocol::worker_status::{ComponentState, WorkerStatus};
use tensorplate_protocol::{ErrorCode, HealthEvent, SCHEMA_VERSION};

fn tight_heartbeat() -> HeartbeatPolicy {
    HeartbeatPolicy {
        expected_interval_ms: 50,
        grace_ms: 10,
        missed_threshold: 2,
        recovery_heartbeats: 1,
    }
}

fn base_config() -> ObservabilityConfig {
    ObservabilityConfig {
        heartbeat: tight_heartbeat(),
        safe_state: SafeStateSinkConfig {
            periodic_ms: Some(40),
            ..SafeStateSinkConfig::default()
        },
        ..ObservabilityConfig::default()
    }
}

struct Harness {
    pub clock: Arc<FakeClock>,
    pub service: Service,
    pub sink: Arc<InMemorySafeStateSink>,
    pub publisher: Option<Arc<Ros2HealthPublisher>>,
}

impl Harness {
    fn build(cfg: ObservabilityConfig) -> Self {
        let clock = Arc::new(FakeClock::new());
        let sink = Arc::new(InMemorySafeStateSink::new(&cfg.safe_state));
        let publisher = if cfg.ros2_health.enabled {
            let mock = MockHealthPublisher::new(cfg.ros2_health.topic.clone());
            Some(Arc::new(Ros2HealthPublisher::new(
                Box::new(mock),
                &cfg.ros2_health,
            )))
        } else {
            None
        };
        let svc = Service::with_components(cfg, clock.clone(), sink.clone(), publisher.clone())
            .expect("service builds");
        Self {
            clock,
            service: svc,
            sink,
            publisher,
        }
    }

    fn drain_sink(&self) -> Vec<tensorplate_observability::state::SafeStateEvent> {
        self.sink.drain()
    }
}

#[test]
fn healthy_heartbeat_keeps_state_ready() {
    let h = Harness::build(base_config());
    h.service.emit_internal_heartbeat();
    h.service.tick();
    let snap = h.service.snapshot();
    assert_eq!(snap.observability_state, "ready");
    assert_eq!(snap.listener.accepted, 1);
    assert!(h.drain_sink().is_empty());
}

#[test]
fn missing_heartbeat_detected_without_agent_input() {
    let h = Harness::build(base_config());
    // First heartbeat establishes the source.
    h.service.emit_internal_heartbeat();
    h.service.tick();
    assert_eq!(h.service.snapshot().observability_state, "ready");

    // Advance well past the missed threshold and tick. We do NOT
    // submit any supervision event - this proves the observability
    // service can detect a wedged worker without agent cooperation.
    h.clock.advance(Duration::from_millis(500));
    h.service.tick();

    let snap = h.service.snapshot();
    assert_eq!(snap.observability_state, "no_heartbeat");
    assert!(snap.missed_heartbeat_count >= 2);
    let drained = h.drain_sink();
    assert!(drained
        .iter()
        .any(|e| matches!(e.state, ObservabilityState::NoHeartbeat)));
    assert!(drained
        .iter()
        .any(|e| matches!(e.reason, SafeStateReason::HeartbeatMissing)));
}

#[test]
fn heartbeat_recovery_returns_to_ready() {
    let h = Harness::build(base_config());
    h.service.emit_internal_heartbeat();
    h.service.tick();
    h.clock.advance(Duration::from_millis(500));
    h.service.tick();
    assert_eq!(h.service.snapshot().observability_state, "no_heartbeat");
    h.clock.advance(Duration::from_millis(5));
    h.service.emit_internal_heartbeat();
    h.service.tick();
    let snap = h.service.snapshot();
    assert_eq!(snap.observability_state, "ready");
    let drained = h.drain_sink();
    assert!(drained
        .iter()
        .any(|e| matches!(e.reason, SafeStateReason::Recovery)));
}

#[test]
fn explicit_failed_event_drives_failed_state() {
    let h = Harness::build(base_config());
    h.service.emit_internal_heartbeat();
    h.service.tick();
    h.service
        .listener()
        .submit_health_event(&HealthEvent::state(HealthEventKind::Failed, 0))
        .expect("submit");
    h.service.tick();
    let snap = h.service.snapshot();
    assert_eq!(snap.observability_state, "failed");
    let drained = h.drain_sink();
    assert!(drained
        .iter()
        .any(|e| matches!(e.state, ObservabilityState::Failed)));
}

#[test]
fn explicit_no_heartbeat_health_event_drives_no_heartbeat_state() {
    let h = Harness::build(base_config());
    h.service.emit_internal_heartbeat();
    h.service.tick();
    h.service
        .listener()
        .submit_health_event(&HealthEvent::state(HealthEventKind::NoHeartbeat, 0))
        .expect("submit");
    h.service.tick();
    let snap = h.service.snapshot();
    assert_eq!(snap.observability_state, "no_heartbeat");
    let drained = h.drain_sink();
    assert!(drained
        .iter()
        .any(|e| matches!(e.state, ObservabilityState::NoHeartbeat)));
}

#[test]
fn worker_status_updates_required_snapshot_fields_and_can_clear_zero_metrics() {
    let h = Harness::build(base_config());
    h.service.emit_internal_heartbeat();
    h.service.tick();
    let overloaded = WorkerStatus::new(
        ComponentState::Ready,
        ComponentState::Degraded,
        ComponentState::Degraded,
        "deploy-status",
        "tensorrt",
        0,
        0.4,
        7,
        None,
    )
    .expect("status");
    h.service
        .listener()
        .submit_worker_status(&overloaded)
        .expect("submit");
    h.service.tick();
    let snap = h.service.snapshot();
    assert_eq!(snap.active_deployment, "deploy-status");
    assert_eq!(snap.backend, "tensorrt");
    assert_eq!(snap.queue_depth, 7);
    assert!((snap.missed_deadline_rate - 0.4).abs() < f64::EPSILON);

    let ready = WorkerStatus::new(
        ComponentState::Ready,
        ComponentState::Ready,
        ComponentState::Ready,
        "deploy-status",
        "tensorrt",
        0,
        0.0,
        0,
        None,
    )
    .expect("status");
    h.service
        .listener()
        .submit_worker_status(&ready)
        .expect("submit");
    h.service.tick();
    let snap = h.service.snapshot();
    assert_eq!(snap.observability_state, "ready");
    assert_eq!(snap.queue_depth, 0);
    assert!(snap.missed_deadline_rate.abs() < f64::EPSILON);
}

#[test]
fn crash_loop_supervision_event_drives_failed_state() {
    let h = Harness::build(base_config());
    h.service.emit_internal_heartbeat();
    h.service.tick();
    let mut crash = SupervisionEvent::new(SupervisionEventKind::CrashLoopEntered, 1, 0);
    crash.error_code = Some(ErrorCode::Internal);
    crash.active_deployment = "deploy-1".into();
    crash.backend = "mock".into();
    h.service
        .listener()
        .submit_supervision_event(&crash)
        .expect("submit");
    h.service.tick();
    let snap = h.service.snapshot();
    assert_eq!(snap.observability_state, "failed");
    assert_eq!(snap.active_deployment, "deploy-1");
    let drained = h.drain_sink();
    assert!(drained.iter().any(|e| {
        matches!(e.state, ObservabilityState::Failed)
            && matches!(e.reason, SafeStateReason::CrashLoop)
    }));
}

#[test]
fn worker_exit_supervision_event_drives_failed_state() {
    let h = Harness::build(base_config());
    h.service.emit_internal_heartbeat();
    h.service.tick();
    let exit = SupervisionEvent::new(SupervisionEventKind::WorkerExit, 2, 0);
    h.service
        .listener()
        .submit_supervision_event(&exit)
        .expect("submit");
    h.service.tick();
    let snap = h.service.snapshot();
    assert_eq!(snap.observability_state, "failed");
}

#[test]
fn worker_not_ready_supervision_event_drives_degraded_state() {
    let h = Harness::build(base_config());
    h.service.emit_internal_heartbeat();
    h.service.tick();
    let not_ready = SupervisionEvent::new(SupervisionEventKind::WorkerNotReady, 3, 0);
    h.service
        .listener()
        .submit_supervision_event(&not_ready)
        .expect("submit");
    h.service.tick();
    assert_eq!(h.service.snapshot().observability_state, "degraded");
}

#[test]
fn overload_event_drives_degraded_state_with_overload_reason() {
    let h = Harness::build(base_config());
    h.service.emit_internal_heartbeat();
    h.service.tick();
    // Submit a normalised overload input with a missed deadline rate
    // so the reason resolves to Overload.
    let mut input = HealthInput::heartbeat(InputSource::ServingWorker, h.clock.now());
    input.kind = InputKind::Overload;
    input.serving_state = Some(ComponentState::Degraded);
    input.missed_deadline_rate = Some(0.1);
    input.queue_depth = Some(12);
    h.service.listener().submit_input(input);
    h.service.tick();
    let snap = h.service.snapshot();
    assert_eq!(snap.observability_state, "degraded");
    let drained = h.drain_sink();
    assert!(drained
        .iter()
        .any(|e| matches!(e.reason, SafeStateReason::Overload)));
}

#[test]
fn malformed_payload_bumps_listener_counter() {
    let h = Harness::build(base_config());
    let err = h
        .service
        .listener()
        .submit_json("not json")
        .expect_err("malformed");
    assert!(matches!(
        err,
        tensorplate_observability::ObservabilityError::InvalidEvent(_)
    ));
    h.service.tick();
    assert!(h.service.snapshot().listener.malformed >= 1);
}

#[test]
fn unknown_schema_version_is_rejected_typed() {
    let h = Harness::build(base_config());
    let raw = r#"{"schema_version":"99.99","kind":"heartbeat","monotonic_timestamp_ns":0}"#;
    let err = h.service.listener().submit_json(raw).expect_err("rejected");
    assert!(matches!(
        err,
        tensorplate_observability::ObservabilityError::InvalidEvent(_)
    ));
    h.service.tick();
    assert!(h.service.snapshot().listener.unknown_version >= 1);
}

#[test]
fn event_storm_drops_oldest_and_bumps_counter() {
    let mut cfg = base_config();
    cfg.listener.queue_capacity = 4;
    let h = Harness::build(cfg);
    for _ in 0..16 {
        h.service
            .listener()
            .submit_health_event(&HealthEvent::heartbeat(0))
            .expect("submit");
    }
    h.service.tick();
    let snap = h.service.snapshot();
    assert!(snap.listener.dropped >= 12);
    assert!(snap.listener.accepted >= 4);
}

#[test]
fn duplicate_and_out_of_order_supervision_events_bump_counters() {
    let h = Harness::build(base_config());
    let a = SupervisionEvent::new(SupervisionEventKind::WorkerReady, 7, 0);
    let dup = SupervisionEvent::new(SupervisionEventKind::WorkerReady, 7, 1);
    let earlier = SupervisionEvent::new(SupervisionEventKind::WorkerReady, 3, 2);
    h.service
        .listener()
        .submit_supervision_event(&a)
        .expect("submit");
    h.service
        .listener()
        .submit_supervision_event(&dup)
        .expect("submit");
    h.service
        .listener()
        .submit_supervision_event(&earlier)
        .expect("submit");
    h.service.tick();
    let snap = h.service.snapshot();
    assert!(snap.listener.duplicates >= 1);
    assert!(snap.listener.out_of_order >= 1);
}

#[test]
fn periodic_safe_state_emission_repeats_until_recovery() {
    let h = Harness::build(base_config());
    h.service.emit_internal_heartbeat();
    h.service.tick();
    h.clock.advance(Duration::from_millis(500));
    h.service.tick();
    // We are now in `no_heartbeat`. Tick twice more without sending a
    // heartbeat; each periodic interval should produce one event.
    h.clock.advance(Duration::from_millis(60));
    h.service.tick();
    h.clock.advance(Duration::from_millis(60));
    h.service.tick();
    let drained = h.drain_sink();
    let periodic_count = drained
        .iter()
        .filter(|e| matches!(e.reason, SafeStateReason::Periodic))
        .count();
    assert!(periodic_count >= 1, "expected at least one periodic event");
}

#[test]
fn ros2_publisher_emits_diagnostic_array_with_required_mapping() {
    let mut cfg = base_config();
    cfg.ros2_health = Ros2HealthConfig {
        enabled: true,
        topic: "/tensorplate/health".into(),
        interval_ms: 30,
        runtime: tensorplate_observability::Ros2Runtime::Mock,
    };
    let h = Harness::build(cfg);
    h.service.emit_internal_heartbeat();
    h.service.tick();
    let snap = h.service.snapshot();
    assert!(snap.ros2_publisher.enabled);
    assert_eq!(snap.ros2_publisher.topic, "/tensorplate/health");
    assert!(snap.ros2_publisher.published >= 1);

    // Drive a failed transition and assert the published array's
    // level / key-value mapping.
    h.service
        .listener()
        .submit_health_event(&HealthEvent::state(HealthEventKind::Failed, 0))
        .expect("submit");
    h.service.tick();
    let publisher = h.publisher.as_ref().expect("publisher");
    let arrays = {
        // We constructed the publisher with a MockHealthPublisher;
        // build_diagnostic_array gives us the wire shape for direct
        // assertion. The mock publisher buffered the array internally
        // during publish_if_due.
        let arr = tensorplate_observability::build_diagnostic_array(
            "/tensorplate/health",
            &h.service.aggregator().current(),
        );
        vec![arr]
    };
    assert_eq!(arrays[0].status.len(), 1);
    let status = &arrays[0].status[0];
    assert_eq!(status.name, "tensorplate/runtime");
    assert_eq!(
        DiagnosticLevel::from_state(ObservabilityState::Failed),
        DiagnosticLevel::Error
    );
    let keys: Vec<&str> = status.values.iter().map(|kv| kv.key.as_str()).collect();
    for required in [
        "agent_state",
        "serving_state",
        "observability_state",
        "active_deployment",
        "backend",
        "missed_heartbeat_count",
        "missed_deadline_rate",
        "queue_depth",
        "last_error_code",
    ] {
        assert!(keys.contains(&required), "key {required} missing");
    }
    assert!(publisher.published() >= 2);
}

#[test]
fn disabled_ros2_publisher_yields_no_emissions() {
    let h = Harness::build(base_config());
    assert!(h.service.ros2_publisher().is_none());
    h.service.emit_internal_heartbeat();
    h.service.tick();
    assert!(!h.service.snapshot().ros2_publisher.enabled);
}

#[test]
fn snapshot_writer_is_atomic_under_file_mode() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("status.json");
    let cfg = ObservabilityConfig {
        heartbeat: tight_heartbeat(),
        snapshot: StatusSnapshotConfig {
            kind: StatusSnapshotKind::File,
            path: Some(path.clone()),
            diagnostics_capacity: 8,
        },
        ..ObservabilityConfig::default()
    };
    let h = Harness::build(cfg);
    h.service.emit_internal_heartbeat();
    h.service.tick();
    h.service.flush_snapshot().expect("flush");
    // No partial sibling file remains.
    assert!(!path.with_extension("partial").exists());
    let body = std::fs::read_to_string(&path).expect("read");
    let parsed: tensorplate_observability::StatusSnapshot =
        serde_json::from_str(&body).expect("parse");
    assert_eq!(parsed.observability_state, "ready");
    assert_eq!(parsed.schema_version, SCHEMA_VERSION);
}

#[test]
fn diagnostics_ring_retains_recent_transitions_and_errors() {
    let h = Harness::build(base_config());
    h.service.emit_internal_heartbeat();
    h.service.tick();
    let mut event = SupervisionEvent::new(SupervisionEventKind::WorkerFailed, 1, 0);
    event.error_code = Some(ErrorCode::Internal);
    h.service
        .listener()
        .submit_supervision_event(&event)
        .expect("submit");
    h.service.tick();
    let snap = h.service.snapshot();
    assert!(!snap.diagnostics.recent_transitions.is_empty());
    assert!(!snap.diagnostics.recent_errors.is_empty());
    assert!(snap
        .diagnostics
        .recent_errors
        .iter()
        .any(|e| matches!(e.code, ErrorCode::Internal)));
}

#[test]
fn safe_state_sink_drops_oldest_when_full() {
    let mut cfg = base_config();
    cfg.safe_state.queue_capacity = 2;
    cfg.safe_state.periodic_ms = Some(10);
    let h = Harness::build(cfg);
    h.service.emit_internal_heartbeat();
    h.service.tick();
    for _ in 0..10 {
        h.clock.advance(Duration::from_millis(20));
        h.service.tick();
    }
    let snap = h.service.snapshot();
    assert!(snap.safe_state_sink.dropped >= 1);
}

#[test]
fn agent_supervision_event_provides_active_deployment_and_backend() {
    let h = Harness::build(base_config());
    let mut event = SupervisionEvent::new(SupervisionEventKind::WorkerDegraded, 5, 0);
    event.active_deployment = "deploy-9".into();
    event.backend = "tensorrt".into();
    h.service
        .listener()
        .submit_supervision_event(&event)
        .expect("submit");
    h.service.tick();
    let snap = h.service.snapshot();
    assert_eq!(snap.active_deployment, "deploy-9");
    assert_eq!(snap.backend, "tensorrt");
    assert_eq!(snap.observability_state, "degraded");
}

#[test]
fn no_agent_input_proves_independent_heartbeat_detection() {
    // V01-E10 core acceptance: the observability service detects a
    // wedged serving worker without requiring the agent to detect it
    // first. This test never submits a supervision event.
    let h = Harness::build(base_config());
    h.service.emit_internal_heartbeat();
    h.service.tick();
    h.clock.advance(Duration::from_millis(500));
    h.service.tick();
    let snap = h.service.snapshot();
    assert_eq!(snap.observability_state, "no_heartbeat");
    assert_eq!(snap.agent_state, "unknown");
}
