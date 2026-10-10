// SPDX-License-Identifier: Apache-2.0
//
// What the member registry's shutdown and restart limit leave in the durable
// state, and what the next agent process makes of it.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use common::Harness;
use tensorplate_agent::coordinator::Coordinator;
use tensorplate_agent::error::{AgentError, AgentResult};
use tensorplate_agent::recovery;
use tensorplate_agent::registry::{
    durable_generations, ControlContact, HealthProbe, LaunchSpec, MemberProcess, MemberRegistry,
    ProcessLauncher, RegistrySettings,
};
use tensorplate_agent::state::StateStore;
use tensorplate_agent::supervision::FakeClock;
use tensorplate_agent::worker::{MemberEvent, WorkerControl};
use tensorplate_protocol::agent_control::{AgentRunState, RecoveryAction};
use tensorplate_protocol::deploy_transaction::DeployState;
use tensorplate_protocol::ErrorCode;

const HELD: Duration = Duration::from_secs(10);

struct Process {
    exited: Arc<AtomicBool>,
}

impl MemberProcess for Process {
    fn exited(&mut self) -> AgentResult<bool> {
        Ok(self.exited.load(Ordering::SeqCst))
    }
    fn startup_failure(&mut self) -> AgentError {
        AgentError::WorkerExited("exit status: 1".into())
    }
    fn control(&self) -> ControlContact {
        ControlContact::Reporting
    }
    fn terminate(&mut self) {
        self.exited.store(true, Ordering::SeqCst);
    }
    fn kill(&mut self) {
        self.exited.store(true, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct Launcher {
    /// Whether each launched process has exited, in launch order.
    launched: Mutex<Vec<Arc<AtomicBool>>>,
    /// The next launch says it has started, then waits to be let finish.
    hold: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
}

impl Launcher {
    fn running(&self) -> usize {
        let launched = self.launched.lock().unwrap();
        launched
            .iter()
            .filter(|exited| !exited.load(Ordering::SeqCst))
            .count()
    }
    fn count(&self) -> usize {
        self.launched.lock().unwrap().len()
    }
    fn exit_last(&self) {
        let launched = self.launched.lock().unwrap();
        launched.last().unwrap().store(true, Ordering::SeqCst);
    }
}

impl ProcessLauncher for Launcher {
    fn launch(&self, _spec: &LaunchSpec) -> AgentResult<Box<dyn MemberProcess>> {
        let held = self.hold.lock().unwrap().take();
        if let Some((started, release)) = held {
            started.send(()).unwrap();
            release.recv().unwrap();
        }
        let exited = Arc::new(AtomicBool::new(false));
        self.launched.lock().unwrap().push(exited.clone());
        Ok(Box::new(Process { exited }))
    }
}

/// Every port reports `stt` ready.
struct Ready;

impl HealthProbe for Ready {
    fn ready_model(&self, _port: u16, _timeout: Duration) -> AgentResult<Option<String>> {
        Ok(Some("stt".into()))
    }
}

/// One agent process over the harness's state directory: its own store
/// handle, registry and coordinator.
struct Agent {
    store: Arc<StateStore>,
    launcher: Arc<Launcher>,
    clock: Arc<FakeClock>,
    registry: Arc<MemberRegistry>,
    coord: Coordinator,
}

fn start_agent(h: &Harness) -> Agent {
    let store = Arc::new(StateStore::open(&h.config.state_dir).expect("open the state"));
    let launcher = Arc::new(Launcher::default());
    let clock = Arc::new(FakeClock::new());
    let settings = RegistrySettings {
        bind_host: "127.0.0.1".into(),
        active_port: 41_180,
        candidate_port: 41_181,
        config_dir: h.td.path().join("worker-configs"),
        use_mock_session: true,
        status_poll_interval: Duration::from_millis(1),
        worker_drain: Duration::from_millis(30),
        stop_deadline: Duration::from_millis(60),
        restart_warm_timeout: Duration::from_secs(30),
    };
    let registry = Arc::new(
        MemberRegistry::new(
            settings,
            launcher.clone(),
            Arc::new(Ready),
            durable_generations(store.clone()),
        )
        .with_clock(clock.clone()),
    );
    let coord = Coordinator::new(h.config.clone(), store.clone(), registry.clone());
    Agent {
        store,
        launcher,
        clock,
        registry,
        coord,
    }
}

fn deploy(agent: &Agent, bundle: &Path) -> AgentResult<()> {
    agent
        .coord
        .deploy("stt", bundle, BTreeMap::new(), None, None)
        .map(|_| ())
}

#[test]
fn a_deploy_held_in_prepare_when_the_agent_stops_stays_in_flight_and_the_next_start_resumes_it() {
    let h = Harness::new();
    let bundle = common::vision_bundle(h.td.path(), "stt");
    let agent = start_agent(&h);
    let (started, has_started) = mpsc::channel();
    let (release, released) = mpsc::channel();
    *agent.launcher.hold.lock().unwrap() = Some((started, released));

    let answer = std::thread::scope(|scope| {
        // Dropped if this closure panics, which lets the held launch end.
        let release = release;
        let deploying = scope.spawn(|| deploy(&agent, &bundle));
        has_started.recv_timeout(HELD).expect("the launch starts");
        let (done, is_done) = mpsc::channel();
        let stopping = &agent;
        scope.spawn(move || {
            stopping.registry.shutdown();
            assert_eq!(stopping.launcher.running(), 0, "a worker outlived shutdown");
            done.send(()).unwrap();
        });
        assert!(
            is_done.recv_timeout(Duration::from_millis(100)).is_err(),
            "shutdown returned while a worker was starting"
        );
        release.send(()).unwrap();
        let answer = deploying.join().unwrap();
        is_done.recv_timeout(HELD).expect("shutdown returns");
        answer
    });

    let err = answer.expect_err("the deploy does not complete");
    assert!(matches!(err, AgentError::Stopping), "{err}");
    assert_eq!(err.code(), ErrorCode::Unavailable);
    assert_eq!(agent.launcher.count(), 1);
    // What an agent killed at this point leaves: the transaction as its
    // last phase wrote it, no failure, nothing quarantined.
    let state = agent.store.snapshot().expect("state");
    let tx = state.in_flight_transaction.expect("still in flight");
    assert_eq!(tx.phase, DeployState::CapacityChecked);
    assert_eq!(tx.failure, None);
    assert_eq!(state.last_error, None);
    assert!(state.quarantined.is_empty());
    assert_eq!(state.active, None);

    // No transaction begins once the agent is stopping.
    let err = deploy(&agent, &bundle).expect_err("refused");
    assert!(matches!(err, AgentError::Stopping), "{err}");
    let err = agent.coord.rollback(None).expect_err("refused");
    assert!(matches!(err, AgentError::Stopping), "{err}");
    let state = agent.store.snapshot().expect("state");
    assert_eq!(
        state.in_flight_transaction.map(|tx| tx.transaction_id),
        Some(tx.transaction_id)
    );
    assert_eq!(agent.launcher.count(), 1);
    drop(agent);

    let next = start_agent(&h);
    let summary = recovery::apply_startup(&next.coord).expect("recovery");

    assert_eq!(summary.action, RecoveryAction::ResumePrepare);
    let state = next.store.snapshot().expect("state");
    assert_eq!(state.in_flight_transaction, None);
    assert_eq!(
        state.active.map(|active| active.deployment_id).as_deref(),
        Some("stt")
    );
    assert_eq!(next.launcher.running(), 1);
}

/// Stop the serving worker and step the registry until its successor serves.
fn crash_and_recover(agent: &Agent) {
    agent.launcher.exit_last();
    let events = agent.registry.supervise();
    let Some(MemberEvent::RestartDue { after, .. }) = events.last() else {
        panic!("no restart is owed: {events:?}");
    };
    agent.clock.advance(*after);
    agent.registry.supervise();
    let events = agent.registry.supervise();
    assert!(
        matches!(&events[..], [MemberEvent::Serving { .. }]),
        "{events:?}"
    );
}

/// The failed state is the registry's, in memory: the state file does not
/// carry it, so the next agent process restores the recorded deployment and
/// counts its restarts from zero.
#[test]
fn a_failed_member_is_reported_from_memory_and_the_next_agent_start_serves_it_again() {
    let h = Harness::new();
    let bundle = common::vision_bundle(h.td.path(), "stt");
    let agent = start_agent(&h);
    deploy(&agent, &bundle).expect("deploy");
    for _ in 0..3 {
        crash_and_recover(&agent);
    }
    agent.launcher.exit_last();
    let events = agent.registry.supervise();
    assert!(
        matches!(events.last(), Some(MemberEvent::Failed { .. })),
        "{events:?}"
    );

    let status = agent.coord.status().expect("status");
    assert_eq!(status.agent_state, AgentRunState::Degraded);
    let reported = status.last_error.expect("the failure is reported");
    assert!(reported.message.contains("3 restarts"), "{reported:?}");
    let state = agent.store.snapshot().expect("state");
    assert_eq!(state.last_error, None);
    assert_eq!(
        state.active.map(|active| active.deployment_id).as_deref(),
        Some("stt")
    );
    assert_eq!(agent.launcher.running(), 0);
    drop(agent);

    let next = start_agent(&h);
    let summary = recovery::apply_startup(&next.coord).expect("recovery");

    assert_eq!(summary.action, RecoveryAction::RestoreActive);
    let status = next.coord.status().expect("status");
    assert_eq!(status.agent_state, AgentRunState::Ready);
    assert_eq!(status.last_error, None);
    assert_eq!(next.registry.member_failure(), None);
    assert_eq!(next.launcher.running(), 1);
    next.launcher.exit_last();
    let events = next.registry.supervise();
    assert!(
        matches!(
            events.last(),
            Some(MemberEvent::RestartDue { after, .. }) if *after == Duration::from_secs(1)
        ),
        "{events:?}"
    );
}
