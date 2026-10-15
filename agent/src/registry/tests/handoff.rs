// SPDX-License-Identifier: Apache-2.0

//! Transactions that hand the registry a generation, driven by the
//! coordinator over the durable state, against a restart of the serving
//! member.

use super::*;
use tensorplate_protocol::agent_control::AgentRunState;
use tensorplate_protocol::agent_state::{AgentState, DeploymentRecord};
use tensorplate_protocol::bundle_profile::BundleRuleCode;
use tensorplate_protocol::deploy_transaction::DeployState;
use tensorplate_protocol::ErrorCode;

use crate::coordinator::{Coordinator, EventSink};
use crate::descriptor_store::restart_stamp;
use crate::descriptor_store::tests::{agent_config, probes};
use crate::error::BundleManifestFailure;
use crate::recovery::apply_startup;
use crate::worker::WorkerEvent;

const STT: &str = "v0_2/speech_stt_streaming";
const TTS: &str = "v0_2/speech_tts_streaming";
const FORMAT_0_1: &str = "v0_1/x86_fixture_smoke";

/// Where the agent's loop runs a supervision step during a transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Step {
    /// As the hold is asked for, before the registry has it: the last
    /// moment a restart can begin.
    BeforeTheHold,
    /// Between the transaction's state update and its `prepare`.
    BeforePrepare,
}

/// The registry, with a supervision step run at `step` of each transaction.
struct Stepped {
    f: Arc<Fixture>,
    step: Step,
    /// What each of those steps did.
    steps: Mutex<Vec<Vec<MemberEvent>>>,
    /// Runs once, before the candidate is fenced.
    before_fence: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs once, before the candidate is activated.
    before_activate: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs once, as a transaction reports its failure.
    on_failure: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs once, before the serving member is read.
    before_serving_member: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// The next warm's budget, in place of the configured timeout, and
    /// doubled until that warm has probed.
    next_warm: Mutex<Option<Duration>>,
    /// While set, `prepare` answers [`refused_by_rule`].
    refuse_prepare: AtomicBool,
}

/// A prepare the staged manifest refuses: its record names the rule.
fn refused_by_rule() -> AgentError {
    AgentError::BundleManifest(BundleManifestFailure {
        rule: Some(BundleRuleCode::RequiredField),
        message: "a required field is missing".into(),
    })
}

impl Stepped {
    fn step(&self, at: Step) {
        if self.step == at {
            let stepped = self.f.registry.supervise();
            self.steps.lock().unwrap().push(stepped);
        }
    }
}

impl WorkerControl for Stepped {
    fn prepare(&self, tx: &str, candidate: &CandidateRef, timeout: Duration) -> AgentResult<()> {
        if self.refuse_prepare.load(Ordering::SeqCst) {
            return Err(refused_by_rule());
        }
        self.step(Step::BeforePrepare);
        self.f.registry.prepare(tx, candidate, timeout)
    }
    fn warm(
        &self,
        tx: &str,
        candidate: &CandidateRef,
        timeout: Duration,
    ) -> AgentResult<WorkerReadiness> {
        let next = self.next_warm.lock().unwrap().take();
        match next {
            Some(budget) => self.f.warm_until_probed(tx, candidate, budget),
            None => self.f.registry.warm(tx, candidate, timeout),
        }
    }
    fn fence(&self, tx: &str, candidate: &CandidateRef) -> AgentResult<()> {
        if let Some(hook) = self.before_fence.lock().unwrap().take() {
            hook();
        }
        self.f.registry.fence(tx, candidate)
    }
    fn activate(&self, tx: &str, candidate: &CandidateRef) -> AgentResult<()> {
        if let Some(hook) = self.before_activate.lock().unwrap().take() {
            hook();
        }
        self.f.registry.activate(tx, candidate)
    }
    fn owe_restore(&self, candidate: &CandidateRef, error: ErrorRecord) -> bool {
        self.f.registry.owe_restore(candidate, error)
    }
    fn abandon(&self, candidate: &CandidateRef) {
        self.f.registry.abandon(candidate);
    }
    fn hold(&self) {
        self.step(Step::BeforeTheHold);
        self.f.registry.hold();
    }
    fn release(&self) {
        self.f.registry.release();
    }
    fn unload(&self, deployment_id: &str) {
        self.f.registry.unload(deployment_id);
    }
    fn active_deployment_id(&self) -> AgentResult<Option<String>> {
        self.f.registry.active_deployment_id()
    }
    fn serving_member(&self) -> AgentResult<Option<ServingMember>> {
        if let Some(hook) = self.before_serving_member.lock().unwrap().take() {
            hook();
        }
        self.f.registry.serving_member()
    }
    fn stopping(&self) -> bool {
        self.f.registry.stopping()
    }
    fn supervise(&self) -> Vec<MemberEvent> {
        self.f.registry.supervise()
    }
    fn member_failure(&self) -> Option<ErrorRecord> {
        self.f.registry.member_failure()
    }
}

/// A coordinator over a registry wired to the durable state as the agent
/// wires it, both speech runner profiles installed.
struct Wired {
    f: Arc<Fixture>,
    /// Holds the state and the staged bundles.
    root: Arc<TempDir>,
    store: Arc<StateStore>,
    worker: Arc<Stepped>,
    coordinator: Coordinator,
}

fn wired() -> Wired {
    wired_with(Step::BeforePrepare, |_| {})
}

/// The fixture's two ports, each given a stream port when the agent's
/// configuration says it holds them (its range itself is not used).
fn with_stream_ports(settings: RegistrySettings, streams: bool) -> RegistrySettings {
    let ports = settings.ports.iter().map(|ports| MemberPorts {
        stream: streams.then_some(ports.http + 10),
        ..*ports
    });
    RegistrySettings {
        ports: ports.collect(),
        ..settings
    }
}

fn wired_with(step: Step, configure: impl FnOnce(&mut AgentConfig)) -> Wired {
    wired_at(Arc::new(TempDir::new().unwrap()), step, configure)
}

