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
use tensorplate_agent::control::dispatch;
use tensorplate_agent::coordinator::Coordinator;
use tensorplate_agent::error::{AgentError, AgentResult};
use tensorplate_agent::recovery;
use tensorplate_agent::registry::{
    durable_generations, ControlContact, HealthProbe, LaunchSpec, MemberPorts, MemberProcess,
    MemberRegistry, ProcessLauncher, RegistrySettings,
};
use tensorplate_agent::state::StateStore;
use tensorplate_agent::supervision::FakeClock;
use tensorplate_agent::worker::{MemberEvent, WorkerControl};
use tensorplate_protocol::agent_control::{
    AgentRunState, ControlRequest, ControlResponse, DeployRequest, RecoveryAction, ResponseStatus,
    RollbackRequest,
};
use tensorplate_protocol::agent_state::TransactionKind;
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
    ready: Arc<Ready>,
    /// Workers launched from now on never become ready.
    never_ready: AtomicBool,
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
    fn exit_first(&self) {
        let launched = self.launched.lock().unwrap();
        launched.first().unwrap().store(true, Ordering::SeqCst);
    }
}

impl ProcessLauncher for Launcher {
    fn launch(&self, spec: &LaunchSpec) -> AgentResult<Box<dyn MemberProcess>> {
        *self.ready.0.lock().unwrap() =
            (!self.never_ready.load(Ordering::SeqCst)).then(|| spec.member.deployment_id.clone());
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

/// Every port reports the model of the launch before it ready.
#[derive(Default)]
struct Ready(Mutex<Option<String>>);

impl HealthProbe for Ready {
    fn ready_model(&self, _port: u16, _timeout: Duration) -> AgentResult<Option<String>> {
        Ok(self.0.lock().unwrap().clone())
    }
}

/// One agent process over the harness's state directory: its own store
/// handle, registry and coordinator.
struct Agent {
    store: Arc<StateStore>,
    launcher: Arc<Launcher>,
    clock: Arc<FakeClock>,
    registry: Arc<MemberRegistry>,
    coord: Arc<Coordinator>,
}

fn start_agent(h: &Harness) -> Agent {
    let store = Arc::new(StateStore::open(&h.config.state_dir).expect("open the state"));
    let launcher = Arc::new(Launcher::default());
    let clock = Arc::new(FakeClock::new());
    let settings = RegistrySettings {
        bind_host: "127.0.0.1".into(),
        ports: [41_180, 41_181]
            .map(|http| MemberPorts { http, stream: None })
            .to_vec(),
        config_dir: h.td.path().join("worker-configs"),
        use_mock_session: true,
        status_poll_interval: Duration::from_millis(1),
        worker_drain: Duration::from_millis(30),
        stop_deadline: Duration::from_millis(60),
        launch_grace: HELD,
        restart_warm_timeout: Duration::from_secs(30),
    };
    let registry = Arc::new(
        MemberRegistry::new(
            settings,
            launcher.clone(),
            launcher.ready.clone(),
            durable_generations(store.clone()),
        )
        .with_clock(clock.clone()),
    );
    let coord = Arc::new(Coordinator::new(
        h.config.clone(),
        store.clone(),
        registry.clone(),
    ));
    Agent {
        store,
        launcher,
        clock,
        registry,
        coord,
    }
}

fn deploy(agent: &Agent, bundle: &Path) -> AgentResult<()> {
    deploy_as(agent, "stt", bundle)
}

fn deploy_as(agent: &Agent, id: &str, bundle: &Path) -> AgentResult<()> {
    agent
        .coord
        .deploy(id, bundle, BTreeMap::new(), None, None)
        .map(|_| ())
}

/// Hold the next launch; returns what tells that it began and what lets
/// it finish.
fn hold_next_launch(agent: &Agent) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (started, has_started) = mpsc::channel();
    let (release, released) = mpsc::channel();
    *agent.launcher.hold.lock().unwrap() = Some((started, released));
    (has_started, release)
}

/// Stop `agent`'s registry while `request` is held in a launch; returns
/// the request's answer once both have ended.
fn stop_during<T: Send>(
    agent: &Agent,
    request: impl FnOnce() -> AgentResult<T> + Send,
) -> AgentResult<T> {
    let (has_started, release) = hold_next_launch(agent);
    std::thread::scope(|scope| {
        // Dropped if this closure panics, which lets the held launch end.
        let release = release;
        let requesting = scope.spawn(request);
        has_started.recv_timeout(HELD).expect("the launch starts");
        let (done, is_done) = mpsc::channel();
        scope.spawn(move || {
            agent.registry.shutdown();
            assert_eq!(agent.launcher.running(), 0, "a worker outlived shutdown");
            done.send(()).unwrap();
        });
        assert!(
            is_done.recv_timeout(Duration::from_millis(100)).is_err(),
            "shutdown returned while a worker was starting"
        );
        release.send(()).unwrap();
        let answer = requesting.join().unwrap();
        is_done.recv_timeout(HELD).expect("shutdown returns");
        answer
    })
}

#[test]
fn a_deploy_held_in_prepare_when_the_agent_stops_stays_in_flight_and_the_next_start_resumes_it() {
    let h = Harness::new();
    let bundle = common::vision_bundle(h.td.path(), "stt");
    let agent = start_agent(&h);
    let answer = stop_during(&agent, || deploy(&agent, &bundle));

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

#[test]
fn a_rollback_held_in_prepare_when_the_agent_stops_stays_in_flight() {
    let h = Harness::new();
    let agent = start_agent(&h);
    deploy_as(&agent, "stt", &common::vision_bundle(h.td.path(), "stt")).expect("deploy");
    deploy_as(&agent, "tts", &common::vision_bundle(h.td.path(), "tts")).expect("deploy");

    let answer = stop_during(&agent, || agent.coord.rollback(None));

    let err = answer.expect_err("the rollback does not complete");
    assert!(matches!(err, AgentError::Stopping), "{err}");
    let state = agent.store.snapshot().expect("state");
    let tx = state.in_flight_transaction.expect("still in flight");
    assert_eq!(tx.kind, TransactionKind::Rollback);
    assert_eq!(tx.phase, DeployState::CapacityChecked);
    assert_eq!(tx.failure, None);
    assert_eq!(state.last_error, None);
    assert_eq!(
        state.active.map(|active| active.deployment_id).as_deref(),
        Some("tts")
    );
    drop(agent);

    // The next start completes the rollback, as it does after a kill; the
    // caller was answered `unavailable` and may not expect it.
    let next = start_agent(&h);
    let summary = recovery::apply_startup(&next.coord).expect("recovery");

    assert_eq!(summary.action, RecoveryAction::ResumePrepare);
    let state = next.store.snapshot().expect("state");
    assert_eq!(state.in_flight_transaction, None);
    assert_eq!(
        state.active.map(|active| active.deployment_id).as_deref(),
        Some("stt")
    );
}

/// Startup recovery quarantines a candidate interrupted in a worker-side
/// phase and starts nothing in that pass, after a stop as after a kill: the
/// deployment that was serving comes back at the start after that.
#[test]
fn a_deploy_stopped_while_it_warms_is_quarantined_by_the_next_start_which_serves_nothing() {
    let h = Harness::new();
    let agent = start_agent(&h);
    deploy_as(&agent, "stt", &common::vision_bundle(h.td.path(), "stt")).expect("deploy");
    let bundle = common::vision_bundle(h.td.path(), "tts");
    agent.launcher.never_ready.store(true, Ordering::SeqCst);

    let answer = std::thread::scope(|scope| {
        let deploying = scope.spawn(|| deploy_as(&agent, "tts", &bundle));
        let deadline = std::time::Instant::now() + HELD;
        let warming = |agent: &Agent| {
            let state = agent.store.snapshot().expect("state");
            state
                .in_flight_transaction
                .is_some_and(|tx| tx.phase == DeployState::Prepared)
        };
        while !warming(&agent) {
            assert!(
                std::time::Instant::now() < deadline,
                "the deploy never warmed"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        agent.registry.shutdown();
        deploying.join().unwrap()
    });

    let err = answer.expect_err("the deploy does not complete");
    assert!(matches!(err, AgentError::Stopping), "{err}");
    assert_eq!(agent.launcher.running(), 0);
    let state = agent.store.snapshot().expect("state");
    let tx = state.in_flight_transaction.expect("still in flight");
    assert_eq!(tx.phase, DeployState::Prepared);
    assert_eq!(tx.failure, None);
    assert_eq!(state.last_error, None);
    assert!(state.quarantined.is_empty());
    drop(agent);

    let next = start_agent(&h);
    let summary = recovery::apply_startup(&next.coord).expect("recovery");

    assert_eq!(summary.action, RecoveryAction::QuarantineCandidate);
    assert_eq!(next.launcher.count(), 0);
    let state = next.store.snapshot().expect("state");
    assert_eq!(state.in_flight_transaction, None);
    assert_eq!(state.quarantined.len(), 1);
    assert_eq!(
        state.active.map(|active| active.deployment_id).as_deref(),
        Some("stt")
    );
    drop(next);

    let after = start_agent(&h);
    let summary = recovery::apply_startup(&after.coord).expect("recovery");

    assert_eq!(summary.action, RecoveryAction::RestoreActive);
    assert_eq!(after.launcher.running(), 1);
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

fn deploy_request(id: &str, bundle: &Path) -> ControlRequest {
    ControlRequest::deploy(
        None,
        DeployRequest {
            bundle_path: bundle.display().to_string(),
            deployment_id: id.into(),
            ..DeployRequest::default()
        },
    )
}

/// What a caller is sent while the agent stops: `error` with the code
/// `unavailable`. The `unavailable` status is the refusal of an operation
/// that cannot apply, which carries `unsupported` and is not retried.
fn assert_answered_stopping(answer: &ControlResponse) {
    let sent = serde_json::to_value(answer).expect("encode");
    assert_eq!(sent["status"], "error", "{sent}");
    assert_eq!(sent["error"]["code"], "unavailable", "{sent}");
    assert_eq!(sent["error"]["message"], "the agent is stopping", "{sent}");
    assert_eq!(answer.status, ResponseStatus::Error);
    assert_eq!(answer.transaction_id, None);
}

#[test]
fn a_caller_is_answered_error_with_the_code_unavailable_while_the_agent_stops() {
    let h = Harness::new();
    let agent = start_agent(&h);
    deploy_as(&agent, "stt", &common::vision_bundle(h.td.path(), "stt")).expect("deploy");
    deploy_as(&agent, "tts", &common::vision_bundle(h.td.path(), "tts")).expect("deploy");
    let bundle = common::vision_bundle(h.td.path(), "third");

    let interrupted = stop_during(&agent, || {
        dispatch(&agent.coord, deploy_request("third", &bundle))
    });

    assert_answered_stopping(&interrupted.expect("an answer"));
    let in_flight = agent.store.snapshot().expect("state").in_flight_transaction;
    let in_flight = in_flight.expect("left in flight").transaction_id;
    for request in [
        deploy_request("third", &bundle),
        ControlRequest::rollback(None, RollbackRequest::default()),
    ] {
        let refused = dispatch(&agent.coord, request).expect("an answer");
        assert_answered_stopping(&refused);
    }
    let state = agent.store.snapshot().expect("state");
    assert_eq!(
        state.in_flight_transaction.map(|tx| tx.transaction_id),
        Some(in_flight)
    );
    assert!(state.quarantined.is_empty());
}

/// Make every state write fail before it renames anything, until the
/// returned guard is dropped.
struct FailedWrites(Vec<std::path::PathBuf>);

fn fail_state_writes(h: &Harness) -> FailedWrites {
    let temps = ["state.json.tmp", "state.json.bak.tmp"].map(|name| h.config.state_dir.join(name));
    for temp in &temps {
        std::fs::create_dir(temp).expect("occupy the temp file's name");
    }
    FailedWrites(temps.to_vec())
}

impl Drop for FailedWrites {
    fn drop(&mut self) {
        for temp in &self.0 {
            let _ = std::fs::remove_dir(temp);
        }
    }
}

/// Run `request`, whose candidate is prepared and whose next state write
/// then fails; the serving worker has exited by the time it returns.
fn fail_the_write_after_prepare<T: Send>(
    h: &Harness,
    agent: &Agent,
    request: impl FnOnce() -> AgentResult<T> + Send,
) -> AgentResult<T> {
    let (has_started, release) = hold_next_launch(agent);
    std::thread::scope(|scope| {
        let release = release;
        let requesting = scope.spawn(request);
        has_started.recv_timeout(HELD).expect("the launch starts");
        let failing = fail_state_writes(h);
        agent.launcher.exit_first();
        release.send(()).unwrap();
        let answer = requesting.join().unwrap();
        drop(failing);
        answer
    })
}

fn assert_the_serving_member_is_restarted(agent: &Agent, launches: usize) {
    let events = agent.registry.supervise();
    let Some(MemberEvent::RestartDue { after, .. }) = events.last() else {
        panic!("no restart is owed: {events:?}");
    };
    agent.clock.advance(*after);
    let events = agent.registry.supervise();
    assert!(
        matches!(&events[..], [MemberEvent::Restarted { .. }]),
        "the prepared candidate still holds the member: {events:?}"
    );
    assert_eq!(agent.launcher.count(), launches);
}

#[test]
fn a_deploy_that_returns_through_a_failed_state_write_releases_the_member_it_prepared_for() {
    let h = Harness::new();
    let agent = start_agent(&h);
    deploy_as(&agent, "stt", &common::vision_bundle(h.td.path(), "stt")).expect("deploy");
    let bundle = common::vision_bundle(h.td.path(), "tts");

    let answer = fail_the_write_after_prepare(&h, &agent, || deploy_as(&agent, "tts", &bundle));

    let err = answer.expect_err("the state write failed");
    assert!(matches!(err, AgentError::Io(_)), "{err}");
    let state = agent.store.snapshot().expect("state");
    let tx = state.in_flight_transaction.expect("still in flight");
    assert_eq!(tx.phase, DeployState::CapacityChecked);
    assert_the_serving_member_is_restarted(&agent, 3);
}

#[test]
fn a_rollback_that_returns_through_a_failed_state_write_releases_the_member_it_prepared_for() {
    let h = Harness::new();
    let agent = start_agent(&h);
    deploy_as(&agent, "stt", &common::vision_bundle(h.td.path(), "stt")).expect("deploy");
    deploy_as(&agent, "tts", &common::vision_bundle(h.td.path(), "tts")).expect("deploy");
    // The worker serving `tts` is the one that exits.
    agent.launcher.launched.lock().unwrap().remove(0);

    let answer = fail_the_write_after_prepare(&h, &agent, || agent.coord.rollback(None));

    let err = answer.expect_err("the state write failed");
    assert!(matches!(err, AgentError::Io(_)), "{err}");
    assert_the_serving_member_is_restarted(&agent, 3);
}
