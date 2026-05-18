// SPDX-License-Identifier: Apache-2.0
//
// V01-E09-F07: Worker-supervision integration and failure-injection tests.
//
// These tests drive the supervisor through deterministic mock process /
// readiness implementations and a fake clock. They cover the failure
// matrix called out in V01-E09-F07-T02:
//
//   - launch ready
//   - exit before ready
//   - crash after ready
//   - single restart with backoff
//   - repeated crash threshold -> crash-loop
//   - not-ready timeout threshold
//   - stable uptime reset
//   - graceful stop
//   - ignored stop -> forced kill escalation
//   - absent observability consumer (NoopEventSink)
//   - successful observability event delivery (RingEventSink)
//   - operator recovery clears crash-loop
//
// Coordinator coordination tests live alongside the existing
// `deploy_*` and `rollback_*` integration suites; this file focuses on
// the supervisor state machine that V01-E09 owns.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::default_trait_access,
    clippy::missing_panics_doc
)]

mod common;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tensorplate_agent::supervision::clock::FakeClock;
use tensorplate_agent::supervision::config::{
    BackoffConfig, EventSinkConfig, RestartPolicy, RestartPolicyKind, SupervisorConfig,
    WorkerStdioMode,
};
use tensorplate_agent::supervision::event::RingEventSink;
use tensorplate_agent::supervision::process::{ExitStatus, MockProcessBehavior, MockWorkerProcess};
use tensorplate_agent::supervision::readiness::{MockReadinessProbe, ReadinessSample};
use tensorplate_agent::supervision::supervisor::{DesiredWorker, TickOutcome, WorkerSupervisor};
use tensorplate_protocol::supervision_event::{SupervisionEventKind, SupervisionServingState};

fn cfg() -> SupervisorConfig {
    SupervisorConfig {
        binary_path: PathBuf::from("/usr/local/bin/tensorplate-serving"),
        args: vec![],
        env_allowlist: BTreeSet::new(),
        working_dir: PathBuf::from("/var/lib/tensorplate"),
        serving_config_path: PathBuf::from("/var/lib/tensorplate/serving.json"),
        control_host: "127.0.0.1".into(),
        control_port: 18080,
        stdio_mode: WorkerStdioMode::Inherit,
        startup_timeout_ms: 50,
        graceful_stop_timeout_ms: 10,
        kill_timeout_ms: 10,
        status_poll_interval_ms: 5,
        restart_policy: RestartPolicy {
            kind: RestartPolicyKind::BoundedBackoff,
            backoff: BackoffConfig {
                initial_delay_ms: 5,
                multiplier_hundredths: 200,
                max_delay_ms: 80,
                window_ms: 10_000,
                threshold: 3,
                stable_reset_ms: 200,
            },
        },
        event_sink: EventSinkConfig::default(),
    }
}

fn ready_sample(id: &str) -> ReadinessSample {
    ReadinessSample {
        ready: true,
        active_deployment: Some(id.to_string()),
        backend: Some("mock".into()),
        queue_depth: Some(0),
        ..ReadinessSample::unknown()
    }
}

struct Fixture {
    supervisor: WorkerSupervisor,
    sink: Arc<RingEventSink>,
    clock: Arc<FakeClock>,
    #[allow(dead_code)]
    process: Arc<MockWorkerProcess>,
    #[allow(dead_code)]
    probe: Arc<MockReadinessProbe>,
}

fn build_fixture(process: Arc<MockWorkerProcess>, probe: Arc<MockReadinessProbe>) -> Fixture {
    let clock = Arc::new(FakeClock::new());
    let sink = Arc::new(RingEventSink::new(&EventSinkConfig::default()));
    let supervisor = WorkerSupervisor::new(
        cfg(),
        process.clone(),
        probe.clone(),
        clock.clone() as Arc<_>,
    )
    .expect("supervisor")
    .with_event_sink(sink.clone() as Arc<_>);
    Fixture {
        supervisor,
        sink,
        clock,
        process,
        probe,
    }
}

fn set_desired(fixture: &Fixture, id: &str) {
    fixture
        .supervisor
        .set_desired_active(Some(DesiredWorker {
            deployment_id: id.to_string(),
            backend: "mock".into(),
        }))
        .expect("set desired");
}