/// One start of the agent over the state and the staged bundles in `root`:
/// its registry holds no worker.
fn wired_at(root: Arc<TempDir>, step: Step, configure: impl FnOnce(&mut AgentConfig)) -> Wired {
    let mut config = agent_config(root.path());
    configure(&mut config);
    let store = Arc::new(StateStore::open(&config.state_dir).unwrap());
    let descriptors = Arc::new(DescriptorStore::new(store.state_dir()));
    let stamp = restart_stamp(store.clone(), descriptors.clone());
    let wire = |mut registry: MemberRegistry| {
        registry.generations = durable_generations(store.clone());
        let stamped = registry.with_restart_stamp(stamp);
        stamped.with_descriptor_store(descriptors)
    };
    let streams = config.worker.holds_stream_ports();
    let f = Arc::new(fixture_with(
        |settings| with_stream_ports(settings, streams),
        wire,
    ));
    // The first deployment is given the first port, the next the second.
    f.health(ACTIVE_PORT, "stt");
    f.health(CANDIDATE_PORT, "tts");
    let steps = Mutex::default();
    let worker = Arc::new(Stepped {
        f: f.clone(),
        step,
        steps,
        before_fence: Mutex::default(),
        before_activate: Mutex::default(),
        on_failure: Mutex::default(),
        before_serving_member: Mutex::default(),
        next_warm: Mutex::default(),
        refuse_prepare: AtomicBool::new(false),
    });
    let failed = worker.clone();
    let sink: Arc<EventSink> = Arc::new(move |event: &WorkerEvent| {
        if matches!(event, WorkerEvent::Failure { .. }) {
            if let Some(hook) = failed.on_failure.lock().unwrap().take() {
                hook();
            }
        }
    });
    let coordinator = Coordinator::new(config, store.clone(), worker.clone());
    Wired {
        f,
        root,
        store,
        worker,
        coordinator: coordinator
            .with_backend_probes(probes())
            .with_event_sink(sink),
    }
}

impl Wired {
    /// The next start of the agent over the same state and staged bundles.
    fn started_again(&self) -> Wired {
        wired_at(self.root.clone(), Step::BeforePrepare, |_| {})
    }
    fn deploy(&self, id: &str, bundle: &str) -> AgentResult<()> {
        let bundle = std::path::PathBuf::from(bundle_fixture(bundle));
        let labels = BTreeMap::new();
        let deployed = self.coordinator.deploy(id, &bundle, labels, None, None);
        deployed.map(|_| ())
    }
    /// The serving generation exits and its restart comes due.
    fn stop(&self, generation: u64) {
        let serving = self.f.launcher.handle(generation);
        serving.exited.store(true, Ordering::SeqCst);
        self.f.registry.supervise();
        self.f.clock.advance(Duration::from_secs(1));
    }
    /// What `active` and `previous_active` name, as `stt@2`, and the
    /// generation counter.
    fn named(&self) -> [String; 3] {
        let state = self.store.snapshot().unwrap();
        let named = |record: &Option<DeploymentRecord>| match record {
            Some(record) => {
                let generation = record.generation.expect("it names a descriptor");
                format!("{}@{generation}", record.deployment_id)
            }
            None => "-".to_owned(),
        };
        let counter = state.next_generation.expect("a generation was taken");
        [
            named(&state.active),
            named(&state.previous_active),
            counter.to_string(),
        ]
    }
    /// Every generation launched, in the order the launches returned.
    fn launched(&self) -> Vec<u64> {
        let launched = self.f.launcher.launched.lock().unwrap();
        launched.iter().map(|h| h.spec.member.generation).collect()
    }
    /// Whether no supervision step of a transaction did anything.
    fn stepped_idle(&self) -> bool {
        self.worker.steps.lock().unwrap().iter().all(Vec::is_empty)
    }
    /// What the last supervision step of a transaction did.
    fn last_step(&self) -> Vec<MemberEvent> {
        self.worker.steps.lock().unwrap().last().cloned().unwrap()
    }
}

fn restarted(generation: u64) -> [MemberEvent; 1] {
    restarted_as("stt", generation)
}

fn restarted_as(id: &str, generation: u64) -> [MemberEvent; 1] {
    let member = MemberRef::new(id, generation);
    [MemberEvent::Restarted { member }]
}

#[test]
fn a_restart_does_not_start_while_the_registry_is_held() {
    let w = wired();
    w.deploy("stt", STT).unwrap();
    w.stop(1);

    w.f.registry.hold();
    w.f.registry.hold();
    assert_eq!(w.f.registry.supervise(), []);
    w.f.registry.release();
    assert_eq!(w.f.registry.supervise(), []);
    assert_eq!(w.launched(), [1]);

    w.f.registry.release();
    assert_eq!(w.f.registry.supervise(), restarted(2));
    assert_eq!(w.named(), ["stt@2", "-", "3"]);
}

#[test]
fn a_restart_due_after_a_deploy_took_its_generation_waits_and_the_deploy_is_promoted() {
    let w = wired();
    w.deploy("stt", STT).unwrap();
    w.stop(1);

    w.deploy("tts", TTS).unwrap();

    // The step inside `prepare` took no generation after the deploy's.
    assert!(w.stepped_idle());
    assert_eq!(w.launched(), [1, 2]);
    assert_eq!(w.named(), ["tts@2", "stt@1", "3"]);
}

#[test]
fn a_restart_due_after_a_rollback_took_its_generation_waits_and_the_rollback_is_promoted() {
    let w = wired();
    w.deploy("stt", STT).unwrap();
    w.deploy("tts", TTS).unwrap();
    w.stop(2);

    w.coordinator.rollback(None).unwrap();

    assert!(w.stepped_idle());
    assert_eq!(w.launched(), [1, 2, 3]);
    assert_eq!(w.named(), ["stt@3", "tts@2", "4"]);
}

#[test]
fn a_restart_that_begins_as_a_deploy_asks_for_the_hold_takes_the_older_generation() {
    let w = wired_with(Step::BeforeTheHold, |_| {});
    w.deploy("stt", STT).unwrap();
    w.stop(1);

    w.deploy("tts", TTS).unwrap();

    // The restart began before the hold, so before the deploy's generation.
    assert_eq!(w.last_step(), restarted(2));
    assert_eq!(w.launched(), [1, 2, 3]);
    assert_eq!(w.named(), ["tts@3", "stt@2", "4"]);
}

