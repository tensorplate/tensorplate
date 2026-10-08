// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
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
    /// The first launch says it has started, then waits to be let finish.
    hold_first: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
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
        let held = self.hold_first.lock().unwrap().take();
        if let Some((started, release)) = held {
            started.send(()).unwrap();
            release.recv().unwrap();
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
    /// Runs inside each probe, before it answers.
    during: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl HealthProbe for FakeProbe {
    fn ready_model(&self, port: u16, _timeout: Duration) -> AgentResult<Option<String>> {
        if let Some(during) = self.during.lock().unwrap().as_ref() {
            during();
        }
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
        worker_drain: DRAIN / 2,
        stop_deadline: DRAIN,
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

fn bundle_fixture(bundle: &str) -> String {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../test/models/bundles")
        .join(bundle)
        .display()
        .to_string()
}

fn candidate(id: &str) -> CandidateRef {
    CandidateRef {
        deployment_id: id.into(),
        staged_path: bundle_fixture("v0_1/x86_fixture_smoke"),
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
        let handle = self
            .launcher
            .launched
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap();
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
        assert_eq!(
            deployment["model"]["artifact_path"],
            format!("{}/model.bin", bundle_fixture("v0_1/x86_fixture_smoke"))
        );
        assert_eq!(handle.config["bind"]["port"], ACTIVE_PORT);
        assert_eq!(handle.config["bind"]["host"], "127.0.0.1");
        assert_eq!(handle.config["shutdown"]["drain_deadline_ms"], 30);
        assert_eq!(deployment["use_mock_session"], true);
    }
    assert_eq!(f.launcher.count(), 2);
}

#[test]
fn a_member_name_the_worker_would_refuse_never_takes_a_generation() {
    let f = fixture();
    let err = f.prepare("not a member name").unwrap_err();
    assert!(matches!(err, AgentError::Config(_)), "{err}");
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
        Arc::new(|| {
            Err(AgentError::StateIndeterminate(
                "write outcome unknown".into(),
            ))
        }),
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
    let launched = f.launcher.launched.lock().unwrap();
    let generations: Vec<u64> = launched.iter().map(|h| h.spec.member.generation).collect();
    assert_eq!(generations, [2]);
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
    assert!(
        matches!(err, AgentError::WorkerExited(ref s) if s == "generation 1"),
        "{err}"
    );
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
    assert_eq!(
        f.registry.active_deployment_id().unwrap().as_deref(),
        Some("tts")
    );
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

#[test]
fn the_rendered_config_names_the_runner_profile_the_staged_bundle_selects() {
    let f = fixture();
    for (generation, (bundle, expected)) in [
        ("v0_2/speech_stt_streaming", Some("faster_whisper")),
        ("v0_2/speech_tts_streaming", Some("kokoro")),
        ("v0_1/x86_fixture_smoke", None),
        // Read without hashing: its artifact does not match its digest.
        ("v0_1/invalid_corrupt_artifact", None),
    ]
    .into_iter()
    .enumerate()
    {
        let staged = CandidateRef {
            staged_path: bundle_fixture(bundle),
            ..candidate("deploy-1")
        };
        f.registry.prepare("tx", &staged, WARM).unwrap();
        let handle = f.launcher.handle(generation as u64 + 1);
        let model = handle.config["deployment"]["model"].as_object().unwrap();
        assert_eq!(
            model.get("runner_profile").and_then(|v| v.as_str()),
            expected,
            "{bundle}"
        );
        assert_eq!(model.contains_key("runner_profile"), expected.is_some());
    }
}

#[test]
fn nothing_is_rendered_or_started_for_a_staged_bundle_that_cannot_be_read() {
    let f = fixture();
    let staged_at = |name: &str, manifest: Option<&str>| {
        let dir = f.td.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        if let Some(text) = manifest {
            std::fs::write(dir.join("manifest.json"), text).unwrap();
        }
        CandidateRef {
            staged_path: dir.display().to_string(),
            ..candidate("deploy-1")
        }
    };

    let gone = CandidateRef {
        staged_path: f.td.path().join("gone").display().to_string(),
        ..candidate("deploy-1")
    };
    let refused = f.registry.prepare("tx", &gone, WARM);
    assert!(
        matches!(refused, Err(AgentError::BundleMissing(_))),
        "{refused:?}"
    );
    // Never read as a bundle that names no profile.
    for (name, manifest) in [
        ("no-manifest", None),
        ("truncated", Some("{")),
        ("not-a-manifest", Some("{}")),
    ] {
        let refused = f.registry.prepare("tx", &staged_at(name, manifest), WARM);
        assert!(
            matches!(refused, Err(AgentError::BundleManifest(_))),
            "{name}: {refused:?}"
        );
    }
    assert_eq!(f.launcher.count(), 0);
    assert_eq!(f.configs(), 0);
}

#[test]
fn configs_left_by_an_earlier_agent_process_are_removed_and_nothing_else() {
    let td = TempDir::new().unwrap();
    for name in [
        "serving-stt-3.json",
        "serving-tts-18081.json",
        "notes.txt",
        "other.json",
    ] {
        std::fs::write(td.path().join(name), "{}").unwrap();
    }
    remove_stale_configs(td.path());
    let mut left: Vec<_> = std::fs::read_dir(td.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(left, ["notes.txt", "other.json"]);
    remove_stale_configs(&td.path().join("absent"));
}

#[test]
fn a_health_read_that_times_out_is_not_ready_and_a_refused_one_is_an_error() {
    let probe = HttpHealth {
        host: "127.0.0.1".into(),
    };
    let listening = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listening.local_addr().unwrap().port();
    assert_eq!(
        probe.ready_model(port, Duration::from_millis(30)).unwrap(),
        None
    );

    drop(listening);
    let refused = probe.ready_model(port, Duration::from_millis(30));
    assert!(
        matches!(refused, Err(AgentError::WorkerControl(_))),
        "{refused:?}"
    );
}

/// Answers one `/health` request with `body`.
fn health_listener(body: &'static str) -> u16 {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut head = Vec::new();
        let mut chunk = [0_u8; 512];
        while !head.windows(4).any(|window| window == b"\r\n\r\n") {
            match stream.read(&mut chunk) {
                Ok(n) if n > 0 => head.extend_from_slice(&chunk[..n]),
                _ => break,
            }
        }
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    });
    port
}

#[test]
fn only_a_health_answer_in_the_ready_state_names_a_ready_model() {
    let probe = HttpHealth {
        host: "127.0.0.1".into(),
    };
    let timeout = Duration::from_secs(2);
    let loading = health_listener(r#"{"state":"loading","active_model_id":"stt"}"#);
    assert_eq!(probe.ready_model(loading, timeout).unwrap(), None);
    let ready = health_listener(r#"{"state":"ready","active_model_id":"stt"}"#);
    assert_eq!(
        probe.ready_model(ready, timeout).unwrap().as_deref(),
        Some("stt")
    );
}

#[test]
fn building_the_agents_registry_clears_the_configs_an_earlier_process_left() {
    use crate::config::{AgentConfig, ControlTransport, WorkerControlMode};
    let td = TempDir::new().unwrap();
    let configs = td.path().join("worker-configs");
    std::fs::create_dir_all(&configs).unwrap();
    std::fs::write(configs.join("serving-stt-1.json"), "{}").unwrap();
    let mut agent = AgentConfig {
        schema_version: tensorplate_protocol::SCHEMA_VERSION.to_string(),
        transport: ControlTransport::UnixSocket,
        socket_path: Some(td.path().join("agent.sock")),
        tcp_bind_host: "127.0.0.1".into(),
        tcp_bind_port: 0,
        state_dir: td.path().join("state"),
        staging_dir: td.path().join("staging"),
        available_backends: vec!["mock".into()],
        backend_capabilities: BTreeMap::new(),
        memory_admission: None,
        device_memory_bytes: None,
        device_family: tensorplate_protocol::bundle_manifest::DeviceFamily::Any,
        admission_posture: None,
        worker: crate::config::WorkerConfig::default(),
        supervision: None,
        runtime_version: None,
    };
    agent.worker.mode = WorkerControlMode::Process;
    agent.worker.serving_binary_path = Some("/usr/local/bin/tensorplate-serving".into());
    agent.worker.serving_config_dir = Some(configs.clone());

    let registry = MemberRegistry::from_config(
        &agent,
        Arc::new(|| Ok(1)),
        crate::worker::agent_stderr_sink(),
    )
    .unwrap();
    assert_eq!(std::fs::read_dir(&configs).unwrap().count(), 0);

    // The worker is told a drain that ends before the registry would kill it.
    assert_eq!(registry.settings.worker_drain, WORKER_DRAIN);
    assert_eq!(registry.settings.stop_deadline, STOP_DEADLINE);
    let rendered = registry.render_config(&candidate("stt"), 1, 18080).unwrap();
    let rendered: serde_json::Value =
        serde_json::from_slice(&std::fs::read(rendered).unwrap()).unwrap();
    let drain_ms = rendered["shutdown"]["drain_deadline_ms"].as_u64().unwrap();
    assert_eq!(u128::from(drain_ms), WORKER_DRAIN.as_millis());
    assert!(u128::from(drain_ms) < registry.settings.stop_deadline.as_millis());
}

#[test]
fn a_candidate_that_exits_after_it_was_warmed_is_not_promoted_and_the_serving_member_stays() {
    let f = fixture();
    let serving = f.serve("stt");
    f.prepare("tts").unwrap();
    f.health(CANDIDATE_PORT, "tts");
    assert!(f.warm("tts").unwrap().ready);
    f.launcher.handle(2).exited.store(true, Ordering::SeqCst);

    let err = f.registry.promote("tx", &candidate("tts")).unwrap_err();
    assert!(
        matches!(err, AgentError::WorkerExited(ref s) if s == "generation 2"),
        "{err}"
    );
    assert!(serving.calls().is_empty());
    assert_eq!(
        f.registry.active_deployment_id().unwrap().as_deref(),
        Some("stt")
    );
    assert!(matches!(
        f.registry.promote("tx", &candidate("tts")),
        Err(AgentError::WorkerNotReady)
    ));
    assert_eq!(f.configs(), 1);
}

#[test]
fn an_exit_during_the_last_probe_outranks_the_warm_deadline() {
    let f = fixture();
    f.prepare("stt").unwrap();
    let handle = f.launcher.handle(1);
    *f.probe.during.lock().unwrap() = Some(Box::new(move || {
        handle.exited.store(true, Ordering::SeqCst);
        std::thread::sleep(WARM * 2);
    }));

    let err = f.warm("stt").unwrap_err();
    assert!(matches!(err, AgentError::WorkerExited(_)), "{err}");
}

#[test]
fn a_candidate_that_took_the_slot_during_a_launch_is_retired_not_forgotten() {
    let f = fixture();
    let (started, has_started) = mpsc::channel();
    let (release, released) = mpsc::channel();
    *f.launcher.hold_first.lock().unwrap() = Some((started, released));

    std::thread::scope(|scope| {
        let held = scope.spawn(|| f.prepare("stt"));
        // The held launch has generation 1; this one takes 2 and the slot.
        has_started.recv().unwrap();
        f.prepare("stt").unwrap();
        release.send(()).unwrap();
        held.join().unwrap().unwrap();
    });

    assert_eq!(f.launcher.handle(2).calls(), ["terminate"]);
    assert!(f.launcher.handle(1).calls().is_empty());
    assert_eq!(f.configs(), 1);
}