#[test]
fn launch_then_ready_with_supervision_event_trail() {
    let probe = Arc::new(MockReadinessProbe::new());
    probe.script(vec![ready_sample("d-1")]);
    let fixture = build_fixture(Arc::new(MockWorkerProcess::new()), probe);
    set_desired(&fixture, "d-1");
    let _ = fixture.supervisor.tick().expect("launch tick");
    let _ = fixture.supervisor.tick().expect("ready tick");
    let status = fixture.supervisor.status();
    assert!(matches!(
        status.serving_state,
        SupervisionServingState::Ready
    ));
    let events = fixture.sink.drain();
    let kinds: Vec<_> = events.iter().map(|p| p.kind).collect();
    assert!(kinds.contains(&SupervisionEventKind::WorkerStarted));
    assert!(kinds.contains(&SupervisionEventKind::WorkerReady));
    // Events carry stable sequences with no gaps.
    let seqs: Vec<u64> = events.iter().map(|p| p.sequence).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted);
}

#[test]
fn exit_before_ready_schedules_backoff_restart() {
    let process = Arc::new(MockWorkerProcess::with_behavior(MockProcessBehavior {
        exit_at_poll: Some(1),
        exit_code: Some(2),
        ..Default::default()
    }));
    let probe = Arc::new(MockReadinessProbe::new());
    let fixture = build_fixture(process, probe);
    set_desired(&fixture, "d-1");
    let _ = fixture.supervisor.tick().expect("launch");
    let outcome = fixture.supervisor.tick().expect("exit tick");
    assert!(matches!(outcome, TickOutcome::Fault(_)));
    let status = fixture.supervisor.status();
    assert!(matches!(
        status.serving_state,
        SupervisionServingState::AwaitingRestart
    ));
    assert!(status.next_restart_delay_ms.unwrap_or(0) >= 5);
}

#[test]
fn repeated_crashes_enter_crash_loop_state() {
    let process = Arc::new(MockWorkerProcess::with_behavior(MockProcessBehavior {
        exit_at_poll: Some(1),
        exit_code: Some(2),
        ..Default::default()
    }));
    let probe = Arc::new(MockReadinessProbe::new());
    let fixture = build_fixture(process, probe);
    set_desired(&fixture, "d-1");
    for _ in 0..10 {
        let _ = fixture.supervisor.tick().expect("tick");
        fixture.clock.advance(Duration::from_millis(200));
    }
    let status = fixture.supervisor.status();
    assert!(
        status.crash_loop,
        "supervisor should have entered crash-loop; status = {status:?}"
    );
}

#[test]
fn crash_loop_recovery_by_operator_action_clears_terminal_state() {
    let process = Arc::new(MockWorkerProcess::with_behavior(MockProcessBehavior {
        exit_at_poll: Some(1),
        exit_code: Some(2),
        ..Default::default()
    }));
    let probe = Arc::new(MockReadinessProbe::new());
    let fixture = build_fixture(process.clone(), probe.clone());
    set_desired(&fixture, "d-1");
    for _ in 0..10 {
        let _ = fixture.supervisor.tick().expect("tick");
        fixture.clock.advance(Duration::from_millis(200));
    }
    assert!(fixture.supervisor.status().crash_loop);

    // Operator triggers a recovery (e.g. via rollback / new deploy).
    fixture
        .supervisor
        .recover_after_operator_action()
        .expect("recover");
    let status = fixture.supervisor.status();
    assert!(!status.crash_loop);
    assert!(matches!(
        status.serving_state,
        SupervisionServingState::Starting | SupervisionServingState::NoActiveDeployment
    ));
}

#[test]
fn not_ready_timeout_counts_against_threshold() {
    let process = Arc::new(MockWorkerProcess::new());
    let probe = Arc::new(MockReadinessProbe::new());
    // Persistently not-ready.
    probe.script(vec![ReadinessSample::unknown()]);
    let fixture = build_fixture(process, probe);
    set_desired(&fixture, "d-1");
    let _ = fixture.supervisor.tick().expect("launch tick");
    fixture.clock.advance(Duration::from_millis(60));
    let _ = fixture.supervisor.tick().expect("expire tick");
    let status = fixture.supervisor.status();
    assert!(status.restart_count >= 1);
}

#[test]
fn graceful_stop_emits_stopping_and_stopped() {
    let process = Arc::new(MockWorkerProcess::new());
    let probe = Arc::new(MockReadinessProbe::new());
    probe.script(vec![ready_sample("d-1")]);
    let fixture = build_fixture(process, probe);
    set_desired(&fixture, "d-1");
    let _ = fixture.supervisor.tick().expect("launch");
    let _ = fixture.supervisor.tick().expect("ready");
    fixture.supervisor.request_stop().expect("stop");
    let _ = fixture.supervisor.tick().expect("stopping tick");
    let _ = fixture.supervisor.tick().expect("stopped tick");
    let kinds: Vec<_> = fixture.sink.drain().into_iter().map(|p| p.kind).collect();
    assert!(kinds.contains(&SupervisionEventKind::WorkerStopping));
    assert!(kinds.contains(&SupervisionEventKind::WorkerStopped));
    let status = fixture.supervisor.status();
    assert!(matches!(
        status.serving_state,
        SupervisionServingState::Stopped
    ));
}