#[test]
fn a_restart_that_begins_as_a_rollback_asks_for_the_hold_takes_the_older_generation() {
    let w = wired_with(Step::BeforeTheHold, |_| {});
    w.deploy("stt", STT).unwrap();
    w.deploy("tts", TTS).unwrap();
    w.stop(2);

    w.coordinator.rollback(None).unwrap();

    assert_eq!(w.last_step(), restarted_as("tts", 3));
    assert_eq!(w.launched(), [1, 2, 3, 4]);
    assert_eq!(w.named(), ["stt@4", "tts@3", "5"]);
}

#[test]
fn a_restart_already_starting_when_a_deploy_takes_its_generation_is_older_and_yields() {
    let w = wired();
    w.deploy("stt", STT).unwrap();
    w.stop(1);
    let (started, has_started) = mpsc::channel();
    let (release, released) = mpsc::channel();
    *w.f.launcher.hold_first.lock().unwrap() = Some((started, released));

    std::thread::scope(|scope| {
        // Dropped if this closure panics, which lets what is held end.
        let release = release;
        let restarting = scope.spawn(|| w.f.registry.supervise());
        has_started
            .recv_timeout(GRACE)
            .expect("the restart's launch began");
        // The restart took generation 2 and stamped it before the deploy.
        assert_eq!(w.named(), ["stt@2", "-", "3"]);

        w.deploy("tts", TTS).unwrap();
        assert_eq!(w.named(), ["tts@3", "stt@2", "4"]);

        release.send(()).unwrap();
        assert_eq!(restarting.join().unwrap(), []);
    });

    // The successor yielded: it was stopped, and nothing is owed for it.
    assert_eq!(w.launched(), [1, 3, 2]);
    assert_eq!(w.f.launcher.handle(2).calls(), ["terminate"]);
    w.f.clock.advance(Duration::from_secs(60));
    assert_eq!(w.f.registry.supervise(), []);
    assert_eq!(w.named(), ["tts@3", "stt@2", "4"]);
}

#[test]
fn a_deploy_after_one_whose_candidate_never_became_ready_takes_a_new_generation() {
    let w = wired();
    w.deploy("stt", STT).unwrap();
    w.f.probe.models.lock().unwrap().remove(&CANDIDATE_PORT);
    *w.worker.next_warm.lock().unwrap() = Some(WAIT_OUT);

    let error = w.deploy("tts", TTS).unwrap_err();

    assert!(matches!(error, AgentError::WorkerNotReady), "{error}");
    // Refused by its warm, not by the fence after a warm a stalled thread
    // gave up and a repeat then found ready.
    let state = w.store.snapshot().unwrap();
    let entry = state.quarantined.last().expect("a quarantine entry");
    assert_eq!(entry.phase, DeployState::Prepared);
    assert_eq!(w.named(), ["stt@1", "-", "3"]);
    // Its candidate, generation 2, runs until the next deploy displaces it.
    w.f.health(CANDIDATE_PORT, "tts");
    w.deploy("tts", TTS).unwrap();
    assert_eq!(w.launched(), [1, 2, 3]);
    assert_eq!(w.named(), ["tts@3", "stt@1", "4"]);
}

#[test]
fn a_start_after_a_same_id_update_whose_worker_could_not_start_restores_the_record() {
    let w = wired();
    w.deploy("stt", STT).unwrap();
    // A same-id update stages another bundle; its worker cannot be started.
    w.f.launcher.refuse.store(true, Ordering::SeqCst);
    w.deploy("stt", TTS).unwrap_err();
    assert_eq!(w.named(), ["stt@1", "-", "3"]);

    let next = w.started_again();
    apply_startup(&next.coordinator).unwrap();

    let status = next.coordinator.status().unwrap();
    assert_eq!(status.active.unwrap().generation, Some(3));
    assert_eq!(
        (status.agent_state, status.last_error),
        (AgentRunState::Ready, None)
    );
    assert_eq!(next.launched(), [3]);
    assert_eq!(next.named(), ["stt@3", "-", "4"]);
}

#[test]
fn status_after_a_start_that_did_not_restore_shows_the_error_and_no_serving_generation_until_the_id_is_deployed_again(
) {
    let w = wired();
    w.deploy("stt", STT).unwrap();
    let serving = w.coordinator.status().unwrap().active.unwrap();
    assert_eq!(serving.generation, Some(1));
    assert!(serving.serving_url.is_some());
    // The tree the record names is gone.
    std::fs::remove_dir_all(serving.staged_path.unwrap()).unwrap();

    let next = w.started_again();
    apply_startup(&next.coordinator).unwrap();

    let status = next.coordinator.status().unwrap();
    let recorded = status.active.unwrap();
    assert_eq!(recorded.deployment_id, "stt");
    assert_eq!((recorded.generation, recorded.serving_url), (None, None));
    assert_eq!(status.agent_state, AgentRunState::Degraded);
    let error = status.last_error.unwrap();
    let says = "`stt` is not serving until it is deployed again";
    assert_eq!(error.code, ErrorCode::ConfigInvalid);
    assert!(error.message.ends_with(says), "{error:?}");
    assert!(next.launched().is_empty() && next.f.registry.serving_member().unwrap().is_none());
    assert_eq!(next.named(), ["stt@1", "-", "2"]);

    next.deploy("stt", TTS).unwrap();

    let status = next.coordinator.status().unwrap();
    assert_eq!(status.active.unwrap().generation, Some(2));
    assert_eq!(
        (status.agent_state, status.last_error),
        (AgentRunState::Ready, None)
    );
    assert_eq!(next.named(), ["stt@2", "stt@1", "3"]);
}

