// SPDX-License-Identifier: Apache-2.0
//
// V01-E09-F06-T02: Coordinator <-> Supervisor coordination.
//
// These tests cover the documented hand-off between the V01-E08 deploy
// transaction coordinator and the V01-E09 supervisor:
//
//   - A successful deploy installs the new active deployment as the
//     supervisor's desired state.
//   - A successful deploy is the documented recovery trigger that
//     clears crash-loop state.
//   - A rollback re-installs the previous active deployment.

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

use tensorplate_agent::coordinator::Coordinator;
use tensorplate_agent::supervision::clock::FakeClock;
use tensorplate_agent::supervision::config::{
    BackoffConfig, EventSinkConfig, RestartPolicy, RestartPolicyKind, SupervisorConfig,
    WorkerStdioMode,
};
use tensorplate_agent::supervision::process::{MockProcessBehavior, MockWorkerProcess};
use tensorplate_agent::supervision::readiness::{MockReadinessProbe, ReadinessSample};
use tensorplate_agent::supervision::supervisor::{DesiredWorker, WorkerSupervisor};
use tensorplate_protocol::supervision_event::SupervisionServingState;

fn supervisor_cfg() -> SupervisorConfig {
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
                threshold: 2,
                stable_reset_ms: 200,
            },
        },
        event_sink: EventSinkConfig::default(),
    }
}

#[test]
fn successful_deploy_promotes_supervisor_desired_active() {
    let harness = common::Harness::new();
    let process = Arc::new(MockWorkerProcess::new());
    let probe = Arc::new(MockReadinessProbe::new());
    let clock = Arc::new(FakeClock::new());
    let supervisor = Arc::new(
        WorkerSupervisor::new(
            supervisor_cfg(),
            process.clone(),
            probe.clone(),
            clock as Arc<_>,
        )
        .expect("supervisor"),
    );
    // Wrap the harness coordinator with a supervisor attachment.
    let store = harness.store.clone();
    let cfg = harness.config.clone();
    let coord =
        Coordinator::new(cfg, store, harness.worker.clone()).with_supervisor(supervisor.clone());

    let bundle = common::vision_bundle(harness.td.path(), "deploy-1");
    coord
        .deploy(
            "deploy-1",
            &bundle,
            std::collections::BTreeMap::new(),
            None,
            None,
        )
        .expect("deploy ok");
    let status = supervisor.status();
    assert_eq!(status.desired_active.as_deref(), Some("deploy-1"));
    let agent_status = coord.status().expect("status");
    assert_eq!(
        agent_status
            .supervision
            .as_ref()
            .and_then(|s| s.desired_active.as_deref()),
        Some("deploy-1")
    );
}

#[test]
fn deploy_resets_supervisor_crash_loop_state() {
    let harness = common::Harness::new();
    let process = Arc::new(MockWorkerProcess::with_behavior(MockProcessBehavior {
        exit_at_poll: Some(1),
        exit_code: Some(2),
        ..Default::default()
    }));
    let probe = Arc::new(MockReadinessProbe::new());
    let clock = Arc::new(FakeClock::new());
    let supervisor = Arc::new(
        WorkerSupervisor::new(
            supervisor_cfg(),
            process.clone(),
            probe.clone(),
            clock.clone() as Arc<_>,
        )
        .expect("supervisor"),
    );
    // Drive supervisor into crash-loop by attaching a desired active
    // and ticking until the threshold trips.
    supervisor
        .set_desired_active(Some(DesiredWorker {
            deployment_id: "stuck".into(),
            backend: "mock".into(),
        }))
        .expect("desired");
    for _ in 0..8 {
        let _ = supervisor.tick().expect("tick");
        clock.advance(Duration::from_millis(200));
    }
    assert!(supervisor.status().crash_loop);

    // Successful deploy of a fresh bundle must clear crash-loop.
    let coord = Coordinator::new(
        harness.config.clone(),
        harness.store.clone(),
        harness.worker.clone(),
    )
    .with_supervisor(supervisor.clone());
    let bundle = common::vision_bundle(harness.td.path(), "recover-1");
    coord
        .deploy(
            "recover-1",
            &bundle,
            std::collections::BTreeMap::new(),
            None,
            None,
        )
        .expect("deploy ok");
    let status = supervisor.status();
    assert!(!status.crash_loop);
    assert_eq!(status.desired_active.as_deref(), Some("recover-1"));
}

#[test]
fn rollback_restores_supervisor_desired_active() {
    let harness = common::Harness::new();
    let process = Arc::new(MockWorkerProcess::new());
    let probe = Arc::new(MockReadinessProbe::new());
    probe.script(vec![ReadinessSample::unknown()]);
    let clock = Arc::new(FakeClock::new());
    let supervisor = Arc::new(
        WorkerSupervisor::new(
            supervisor_cfg(),
            process.clone(),
            probe.clone(),
            clock as Arc<_>,
        )
        .expect("supervisor"),
    );
    let coord = Coordinator::new(
        harness.config.clone(),
        harness.store.clone(),
        harness.worker.clone(),
    )
    .with_supervisor(supervisor.clone());

    let bundle1 = common::vision_bundle(harness.td.path(), "deploy-1");
    coord
        .deploy(
            "deploy-1",
            &bundle1,
            std::collections::BTreeMap::new(),
            None,
            None,
        )
        .expect("deploy-1");
    let bundle2 = common::vision_bundle(harness.td.path(), "deploy-2");
    coord
        .deploy(
            "deploy-2",
            &bundle2,
            std::collections::BTreeMap::new(),
            None,
            None,
        )
        .expect("deploy-2");
    coord.rollback(None).expect("rollback");
    let status = supervisor.status();
    assert_eq!(status.desired_active.as_deref(), Some("deploy-1"));
    assert!(matches!(
        status.serving_state,
        SupervisionServingState::Starting
            | SupervisionServingState::NoActiveDeployment
            | SupervisionServingState::Stopped
    ));
}
