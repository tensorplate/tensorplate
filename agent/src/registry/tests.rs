// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;
use tempfile::TempDir;

const ACTIVE_PORT: u16 = 41_080;
const CANDIDATE_PORT: u16 = 41_081;
const DRAIN: Duration = Duration::from_millis(60);
const WARM: Duration = Duration::from_millis(40);

/// What a test sees and steers of one launched process.
struct Handle {
    spec: LaunchSpec,
    config: serde_json::Value,
    calls: Mutex<Vec<&'static str>>,
    exited: AtomicBool,
    obeys_terminate: AtomicBool,
    contact: Mutex<ControlContact>,
}

impl Handle {
    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }
    fn set_contact(&self, contact: ControlContact) {
        *self.contact.lock().unwrap() = contact;
    }
}

struct FakeProcess(Arc<Handle>);

impl MemberProcess for FakeProcess {
    fn exited(&mut self) -> AgentResult<bool> {
        Ok(self.0.exited.load(Ordering::SeqCst))
    }
    fn startup_failure(&mut self) -> AgentError {
        AgentError::WorkerExited(format!("generation {}", self.0.spec.member.generation))
    }
    fn control(&self) -> ControlContact {
        *self.0.contact.lock().unwrap()
    }
    fn terminate(&mut self) {
        self.0.calls.lock().unwrap().push("terminate");
        if self.0.obeys_terminate.load(Ordering::SeqCst) {
            self.0.exited.store(true, Ordering::SeqCst);
        }
    }
    fn kill(&mut self) {
        self.0.calls.lock().unwrap().push("kill");
        self.0.exited.store(true, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct FakeLauncher {
    launched: Mutex<Vec<Arc<Handle>>>,
    refuse: AtomicBool,
}

impl FakeLauncher {
    fn handle(&self, generation: u64) -> Arc<Handle> {
        self.launched
            .lock()
            .unwrap()
            .iter()
            .find(|h| h.spec.member.generation == generation)
            .cloned()
            .expect("launched generation")
    }
    fn count(&self) -> usize {
        self.launched.lock().unwrap().len()
    }
}

impl ProcessLauncher for FakeLauncher {
    fn launch(&self, spec: &LaunchSpec) -> AgentResult<Box<dyn MemberProcess>> {
        if self.refuse.load(Ordering::SeqCst) {
            return Err(AgentError::WorkerControl("spawn refused".into()));
        }
        let config = serde_json::from_slice(&std::fs::read(&spec.config_path).unwrap()).unwrap();
        let handle = Arc::new(Handle {
            spec: spec.clone(),
            config,
            calls: Mutex::new(Vec::new()),
            exited: AtomicBool::new(false),
            obeys_terminate: AtomicBool::new(true),
            contact: Mutex::new(ControlContact::Reporting),
        });
        self.launched.lock().unwrap().push(handle.clone());
        Ok(Box::new(FakeProcess(handle)))
    }
}

/// Reports every port ready for the model its member was rendered with.
#[derive(Default)]
struct FakeProbe {
    silent: AtomicBool,
    models: Mutex<BTreeMap<u16, String>>,
}

impl HealthProbe for FakeProbe {
    fn ready_model(&self, port: u16, _timeout: Duration) -> AgentResult<Option<String>> {
        if self.silent.load(Ordering::SeqCst) {
            return Ok(None);
        }
        Ok(self.models.lock().unwrap().get(&port).cloned())
    }
}

struct Fixture {
    td: TempDir,
    launcher: Arc<FakeLauncher>,
    probe: Arc<FakeProbe>,
    next_generation: Arc<AtomicU64>,
    registry: MemberRegistry,
}

fn settings(dir: &std::path::Path) -> RegistrySettings {
    RegistrySettings {
        bind_host: "127.0.0.1".into(),
        active_port: ACTIVE_PORT,
        candidate_port: CANDIDATE_PORT,
        config_dir: dir.join("worker-configs"),
        use_mock_session: true,
        status_poll_interval: Duration::from_millis(1),
        stop_drain: DRAIN,
    }
}

fn fixture() -> Fixture {
    let td = TempDir::new().unwrap();
    let launcher = Arc::new(FakeLauncher::default());
    let probe = Arc::new(FakeProbe::default());
    let next_generation = Arc::new(AtomicU64::new(1));
    let counter = next_generation.clone();
    let registry = MemberRegistry::new(
        settings(td.path()),
        launcher.clone(),
        probe.clone(),
        Arc::new(move || Ok(counter.fetch_add(1, Ordering::SeqCst))),
    );
    Fixture {
        td,
        launcher,
        probe,
        next_generation,
        registry,
    }
}

fn candidate(id: &str) -> CandidateRef {
    CandidateRef {
        deployment_id: id.into(),
        staged_path: format!("/staging/{id}"),
        bundle_digest: "sha256:cafe".into(),
        backend_hint: "mock".into(),
        model_class: "vision".into(),
        bundle_name: Some("m".into()),
        bundle_version: Some("1".into()),
        artifact_relative_path: Some("model.bin".into()),
    }
}

impl Fixture {
    fn prepare(&self, id: &str) -> AgentResult<()> {
        self.registry.prepare("tx", &candidate(id), WARM)
    }
    fn warm(&self, id: &str) -> AgentResult<WorkerReadiness> {
        self.registry.warm("tx", &candidate(id), WARM)
    }
    /// Make `/health` on `port` report `id` ready.
    fn health(&self, port: u16, id: &str) {
        self.probe.models.lock().unwrap().insert(port, id.into());
    }
    /// Launch `id` and take it through to serving.
    fn serve(&self, id: &str) -> Arc<Handle> {
        self.prepare(id).unwrap();
        let handle = self.launcher.launched.lock().unwrap().last().cloned().unwrap();
        let port = u16::try_from(handle.config["bind"]["port"].as_u64().unwrap()).unwrap();
        self.health(port, id);
        assert!(self.warm(id).unwrap().ready);
        self.registry.promote("tx", &candidate(id)).unwrap();
        handle
    }
    fn configs(&self) -> usize {
        std::fs::read_dir(self.td.path().join("worker-configs")).map_or(0, Iterator::count)
    }
}

#[test]
fn each_launch_takes_a_new_generation_and_renders_it_with_the_member_name() {
    let f = fixture();
    f.next_generation.store(7, Ordering::SeqCst);
    f.prepare("stt").unwrap();
    f.prepare("stt").unwrap();

    for generation in [7, 8] {
        let handle = f.launcher.handle(generation);
        assert_eq!(handle.spec.member, MemberRef::new("stt", generation));
        let deployment = &handle.config["deployment"];
        assert_eq!(deployment["generation"], generation);
        assert_eq!(deployment["endpoint"], "stt");
        assert_eq!(deployment["model"]["model_id"], "stt");
        assert_eq!(deployment["model"]["artifact_path"], "/staging/stt/model.bin");
        assert_eq!(deployment["use_mock_session"], true);
    }
    assert_eq!(f.launcher.count(), 2);
}

#[test]
fn a_member_name_the_worker_would_refuse_never_takes_a_generation() {
    let f = fixture();
    let err = f.prepare("not a member name").unwrap_err();
    assert!(matches!(err, AgentError::WorkerControl(_)), "{err}");
    assert_eq!(f.launcher.count(), 0);
    assert_eq!(f.next_generation.load(Ordering::SeqCst), 1);
}

#[test]
fn no_worker_starts_when_a_generation_cannot_be_allocated() {
    let td = TempDir::new().unwrap();
    let launcher = Arc::new(FakeLauncher::default());
    let registry = MemberRegistry::new(
        settings(td.path()),
        launcher.clone(),
        Arc::new(FakeProbe::default()),
        Arc::new(|| Err(AgentError::StateIndeterminate("write outcome unknown".into()))),
    );
    let err = registry.prepare("tx", &candidate("stt"), WARM).unwrap_err();
    assert!(matches!(err, AgentError::StateIndeterminate(_)), "{err}");
    assert_eq!(launcher.count(), 0);
}

#[test]
fn a_launch_that_fails_spends_its_generation_and_leaves_no_config() {
    let f = fixture();
    f.launcher.refuse.store(true, Ordering::SeqCst);
    assert!(f.prepare("stt").is_err());
    assert_eq!(f.configs(), 0);

    f.launcher.refuse.store(false, Ordering::SeqCst);
    f.prepare("stt").unwrap();
    assert_eq!(f.launcher.handle(2).spec.member.generation, 2);
}

#[test]
fn a_candidate_is_warm_only_with_health_ready_and_its_ledger_reported() {
    let f = fixture();
    f.prepare("stt").unwrap();
    let handle = f.launcher.handle(1);

    handle.set_contact(ControlContact::Pending);
    f.health(ACTIVE_PORT, "stt");
    assert!(!f.warm("stt").unwrap().ready, "health alone is not warm");

    handle.set_contact(ControlContact::Reporting);
    f.health(ACTIVE_PORT, "another-model");
    assert!(!f.warm("stt").unwrap().ready, "ready for another model");

    f.probe.silent.store(true, Ordering::SeqCst);
    assert!(!f.warm("stt").unwrap().ready, "a ledger alone is not warm");

    f.probe.silent.store(false, Ordering::SeqCst);
    f.health(ACTIVE_PORT, "stt");
    assert!(f.warm("stt").unwrap().ready);
}

#[test]
fn a_candidate_that_exits_while_loading_answers_with_its_failure_and_is_forgotten() {
    let f = fixture();
    f.prepare("stt").unwrap();
    f.launcher.handle(1).exited.store(true, Ordering::SeqCst);

    let err = f.warm("stt").unwrap_err();
    assert!(matches!(err, AgentError::WorkerExited(ref s) if s == "generation 1"), "{err}");
    assert!(matches!(f.warm("stt"), Err(AgentError::WorkerNotReady)));
    assert_eq!(f.configs(), 0);
}

#[test]
fn a_candidate_whose_control_channel_failed_is_killed_and_refused() {
    let f = fixture();
    f.prepare("stt").unwrap();
    let handle = f.launcher.handle(1);
    f.health(ACTIVE_PORT, "stt");
    handle.set_contact(ControlContact::Failed);

    let err = f.warm("stt").unwrap_err();
    assert!(matches!(err, AgentError::WorkerControl(_)), "{err}");
    assert_eq!(handle.calls(), ["kill"]);
    assert!(matches!(f.warm("stt"), Err(AgentError::WorkerNotReady)));
}

#[test]
fn warming_another_deployment_than_the_prepared_one_is_refused() {
    let f = fixture();
    f.prepare("stt").unwrap();
    f.health(ACTIVE_PORT, "tts");
    assert!(matches!(f.warm("tts"), Err(AgentError::WorkerControl(_))));
}

#[test]
fn promotion_retires_the_old_generation_by_asking_it_to_stop() {
    let f = fixture();
    let old = f.serve("stt");
    assert_eq!(old.config["bind"]["port"], ACTIVE_PORT);
    let new = f.serve("tts");
    assert_eq!(new.config["bind"]["port"], CANDIDATE_PORT);

    assert_eq!(old.calls(), ["terminate"]);
    assert!(new.calls().is_empty());
    assert_eq!(f.registry.active_deployment_id().unwrap().as_deref(), Some("tts"));
    assert_eq!(
        f.registry.active_serving_url().unwrap().as_deref(),
        Some(format!("http://127.0.0.1:{CANDIDATE_PORT}/infer").as_str())
    );
    assert_eq!(f.configs(), 1);

    let third = f.serve("stt");
    assert_eq!(third.config["bind"]["port"], ACTIVE_PORT);
    assert_eq!(third.spec.member.generation, 3);
}

#[test]
fn a_member_that_ignores_the_stop_request_is_killed_only_after_the_drain() {
    let f = fixture();
    let old = f.serve("stt");
    old.obeys_terminate.store(false, Ordering::SeqCst);

    let started = Instant::now();
    f.serve("tts");
    assert!(started.elapsed() >= DRAIN, "{:?}", started.elapsed());
    assert_eq!(old.calls(), ["terminate", "kill"]);
}

#[test]
fn a_second_prepare_retires_the_candidate_it_displaces() {
    let f = fixture();
    let serving = f.serve("stt");
    f.prepare("tts").unwrap();
    f.prepare("tts").unwrap();

    assert_eq!(f.launcher.handle(2).calls(), ["terminate"]);
    assert!(f.launcher.handle(3).calls().is_empty());
    assert!(serving.calls().is_empty());
    assert_eq!(f.launcher.handle(3).config["bind"]["port"], CANDIDATE_PORT);
}

#[test]
fn promoting_another_deployment_than_the_prepared_one_retires_the_candidate() {
    let f = fixture();
    f.prepare("stt").unwrap();
    assert!(matches!(
        f.registry.promote("tx", &candidate("tts")),
        Err(AgentError::WorkerControl(_))
    ));
    assert_eq!(f.launcher.handle(1).calls(), ["terminate"]);
    assert!(matches!(
        f.registry.promote("tx", &candidate("stt")),
        Err(AgentError::WorkerNotReady)
    ));
}

#[test]
fn a_serving_member_that_exited_is_no_longer_active() {
    let f = fixture();
    let serving = f.serve("stt");
    serving.exited.store(true, Ordering::SeqCst);

    assert_eq!(f.registry.active_deployment_id().unwrap(), None);
    assert_eq!(f.registry.active_serving_url().unwrap(), None);
    assert_eq!(f.configs(), 0);
}

#[test]
fn unload_stops_the_members_of_that_deployment_only() {
    let f = fixture();
    let serving = f.serve("stt");
    f.prepare("tts").unwrap();

    f.registry.unload("tts");
    assert_eq!(f.launcher.handle(2).calls(), ["terminate"]);
    assert!(serving.calls().is_empty());

    f.registry.unload("stt");
    assert_eq!(serving.calls(), ["terminate"]);
    assert_eq!(f.registry.active_deployment_id().unwrap(), None);
}

#[test]
fn dropping_the_registry_stops_every_member_it_still_runs() {
    let f = fixture();
    let serving = f.serve("stt");
    f.prepare("tts").unwrap();
    let candidate = f.launcher.handle(2);
    candidate.obeys_terminate.store(false, Ordering::SeqCst);

    drop(f.registry);
    assert_eq!(serving.calls(), ["terminate"]);
    assert_eq!(candidate.calls(), ["terminate", "kill"]);
}