#[test]
fn a_format_0_1_transaction_killed_after_the_registry_took_its_generation_is_ended_by_the_next_start(
) {
    // A format 0.1 bundle names no descriptor, so the registry takes the
    // generation inside `prepare`, in a state write of its own: the seventh
    // of a deploy and the fifth of a rollback, each after `capacity_checked`.
    for (rollback, landed) in [(false, 7), (true, 5)] {
        let w = wired();
        w.f.health(ACTIVE_PORT, "first");
        w.f.health(CANDIDATE_PORT, "second");
        w.deploy("first", FORMAT_0_1).unwrap();
        if rollback {
            w.deploy("second", FORMAT_0_1).unwrap();
        }
        let before = w.store.snapshot().unwrap();
        let launched = w.launched();
        w.store.kill_after_writes(landed);

        let ended = if rollback {
            w.coordinator.rollback(None).map(|_| ())
        } else {
            w.deploy("second", FORMAT_0_1)
        };

        assert!(matches!(ended, Err(AgentError::Io(_))), "{rollback}");
        // The generation is taken and its worker launched; the phase is the
        // one before.
        let found = w.store.snapshot().unwrap();
        let taken = before.next_generation.unwrap();
        assert_eq!(found.next_generation, Some(taken + 1), "{rollback}");
        assert_eq!(w.launched(), [launched, vec![taken]].concat(), "{rollback}");
        let tx = found.in_flight_transaction.expect("in flight");
        assert_eq!(tx.phase, DeployState::CapacityChecked, "{rollback}");
        assert_eq!(found.active, before.active, "{rollback}");

        // The next start's registry holds no worker. It ends the transaction
        // and starts the active record as the generation after.
        let next = w.started_again();
        let serving = found.active.unwrap().deployment_id;
        next.f.health(ACTIVE_PORT, &serving);
        apply_startup(&next.coordinator).unwrap();

        let settled = next.store.snapshot().unwrap();
        assert!(settled.in_flight_transaction.is_none() && settled.candidate.is_none());
        let ended: Vec<_> = settled.quarantined.iter().collect();
        assert_eq!(ended.len(), 1, "{rollback}");
        let reached = (ended[0].transaction_id.as_str(), ended[0].phase);
        let expected = (tx.transaction_id.as_str(), DeployState::CapacityChecked);
        assert_eq!(reached, expected, "{rollback}");
        assert_eq!(
            (&settled.active, &settled.previous_active),
            (&before.active, &before.previous_active),
            "{rollback}"
        );
        assert_eq!(next.launched(), [taken + 1], "{rollback}");
        let member = next.f.registry.serving_member().unwrap().unwrap().member;
        assert_eq!(member, MemberRef::new(&serving, taken + 1), "{rollback}");
        assert_eq!(settled.next_generation, Some(taken + 2), "{rollback}");
        assert_eq!(settled.last_error, None, "{rollback}");
    }
}

/// The commit write alone fails: writes are refused from the candidate's
/// fence until the transaction reports its failure, which it does before the
/// write that records it.
fn refuse_the_commit_write(w: &Wired) {
    let store = w.store.clone();
    let refuse = move || store.kill_after_writes(0);
    *w.worker.before_fence.lock().unwrap() = Some(Box::new(refuse));
    let store = w.store.clone();
    let reopen = move || store.kill_after_writes(usize::MAX);
    *w.worker.on_failure.lock().unwrap() = Some(Box::new(reopen));
}

fn roles(f: &Fixture) -> Vec<(MemberRef, MemberRole)> {
    let live = f.registry.live_members().unwrap();
    live.into_iter().map(|m| (m.member, m.role)).collect()
}

#[test]
fn a_deploy_whose_commit_write_is_refused_fails_and_the_serving_member_stays() {
    let w = wired();
    w.deploy("stt", STT).unwrap();
    let before = w.store.snapshot().unwrap();
    refuse_the_commit_write(&w);

    let refused = w.deploy("tts", TTS).unwrap_err();

    assert!(matches!(refused, AgentError::Io(_)), "{refused:?}");
    let serving = (MemberRef::new("stt", 1), MemberRole::Serving);
    let given_up = (MemberRef::new("tts", 2), MemberRole::Abandoned);
    assert_eq!(roles(&w.f), [serving, given_up]);
    assert!(w.f.launcher.handle(1).calls().is_empty());
    let found = w.store.snapshot().unwrap();
    assert_eq!(found.active, before.active);
    assert!(found.in_flight_transaction.is_none());
    let recorded = found.last_error.expect("the failure is recorded");
    assert!(recorded.message.contains("injected"), "{recorded:?}");
    let entry = found.quarantined.last().expect("a quarantine entry");
    assert_eq!(entry.phase, DeployState::Warmed);
}

#[test]
fn a_deploy_whose_candidate_lost_its_control_channel_after_its_warm_fails_before_the_commit() {
    let w = wired();
    w.deploy("stt", STT).unwrap();
    let before = w.store.snapshot().unwrap();
    let launcher = w.f.launcher.clone();
    let lose = move || launcher.handle(2).set_contact(ControlContact::Failed);
    *w.worker.before_fence.lock().unwrap() = Some(Box::new(lose));

    let refused = w.deploy("tts", TTS).unwrap_err();

    assert!(
        matches!(refused, AgentError::WorkerControl(_)),
        "{refused:?}"
    );
    let serving = (MemberRef::new("stt", 1), MemberRole::Serving);
    assert_eq!(roles(&w.f), [serving]);
    assert!(w.f.launcher.handle(1).calls().is_empty());
    assert_eq!(w.f.launcher.handle(2).calls(), ["kill"]);
    let found = w.store.snapshot().unwrap();
    assert_eq!(found.active, before.active);
    assert!(found.in_flight_transaction.is_none());
    let entry = found.quarantined.last().expect("a quarantine entry");
    assert_eq!(entry.phase, DeployState::Warmed);
}

#[test]
fn a_deploy_whose_candidate_cannot_be_inspected_kills_it_and_the_serving_member_stays() {
    for at_the_fence in [false, true] {
        let w = wired();
        w.deploy("stt", STT).unwrap();
        let before = w.store.snapshot().unwrap();
        let launcher = w.f.launcher.clone();
        let probe = w.f.probe.clone();
        let blind = move || {
            launcher
                .handle(2)
                .uninspectable
                .store(true, Ordering::SeqCst);
            probe.silent.store(!at_the_fence, Ordering::SeqCst);
        };
        if at_the_fence {
            *w.worker.before_fence.lock().unwrap() = Some(Box::new(blind));
        } else {
            *w.f.probe.during.lock().unwrap() = Some(Box::new(blind));
        }

        let refused = w.deploy("tts", TTS).unwrap_err();

        let says = matches!(&refused, AgentError::WorkerControl(s) if s == "wait failed");
        assert!(says, "{at_the_fence}: {refused:?}");
        let serving = (MemberRef::new("stt", 1), MemberRole::Serving);
        assert_eq!(roles(&w.f), [serving], "{at_the_fence}");
        assert!(w.f.launcher.handle(1).calls().is_empty(), "{at_the_fence}");
        assert_eq!(w.f.launcher.handle(2).calls(), ["kill"], "{at_the_fence}");
        let found = w.store.snapshot().unwrap();
        assert_eq!(found.active, before.active, "{at_the_fence}");
        assert!(found.in_flight_transaction.is_none(), "{at_the_fence}");
    }
}