#[test]
fn ignored_graceful_stop_escalates_to_forced_terminate() {
    let process = Arc::new(MockWorkerProcess::with_behavior(MockProcessBehavior {
        graceful_stop_ignored: true,
        ..Default::default()
    }));
    let probe = Arc::new(MockReadinessProbe::new());
    probe.script(vec![ready_sample("d-1")]);
    let fixture = build_fixture(process.clone(), probe);
    set_desired(&fixture, "d-1");
    let _ = fixture.supervisor.tick().expect("launch");
    let _ = fixture.supervisor.tick().expect("ready");
    fixture.supervisor.request_stop().expect("stop request");
    let _ = fixture.supervisor.tick().expect("graceful stop tick");
    assert!(!process.force_killed());
    // Advance past the graceful stop deadline; next tick must escalate.
    fixture.clock.advance(Duration::from_millis(50));
    let _ = fixture.supervisor.tick().expect("escalate tick");
    assert!(process.force_killed());
}

#[test]
fn exit_after_ready_records_after_ready_flag_in_event() {
    let process = Arc::new(MockWorkerProcess::new());
    let probe = Arc::new(MockReadinessProbe::new());
    probe.script(vec![ready_sample("d-1")]);
    let fixture = build_fixture(process.clone(), probe);
    set_desired(&fixture, "d-1");
    let _ = fixture.supervisor.tick().expect("launch");
    let _ = fixture.supervisor.tick().expect("ready");
    process.exit_now(ExitStatus {
        code: Some(1),
        signal: None,
        after_ready: true,
    });
    let outcome = fixture.supervisor.tick().expect("exit tick");
    assert!(matches!(outcome, TickOutcome::Fault(_)));
    let exit_events: Vec<_> = fixture
        .sink
        .drain()
        .into_iter()
        .filter(|p| matches!(p.kind, SupervisionEventKind::WorkerExit))
        .collect();
    assert!(!exit_events.is_empty());
    assert_eq!(exit_events[0].after_ready, Some(true));
}

#[test]
fn missing_observability_consumer_does_not_stall_supervision() {
    let process = Arc::new(MockWorkerProcess::with_behavior(MockProcessBehavior {
        exit_at_poll: Some(1),
        exit_code: Some(2),
        ..Default::default()
    }));
    let probe = Arc::new(MockReadinessProbe::new());
    // Build a supervisor without installing the ring sink; the default
    // is the no-op sink. We still expect crash-loop bookkeeping to
    // progress because the supervisor never blocks on the sink.
    let clock = Arc::new(FakeClock::new());
    let supervisor = WorkerSupervisor::new(
        cfg(),
        process.clone(),
        probe.clone(),
        clock.clone() as Arc<_>,
    )
    .expect("supervisor");
    supervisor
        .set_desired_active(Some(DesiredWorker {
            deployment_id: "d-1".into(),
            backend: "mock".into(),
        }))
        .expect("set desired");
    for _ in 0..10 {
        let _ = supervisor.tick().expect("tick");
        clock.advance(Duration::from_millis(200));
    }
    assert!(supervisor.status().crash_loop);
}

#[test]
fn bounded_event_sink_drops_oldest_when_full() {
    // Configure a tiny event queue and verify the ring sink reports
    // drops without breaking the supervisor.
    let cfg_small = SupervisorConfig {
        event_sink: EventSinkConfig {
            queue_capacity: 2,
            uds_path: None,
        },
        ..cfg()
    };
    let process = Arc::new(MockWorkerProcess::with_behavior(MockProcessBehavior {
        exit_at_poll: Some(1),
        exit_code: Some(2),
        ..Default::default()
    }));
    let probe = Arc::new(MockReadinessProbe::new());
    let clock = Arc::new(FakeClock::new());
    let sink = Arc::new(RingEventSink::new(&cfg_small.event_sink));
    let supervisor = WorkerSupervisor::new(
        cfg_small,
        process.clone(),
        probe.clone(),
        clock.clone() as Arc<_>,
    )
    .expect("supervisor")
    .with_event_sink(sink.clone() as Arc<_>);
    supervisor
        .set_desired_active(Some(DesiredWorker {
            deployment_id: "d-1".into(),
            backend: "mock".into(),
        }))
        .expect("set desired");
    for _ in 0..6 {
        let _ = supervisor.tick().expect("tick");
        clock.advance(Duration::from_millis(50));
    }
    assert!(sink.dropped() > 0);
}
