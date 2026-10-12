// SPDX-License-Identifier: Apache-2.0

//! Transactions that hand the registry a generation, driven by the
//! coordinator over the durable state, against a restart of the serving
//! member.

use super::*;
use tensorplate_protocol::agent_state::DeploymentRecord;

use crate::coordinator::Coordinator;
use crate::descriptor_store::restart_stamp;
use crate::descriptor_store::tests::{agent_config, probes};

const STT: &str = "v0_2/speech_stt_streaming";
const TTS: &str = "v0_2/speech_tts_streaming";

/// The registry, behind a `prepare` that first runs a supervision step: the
/// agent's loop may run one between a transaction's state update and its
/// `prepare`.
struct Stepped {
    f: Arc<Fixture>,
    /// What each of those steps did.
    steps: Mutex<Vec<Vec<MemberEvent>>>,
}

impl WorkerControl for Stepped {
    fn prepare(&self, tx: &str, candidate: &CandidateRef, timeout: Duration) -> AgentResult<()> {
        let stepped = self.f.registry.supervise();
        self.steps.lock().unwrap().push(stepped);
        self.f.registry.prepare(tx, candidate, timeout)
    }
    fn warm(
        &self,
        tx: &str,
        candidate: &CandidateRef,
        timeout: Duration,
    ) -> AgentResult<WorkerReadiness> {
        self.f.registry.warm(tx, candidate, timeout)
    }
    fn promote(&self, tx: &str, candidate: &CandidateRef) -> AgentResult<()> {
        self.f.registry.promote(tx, candidate)
    }
    fn abandon(&self, candidate: &CandidateRef) {
        self.f.registry.abandon(candidate);
    }
    fn hold(&self) {
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
    fn stopping(&self) -> bool {
        self.f.registry.stopping()
    }
}

/// A coordinator over a registry wired to the durable state as the agent
/// wires it, both speech runner profiles installed.
struct Wired {
    f: Arc<Fixture>,
    /// Holds the state and the staged bundles.
    _root: TempDir,
    store: Arc<StateStore>,
    worker: Arc<Stepped>,
    coordinator: Coordinator,
}

fn wired() -> Wired {
    wired_with(|_| {})
}

fn wired_with(configure: impl FnOnce(&mut AgentConfig)) -> Wired {
    let root = TempDir::new().unwrap();
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
    let f = Arc::new(fixture_with(|settings| settings, wire));
    // The first deployment is given the first port, the next the second.
    f.health(ACTIVE_PORT, "stt");
    f.health(CANDIDATE_PORT, "tts");
    let steps = Mutex::default();
    let worker = Arc::new(Stepped {
        f: f.clone(),
        steps,
    });
    let coordinator = Coordinator::new(config, store.clone(), worker.clone());
    Wired {
        f,
        _root: root,
        store,
        worker,
        coordinator: coordinator.with_backend_probes(probes()),
    }
}

impl Wired {
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
    /// Whether no supervision step inside a `prepare` did anything.
    fn stepped_idle(&self) -> bool {
        self.worker.steps.lock().unwrap().iter().all(Vec::is_empty)
    }
}

fn restarted(generation: u64) -> [MemberEvent; 1] {
    let member = MemberRef::new("stt", generation);
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
    let w = wired_with(|config| config.worker.warm_timeout_ms = 40);
    w.deploy("stt", STT).unwrap();
    w.f.probe.models.lock().unwrap().remove(&CANDIDATE_PORT);

    let error = w.deploy("tts", TTS).unwrap_err();

    assert!(matches!(error, AgentError::WorkerNotReady), "{error}");
    assert_eq!(w.named(), ["stt@1", "-", "3"]);
    // Its candidate, generation 2, runs until the next deploy displaces it.
    w.f.health(CANDIDATE_PORT, "tts");
    w.deploy("tts", TTS).unwrap();
    assert_eq!(w.launched(), [1, 2, 3]);
    assert_eq!(w.named(), ["tts@3", "stt@1", "4"]);
}