#[test]
fn a_rollback_whose_commit_write_is_refused_fails_and_the_serving_member_stays() {
    let w = wired();
    w.deploy("stt", STT).unwrap();
    w.deploy("tts", TTS).unwrap();
    let before = w.store.snapshot().unwrap();
    refuse_the_commit_write(&w);

    let refused = w.coordinator.rollback(None).unwrap_err();

    assert!(matches!(refused, AgentError::Io(_)), "{refused:?}");
    let serving = (MemberRef::new("tts", 2), MemberRole::Serving);
    let given_up = (MemberRef::new("stt", 3), MemberRole::Abandoned);
    assert_eq!(roles(&w.f), [serving, given_up]);
    assert!(w.f.launcher.handle(2).calls().is_empty());
    let found = w.store.snapshot().unwrap();
    assert_eq!(found.active, before.active);
    assert!(found.in_flight_transaction.is_none());
    let recorded = found.last_error.expect("the failure is recorded");
    assert!(recorded.message.contains("injected"), "{recorded:?}");
    assert_eq!(found.quarantined.len(), before.quarantined.len());
}

#[test]
fn the_serving_member_is_asked_to_stop_only_once_the_commit_is_durable() {
    for rollback in [false, true] {
        let w = wired();
        w.deploy("stt", STT).unwrap();
        if rollback {
            w.deploy("tts", TTS).unwrap();
        }
        let outgoing = w.f.launcher.handle(if rollback { 2 } else { 1 });
        let (asked, release) = hold_stop(&outgoing);

        let committed = std::thread::scope(|scope| {
            // Dropped if this closure panics, which lets the stop end.
            let release = release;
            let transaction = scope.spawn(|| {
                if rollback {
                    w.coordinator.rollback(None).map(|_| ())
                } else {
                    w.deploy("tts", TTS)
                }
            });
            asked.recv_timeout(GRACE).unwrap();
            let active = w.store.snapshot().unwrap().active.unwrap();
            release.send(()).unwrap();
            transaction.join().unwrap().unwrap();
            active.deployment_id
        });

        let incoming = if rollback { "stt" } else { "tts" };
        assert_eq!(committed, incoming, "{rollback}");
    }
}

/// The next start of `w`'s agent, its worker failing as `fail` makes it.
fn started_failing(w: &Wired, fail: &str) -> Wired {
    // Only a worker that never answers waits the warm out; the other modes
    // keep the default so a loaded machine cannot turn them into that one.
    let next = wired_at(w.root.clone(), Step::BeforePrepare, |config| {
        if fail == "ready" {
            config.worker.warm_timeout_ms = 50;
        }
    });
    let launcher = next.f.launcher.clone();
    let last = move || launcher.launched.lock().unwrap().last().cloned().unwrap();
    let exit_last = {
        let last = last.clone();
        move || last().exited.store(true, Ordering::SeqCst)
    };
    let probe = next.f.probe.clone();
    match fail {
        "spawn" => next.f.launcher.refuse.store(true, Ordering::SeqCst),
        "rule" => next.worker.refuse_prepare.store(true, Ordering::SeqCst),
        "ready" => next.f.probe.silent.store(true, Ordering::SeqCst),
        // Gone while loading: the probe that finds it gone gets no answer.
        "loading" => {
            let exit_unanswered = move || {
                exit_last();
                probe.silent.store(true, Ordering::SeqCst);
            };
            *next.f.probe.during.lock().unwrap() = Some(Box::new(exit_unanswered));
        }
        // Ready, then gone before it would serve.
        "fence" => *next.worker.before_fence.lock().unwrap() = Some(Box::new(exit_last)),
        // Its process can no longer be inspected while it loads.
        "uninspectable" => {
            let blind = move || {
                last().uninspectable.store(true, Ordering::SeqCst);
                probe.silent.store(true, Ordering::SeqCst);
            };
            *next.f.probe.during.lock().unwrap() = Some(Box::new(blind));
        }
        // Ready, then its process cannot be inspected at the fence.
        "uninspectable at the fence" => {
            let blind = move || last().uninspectable.store(true, Ordering::SeqCst);
            *next.worker.before_fence.lock().unwrap() = Some(Box::new(blind));
        }
        _ => unreachable!(),
    }
    next
}

fn heal(next: &Wired) {
    next.f.launcher.refuse.store(false, Ordering::SeqCst);
    next.worker.refuse_prepare.store(false, Ordering::SeqCst);
    next.f.probe.silent.store(false, Ordering::SeqCst);
    next.f.probe.during.lock().unwrap().take();
}

fn serving(id: &str, generation: u64) -> [MemberEvent; 1] {
    let member = MemberRef::new(id, generation);
    [MemberEvent::Serving { member }]
}

#[test]
fn a_start_whose_restored_worker_does_not_come_up_stays_up_and_restarts_it() {
    for fail in [
        "spawn",
        "ready",
        "loading",
        "fence",
        "uninspectable",
        "uninspectable at the fence",
    ] {
        let w = wired();
        w.deploy("stt", STT).unwrap();
        let next = started_failing(&w, fail);

        apply_startup(&next.coordinator).unwrap();

        let status = next.coordinator.status().unwrap();
        assert_eq!(status.agent_state, AgentRunState::Degraded, "{fail}");
        let error = status.last_error.expect(fail);
        if matches!(fail, "loading" | "fence") {
            // Ended by the worker's exit, not by the warm's deadline.
            assert_eq!(error.code, ErrorCode::LoadFailed, "{fail}");
        }
        // The restore's own generation is not left running.
        assert!(roles(&next.f).is_empty(), "{fail}");
        heal(&next);
        next.f.clock.advance(Duration::from_secs(1));
        assert_eq!(next.coordinator.supervise(), restarted(3), "{fail}");
        assert_eq!(next.coordinator.supervise(), serving("stt", 3), "{fail}");
        let status = next.coordinator.status().unwrap();
        let reads = (status.agent_state, status.last_error);
        assert_eq!(reads, (AgentRunState::Ready, None), "{fail}");
        assert_eq!(next.named(), ["stt@3", "-", "4"], "{fail}");
    }
}

#[test]
fn a_start_whose_restored_worker_never_comes_up_shows_the_members_failure() {
    let w = wired();
    w.deploy("stt", STT).unwrap();
    let next = started_failing(&w, "spawn");
    apply_startup(&next.coordinator).unwrap();

    let mut failed = None;
    for _ in RESTART_DELAYS {
        next.f.clock.advance(Duration::from_secs(4));
        for event in next.coordinator.supervise() {
            if let MemberEvent::Failed { error, .. } = event {
                failed = Some(error);
            }
        }
    }

    let failed = failed.expect("the restarts are used up");
    let status = next.coordinator.status().unwrap();
    assert_eq!(status.agent_state, AgentRunState::Degraded);
    assert_eq!(status.last_error.unwrap().message, failed.message);
    assert!(failed.message.contains("restarts inside"), "{failed:?}");
}

#[test]
fn the_loop_never_clears_an_error_that_is_not_the_restores() {
    let w = wired();
    w.deploy("stt", STT).unwrap();
    w.f.launcher.refuse.store(true, Ordering::SeqCst);
    w.deploy("tts", TTS).unwrap_err();
    w.f.launcher.refuse.store(false, Ordering::SeqCst);
    let recorded = w.store.snapshot().unwrap().last_error;
    assert!(recorded.is_some());

    w.stop(1);
    assert_eq!(w.coordinator.supervise(), restarted(3));
    assert_eq!(w.coordinator.supervise(), serving("stt", 3));

    assert_eq!(w.store.snapshot().unwrap().last_error, recorded);
}

#[test]
fn a_cleared_restore_error_recorded_again_is_not_cleared_by_a_later_restart() {
    let w = wired();
    w.deploy("stt", STT).unwrap();
    let next = restarting_after_a_failed_restore(&w);
    let restored = next.store.snapshot().unwrap().last_error.unwrap();
    assert_eq!(next.coordinator.supervise(), serving("stt", 3));
    assert_eq!(next.store.snapshot().unwrap().last_error, None);
    let again = |s: &mut AgentState| {
        s.last_error = Some(restored.clone());
        Ok(())
    };
    next.store.update(again).unwrap();

    next.stop(3);
    // The restore's restart was the first inside the window.
    next.f.clock.advance(RESTART_DELAYS[1]);
    assert_eq!(next.coordinator.supervise(), restarted(4));
    assert_eq!(next.coordinator.supervise(), serving("stt", 4));

    assert_eq!(next.store.snapshot().unwrap().last_error, Some(restored));
}

/// The next start of `w`'s agent: its restore failed, and the restart
/// of the restored deployment has been launched.
fn restarting_after_a_failed_restore(w: &Wired) -> Wired {
    let next = started_failing(w, "spawn");
    apply_startup(&next.coordinator).unwrap();
    heal(&next);
    next.f.clock.advance(Duration::from_secs(1));
    assert_eq!(next.coordinator.supervise(), restarted(3));
    next
}

#[test]
fn the_loop_keeps_an_error_written_after_it_read_the_restores() {
    let w = wired();
    w.deploy("stt", STT).unwrap();
    let next = restarting_after_a_failed_restore(&w);
    let other = ErrorRecord::new(ErrorCode::Internal, "a deploy's failure");
    let (store, written) = (next.store.clone(), other.clone());
    // Between the loop's read of the restore's error and its clearing write.
    let write = move || store.set_last_error(Some(written)).unwrap();
    *next.worker.before_serving_member.lock().unwrap() = Some(Box::new(write));

    assert_eq!(next.coordinator.supervise(), serving("stt", 3));

    assert_eq!(next.store.snapshot().unwrap().last_error, Some(other));
}

#[test]
fn a_restores_error_is_kept_while_the_member_serving_is_not_the_active_records() {
    // An update of the id, another id with the same bundle, and a redeploy
    // of both.
    for (id, bundle) in [("stt", TTS), ("tts", STT), ("stt", STT)] {
        let w = wired();
        w.deploy("stt", STT).unwrap();
        let next = restarting_after_a_failed_restore(&w);
        next.f.health(CANDIDATE_PORT, id);
        // The clearing write fails, so the clearing is still owed at the next turn.
        next.store.kill_after_writes(0);
        assert_eq!(next.coordinator.supervise(), serving("stt", 3));
        next.store.kill_after_writes(usize::MAX);
        let recorded = next.store.snapshot().unwrap().last_error;
        assert!(recorded.is_some());
        let (at, is_at) = std::sync::mpsc::channel();
        let (go, gone) = std::sync::mpsc::channel::<()>();
        let wait = move || {
            at.send(()).unwrap();
            let _ = gone.recv();
        };
        *next.worker.before_activate.lock().unwrap() = Some(Box::new(wait));

        let during = std::thread::scope(|scope| {
            // Dropped if this closure panics, which lets the deploy end.
            let go = go;
            let deploying = scope.spawn(|| next.deploy(id, bundle));
            is_at.recv_timeout(GRACE).unwrap();
            // A turn between the deploy's commit and its activation.
            next.coordinator.supervise();
            let during = next.store.snapshot().unwrap().last_error;
            go.send(()).unwrap();
            deploying.join().unwrap().unwrap();
            during
        });

        assert_eq!(during, recorded, "{id}");
        assert_eq!(next.store.snapshot().unwrap().last_error, None, "{id}");
    }
}

#[test]
fn a_restores_error_keeps_its_typed_reason_and_is_cleared_once_its_restart_serves() {
    let w = wired();
    w.deploy("stt", STT).unwrap();
    let next = started_failing(&w, "rule");
    apply_startup(&next.coordinator).unwrap();
    let recorded = next.store.snapshot().unwrap().last_error;
    assert_eq!(recorded, Some(refused_by_rule().to_record()));
    let rule = BundleRuleCode::RequiredField.as_str();
    assert_eq!(recorded.unwrap().context.as_deref(), Some(rule));

    heal(&next);
    next.f.clock.advance(Duration::from_secs(1));
    assert_eq!(next.coordinator.supervise(), restarted(3));
    assert_eq!(next.coordinator.supervise(), serving("stt", 3));

    assert_eq!(next.store.snapshot().unwrap().last_error, None);
}

#[test]
fn a_deploys_failure_equal_to_the_restores_is_not_cleared_once_the_restart_serves() {
    let w = wired();
    w.deploy("stt", STT).unwrap();
    let next = started_failing(&w, "rule");
    apply_startup(&next.coordinator).unwrap();
    next.f.clock.advance(Duration::from_secs(1));
    assert_eq!(next.coordinator.supervise(), restarted(3));

    next.deploy("tts", TTS).unwrap_err();
    let failed = next.store.snapshot().unwrap();
    assert_eq!(failed.last_error, Some(refused_by_rule().to_record()));
    assert_eq!(failed.quarantined.last().unwrap().deployment_id, "tts");
    assert_eq!(next.coordinator.supervise(), serving("stt", 3));

    let kept = next.store.snapshot().unwrap().last_error;
    assert_eq!(kept, Some(refused_by_rule().to_record()));
}

#[test]
fn a_rollbacks_failure_equal_to_the_restores_is_not_cleared_once_the_restart_serves() {
    let w = wired();
    w.deploy("stt", STT).unwrap();
    w.deploy("tts", TTS).unwrap();
    let next = started_failing(&w, "rule");
    // The restore's restart of `tts` takes the first port.
    next.f.health(ACTIVE_PORT, "tts");
    apply_startup(&next.coordinator).unwrap();
    let recorded = next.store.snapshot().unwrap().last_error;
    assert_eq!(recorded, Some(refused_by_rule().to_record()));
    next.f.clock.advance(Duration::from_secs(1));
    assert_eq!(next.coordinator.supervise(), restarted_as("tts", 4));

    next.coordinator.rollback(None).unwrap_err();
    let failed = next.store.snapshot().unwrap();
    assert_eq!(failed.last_error, recorded);
    assert_eq!(failed.active.unwrap().deployment_id, "tts");
    assert_eq!(next.coordinator.supervise(), serving("tts", 4));

    assert_eq!(next.store.snapshot().unwrap().last_error, recorded);
}

#[test]
fn a_deploy_whose_transaction_never_began_leaves_the_restores_error_to_the_loop() {
    let w = wired();
    w.deploy("stt", STT).unwrap();
    let next = restarting_after_a_failed_restore(&w);
    next.store.kill_after_writes(0);

    next.deploy("tts", TTS).unwrap_err();
    next.store.kill_after_writes(usize::MAX);

    assert!(next
        .store
        .snapshot()
        .unwrap()
        .in_flight_transaction
        .is_none());
    assert_eq!(next.coordinator.supervise(), serving("stt", 3));
    assert_eq!(next.store.snapshot().unwrap().last_error, None);
}

#[test]
fn a_deploys_failure_written_between_the_loops_read_and_its_write_is_kept() {
    let w = wired();
    w.deploy("stt", STT).unwrap();
    let next = started_failing(&w, "rule");
    apply_startup(&next.coordinator).unwrap();
    next.f.clock.advance(Duration::from_secs(1));
    assert_eq!(next.coordinator.supervise(), restarted(3));
    let (at, is_at) = std::sync::mpsc::channel();
    let (go, gone) = std::sync::mpsc::channel::<()>();
    let wait = move || {
        at.send(()).unwrap();
        let _ = gone.recv();
    };
    *next.worker.before_serving_member.lock().unwrap() = Some(Box::new(wait));

    std::thread::scope(|scope| {
        // Dropped if this closure panics, which lets the turn end.
        let go = go;
        let turn = scope.spawn(|| next.coordinator.supervise());
        is_at.recv_timeout(GRACE).unwrap();
        // The loop has read the restore's error and not written yet.
        next.deploy("tts", TTS).unwrap_err();
        go.send(()).unwrap();
        assert_eq!(turn.join().unwrap(), serving("stt", 3));
    });

    let kept = next.store.snapshot().unwrap().last_error;
    assert_eq!(kept, Some(refused_by_rule().to_record()));
}

#[test]
fn a_restores_error_whose_clearing_is_owed_is_kept_while_nothing_serves() {
    let w = wired();
    w.deploy("stt", STT).unwrap();
    let next = restarting_after_a_failed_restore(&w);
    let recorded = next.store.snapshot().unwrap().last_error;
    next.store.kill_after_writes(0);
    assert_eq!(next.coordinator.supervise(), serving("stt", 3));
    next.store.kill_after_writes(usize::MAX);

    // The restart stops before the turn that owes the clearing.
    let restart = next.f.launcher.handle(3);
    restart.exited.store(true, Ordering::SeqCst);
    next.coordinator.supervise();

    assert!(next.f.registry.serving_member().unwrap().is_none());
    assert_eq!(next.store.snapshot().unwrap().last_error, recorded);
}

#[test]
fn a_restores_error_whose_clearing_write_fails_is_kept_and_cleared_on_a_later_turn() {
    let w = wired();
    w.deploy("stt", STT).unwrap();
    let next = started_failing(&w, "spawn");
    apply_startup(&next.coordinator).unwrap();
    heal(&next);
    next.f.clock.advance(Duration::from_secs(1));
    assert_eq!(next.coordinator.supervise(), restarted(3));
    let recorded = next.store.snapshot().unwrap().last_error;
    assert!(recorded.is_some());

    next.store.kill_after_writes(0);
    assert_eq!(next.coordinator.supervise(), serving("stt", 3));
    assert_eq!(next.coordinator.supervise(), []);

    let member = next.f.registry.serving_member().unwrap().unwrap().member;
    assert_eq!(member, MemberRef::new("stt", 3));
    assert_eq!(next.store.snapshot().unwrap().last_error, recorded);
    next.store.kill_after_writes(usize::MAX);
    assert_eq!(next.coordinator.supervise(), []);
    let status = next.coordinator.status().unwrap();
    assert_eq!(
        (status.agent_state, status.last_error),
        (AgentRunState::Ready, None)
    );
}

#[test]
fn a_restores_error_whose_clearing_write_keeps_failing_is_logged_once() {
    let w = wired();
    w.deploy("stt", STT).unwrap();
    let next = started_failing(&w, "spawn");
    apply_startup(&next.coordinator).unwrap();
    heal(&next);
    next.f.clock.advance(Duration::from_secs(1));
    next.coordinator.supervise();
    next.store.kill_after_writes(0);

    for _ in 0..3 {
        next.coordinator.supervise();
    }
    let logged = || next.coordinator.restore_clear_logs.load(Ordering::SeqCst);
    assert_eq!(logged(), 1);
    next.store.kill_after_writes(usize::MAX);
    next.coordinator.supervise();
    assert_eq!(next.store.snapshot().unwrap().last_error, None);
}

#[test]
fn a_transaction_whose_activation_a_stop_refused_is_settled_forward_at_the_next_start() {
    for rollback in [false, true] {
        let w = wired();
        w.deploy("stt", STT).unwrap();
        if rollback {
            w.deploy("tts", TTS).unwrap();
        }
        let f = w.f.clone();
        let stop = move || f.registry.shutdown();
        *w.worker.before_activate.lock().unwrap() = Some(Box::new(stop));

        let refused = if rollback {
            w.coordinator.rollback(None).map(|_| ())
        } else {
            w.deploy("tts", TTS)
        };

        assert!(matches!(refused, Err(AgentError::Stopping)), "{rollback}");
        let left = w.store.snapshot().unwrap();
        let tx = left.in_flight_transaction.expect("in flight");
        assert_eq!(tx.phase, DeployState::Promoted, "{rollback}");
        let incoming = if rollback { "stt" } else { "tts" };
        let committed = left.active.unwrap();
        assert_eq!(committed.deployment_id, incoming, "{rollback}");

        let next = w.started_again();
        next.f.health(ACTIVE_PORT, incoming);
        apply_startup(&next.coordinator).unwrap();

        let settled = next.store.snapshot().unwrap();
        assert!(settled.in_flight_transaction.is_none(), "{rollback}");
        assert!(settled.quarantined.is_empty(), "{rollback}");
        assert_eq!(settled.active.unwrap().deployment_id, incoming);
        let member = next.f.registry.serving_member().unwrap().unwrap().member;
        assert_eq!(member.deployment_id, incoming, "{rollback}");
    }
}

#[test]
fn status_withholds_the_stream_endpoint_between_the_commit_and_the_activation() {
    let w = wired_with(Step::BeforePrepare, |config| {
        config.worker.mode = crate::config::WorkerControlMode::Process;
        config.worker.serving_port_range = Some(crate::config::PortRange {
            first: ACTIVE_PORT + 10,
            last: CANDIDATE_PORT + 10,
        });
    });
    let streaming = std::path::PathBuf::from(bundle_fixture(STT));
    let deployed = w
        .coordinator
        .deploy_with("stt", &streaming, BTreeMap::new(), None, None, true);
    deployed.unwrap();
    let endpoint = format!("127.0.0.1:{}", ACTIVE_PORT + 10);
    let before = w.coordinator.status().unwrap().active.unwrap();
    assert_eq!(before.stream_endpoint, Some(endpoint));

    let (at, is_at) = std::sync::mpsc::channel();
    let (go, gone) = std::sync::mpsc::channel::<()>();
    let wait = move || {
        at.send(()).unwrap();
        let _ = gone.recv();
    };
    *w.worker.before_activate.lock().unwrap() = Some(Box::new(wait));
    let during = std::thread::scope(|scope| {
        // Dropped if this closure panics, which lets the deploy end.
        let go = go;
        let deploying = scope.spawn(|| w.deploy("tts", TTS));
        is_at.recv_timeout(GRACE).unwrap();
        let during = w.coordinator.status().unwrap().active.unwrap();
        go.send(()).unwrap();
        deploying.join().unwrap().unwrap();
        during
    });

    // The outgoing worker streams, and it is not the committed deployment's.
    assert_eq!(during.deployment_id, "tts");
    assert_eq!(during.generation, None);
    assert_eq!(
        (during.stream_endpoint, during.descriptor_digest),
        (None, None)
    );
    let after = w.coordinator.status().unwrap().active.unwrap();
    assert_eq!(after.generation, Some(2));
    assert_eq!(
        (after.stream_endpoint, after.descriptor_digest),
        (None, None)
    );
}

#[test]
fn status_withholds_the_generation_between_the_commit_and_the_activation() {
    // The same id with another bundle, and another id.
    for (id, bundle) in [("stt", TTS), ("tts", TTS)] {
        let w = wired();
        w.deploy("stt", STT).unwrap();
        w.f.health(CANDIDATE_PORT, id);
        let (at, is_at) = std::sync::mpsc::channel();
        let (go, gone) = std::sync::mpsc::channel::<()>();
        let wait = move || {
            at.send(()).unwrap();
            let _ = gone.recv();
        };
        *w.worker.before_activate.lock().unwrap() = Some(Box::new(wait));

        let during = std::thread::scope(|scope| {
            // Dropped if this closure panics, which lets the deploy end.
            let go = go;
            let deploying = scope.spawn(|| w.deploy(id, bundle));
            is_at.recv_timeout(GRACE).unwrap();
            let during = w.coordinator.status().unwrap().active.unwrap();
            go.send(()).unwrap();
            deploying.join().unwrap().unwrap();
            during
        });

        let committed = w.store.snapshot().unwrap().active.unwrap();
        assert_eq!(during.deployment_id, id);
        assert_eq!(during.bundle_digest, committed.bundle_digest, "{id}");
        assert_eq!(during.generation, None, "{id}");
        let outgoing = format!("http://127.0.0.1:{ACTIVE_PORT}/infer");
        assert_eq!(during.serving_url.as_deref(), Some(outgoing.as_str()));
        let after = w.coordinator.status().unwrap().active.unwrap();
        assert_eq!(after.generation, Some(2), "{id}");
    }
}
