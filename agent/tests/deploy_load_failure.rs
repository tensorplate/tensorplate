// SPDX-License-Identifier: Apache-2.0
//
// A candidate serving worker that exits while it loads: the code its runner
// raised reaches the operator, the deploy answers without waiting out the
// warm timeout, the worker's stderr is kept, and the agent accepts the next
// transaction. The stand-in worker replays what the real one writes to
// stderr (see `fixtures/worker_stderr/PROVENANCE.md`).

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::default_trait_access
)]

mod common;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::vision_bundle;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tensorplate_agent::config::{
    AgentConfig, BackendCapability, ControlTransport, WorkerControlMode,
};
use tensorplate_agent::control::dispatch;
use tensorplate_agent::coordinator::Coordinator;
use tensorplate_agent::state::StateStore;
use tensorplate_agent::worker::{ProcessWorkerControl, WorkerStderrSink};
use tensorplate_protocol::agent_control::{
    ControlRequest, ControlResponse, DeployRequest, ResponseStatus, RollbackRequest,
};
use tensorplate_protocol::bundle_manifest::DeviceFamily;
use tensorplate_protocol::{ErrorCode, SCHEMA_VERSION};

const WARM_TIMEOUT: Duration = Duration::from_secs(10);
/// A deploy that answers this fast did not wait for the warm timeout.
const PROMPT: Duration = Duration::from_secs(5);

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/worker_stderr")
}

/// Answers `/health` on one port as a warmed worker for whichever
/// deployment id the test last assigned to it.
struct HealthPort {
    port: u16,
    serving: Arc<Mutex<String>>,
}

impl HealthPort {
    fn listen() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let serving = Arc::new(Mutex::new(String::new()));
        let answer = serving.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut request = [0_u8; 512];
                let _ = stream.read(&mut request);
                let body = serde_json::json!({
                    "state": "ready",
                    "active_model_id": *answer.lock().expect("lock"),
                })
                .to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Self { port, serving }
    }

    fn serve(&self, deployment_id: &str) {
        *self.serving.lock().expect("lock") = deployment_id.to_string();
    }
}

fn unused_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

struct ProcessHarness {
    td: TempDir,
    store: Arc<StateStore>,
    coord: Arc<Coordinator>,
}

impl ProcessHarness {
    fn new(active_port: u16, candidate_port: u16) -> Self {
        Self::build(active_port, candidate_port, WARM_TIMEOUT, None)
    }

    fn build(
        active_port: u16,
        candidate_port: u16,
        warm_timeout: Duration,
        stderr_sink: Option<WorkerStderrSink>,
    ) -> Self {
        let td = TempDir::new().expect("td");
        let state_dir = td.path().join("state");
        let mut config = AgentConfig {
            schema_version: SCHEMA_VERSION.to_string(),
            transport: ControlTransport::UnixSocket,
            socket_path: Some(td.path().join("agent.sock")),
            tcp_bind_host: "127.0.0.1".into(),
            tcp_bind_port: 0,
            state_dir: state_dir.clone(),
            staging_dir: td.path().join("staging"),
            available_backends: vec!["mock".into()],
            backend_capabilities: BTreeMap::new(),
            memory_admission: None,
            device_memory_bytes: Some(8 * 1024 * 1024 * 1024),
            device_family: DeviceFamily::Any,
            admission_posture: None,
            worker: Default::default(),
            supervision: None,
            runtime_version: Some("0.1.0".into()),
        };
        config.worker.mode = WorkerControlMode::Process;
        config.worker.serving_binary_path = Some(fixture_dir().join("stub-worker.sh"));
        config.worker.serving_config_dir = Some(td.path().join("worker-configs"));
        config.worker.serving_bind_port = active_port;
        config.worker.serving_candidate_bind_port = candidate_port;
        config.worker.warm_timeout_ms = u64::try_from(warm_timeout.as_millis()).expect("ms");
        config.worker.status_poll_interval_ms = 10;
        let config = config.validate().expect("valid config");
        let store = Arc::new(StateStore::open(&state_dir).expect("open store"));
        let mut worker = ProcessWorkerControl::new(&config).expect("worker");
        if let Some(sink) = stderr_sink {
            worker = worker.with_stderr_sink(sink);
        }
        let worker = Arc::new(worker);
        let coord = Arc::new(Coordinator::new(config, store.clone(), worker));
        Self { td, store, coord }
    }

    fn deploy(&self, deployment_id: &str) -> ControlResponse {
        let bundle = vision_bundle(self.td.path(), deployment_id);
        let request = ControlRequest::deploy(
            None,
            DeployRequest {
                bundle_path: bundle.display().to_string(),
                deployment_id: deployment_id.to_string(),
                ..DeployRequest::default()
            },
        );
        dispatch(&self.coord, request).expect("dispatch deploy")
    }

    fn rollback(&self) -> ControlResponse {
        let request = ControlRequest::rollback(None, RollbackRequest::default());
        dispatch(&self.coord, request).expect("dispatch rollback")
    }
}

fn assert_typed_load_failure(fixture: &str, code: ErrorCode, message: &str) {
    let h = ProcessHarness::new(unused_port(), unused_port());
    let deployment_id = format!("fail-{fixture}");

    let started = Instant::now();
    let response = h.deploy(&deployment_id);
    let elapsed = started.elapsed();

    assert_eq!(response.status, ResponseStatus::Error);
    let error = response.error.expect("typed error");
    assert_eq!(error.code, code, "{}", error.message);
    assert!(error.message.contains(message), "{}", error.message);
    assert!(
        elapsed < PROMPT,
        "deploy took {elapsed:?} of a {WARM_TIMEOUT:?} warm timeout"
    );

    let snap = h.store.snapshot().expect("snapshot");
    assert!(snap.in_flight_transaction.is_none());
    assert!(snap.active.is_none());
    assert_eq!(snap.quarantined.len(), 1);
    assert_eq!(snap.quarantined[0].deployment_id, deployment_id);
    assert_eq!(snap.quarantined[0].error.code, code);
    assert_eq!(snap.last_error.expect("last_error").code, code);
}

#[test]
fn unsupported_raised_at_load_reaches_the_operator() {
    assert_typed_load_failure(
        "kokoro_undeclared_voice",
        ErrorCode::Unsupported,
        "the selected language or voice is not declared",
    );
}

#[test]
fn oom_error_raised_at_load_reaches_the_operator() {
    assert_typed_load_failure(
        "oom_at_load",
        ErrorCode::OomError,
        "the Kokoro model could not be loaded",
    );
}

#[test]
fn rollback_right_after_a_failed_deploy_is_accepted() {
    let first = HealthPort::listen();
    let second = HealthPort::listen();
    let h = ProcessHarness::new(first.port, second.port);

    first.serve("good-a");
    assert_eq!(h.deploy("good-a").status, ResponseStatus::Ok);
    second.serve("good-b");
    assert_eq!(h.deploy("good-b").status, ResponseStatus::Ok);

    let started = Instant::now();
    let failed = h.deploy("fail-kokoro_undeclared_voice");
    let elapsed = started.elapsed();
    assert_eq!(
        failed.error.expect("typed error").code,
        ErrorCode::Unsupported
    );
    // What makes the rollback acceptable: the deploy has answered within a
    // CLI's patience and left no transaction behind.
    assert!(elapsed < PROMPT, "deploy took {elapsed:?}");
    let snap = h.store.snapshot().expect("snapshot");
    assert!(snap.in_flight_transaction.is_none());
    assert_eq!(snap.active.expect("active").deployment_id, "good-b");

    let rolled_back = h.rollback();
    assert_eq!(rolled_back.status, ResponseStatus::Ok);
    let snap = h.store.snapshot().expect("snapshot");
    assert_eq!(snap.active.expect("active").deployment_id, "good-a");
    assert_eq!(
        snap.previous_active.expect("previous").deployment_id,
        "good-b"
    );
}

#[test]
fn exit_without_a_startup_record_is_load_failed_with_the_exit_status() {
    let h = ProcessHarness::new(unused_port(), unused_port());

    let started = Instant::now();
    let response = h.deploy("exit-66");
    let elapsed = started.elapsed();

    let error = response.error.expect("typed error");
    assert_eq!(error.code, ErrorCode::LoadFailed, "{}", error.message);
    assert!(
        error.message.contains("exit status: 66"),
        "{}",
        error.message
    );
    // Its stderr closed with it, so the agent does not wait for a record.
    assert!(
        elapsed < Duration::from_millis(750),
        "deploy took {elapsed:?}"
    );
}

#[test]
fn a_record_that_arrives_just_after_the_exit_is_still_read() {
    let h = ProcessHarness::new(unused_port(), unused_port());

    let error = h
        .deploy("late-kokoro_undeclared_voice")
        .error
        .expect("typed error");

    assert_eq!(error.code, ErrorCode::Unsupported, "{}", error.message);
}

#[test]
fn a_rollback_whose_candidate_exits_fails_with_the_workers_code() {
    let first = HealthPort::listen();
    let second = HealthPort::listen();
    let h = ProcessHarness::new(first.port, second.port);
    first.serve("again-kokoro_undeclared_voice");
    assert_eq!(
        h.deploy("again-kokoro_undeclared_voice").status,
        ResponseStatus::Ok
    );
    second.serve("good-b");
    assert_eq!(h.deploy("good-b").status, ResponseStatus::Ok);
    first.serve("");

    let started = Instant::now();
    let response = h.rollback();
    let elapsed = started.elapsed();

    let error = response.error.expect("typed error");
    assert_eq!(error.code, ErrorCode::Unsupported, "{}", error.message);
    assert!(elapsed < PROMPT, "rollback took {elapsed:?}");
    let snap = h.store.snapshot().expect("snapshot");
    assert!(snap.in_flight_transaction.is_none());
    assert_eq!(snap.active.expect("active").deployment_id, "good-b");
}

#[test]
fn the_workers_stderr_is_forwarded_line_for_line() {
    let forwarded = Arc::new(Mutex::new(Vec::new()));
    let sink = forwarded.clone();
    let h = ProcessHarness::build(
        unused_port(),
        unused_port(),
        WARM_TIMEOUT,
        Some(Arc::new(move |line: &[u8]| {
            sink.lock().expect("lock").extend_from_slice(line);
        })),
    );

    let response = h.deploy("fail-kokoro_undeclared_voice");

    assert_eq!(
        response.error.expect("typed error").code,
        ErrorCode::Unsupported
    );
    let recorded =
        std::fs::read(fixture_dir().join("kokoro_undeclared_voice.stderr")).expect("fixture");
    assert_eq!(*forwarded.lock().expect("lock"), recorded);
}

#[test]
fn a_candidate_that_stays_alive_still_waits_out_the_warm_timeout() {
    let warm_timeout = Duration::from_millis(400);

    let h = ProcessHarness::build(unused_port(), unused_port(), warm_timeout, None);
    let started = Instant::now();
    let error = h.deploy("silent").error.expect("typed error");
    assert!(started.elapsed() >= warm_timeout);
    assert_eq!(error.code, ErrorCode::InferenceFailed, "{}", error.message);

    let other = HealthPort::listen();
    other.serve("another-deployment");
    let h = ProcessHarness::build(other.port, unused_port(), warm_timeout, None);
    let started = Instant::now();
    let error = h.deploy("not-warmed").error.expect("typed error");
    assert!(started.elapsed() >= warm_timeout);
    assert_eq!(error.code, ErrorCode::NotReady, "{}", error.message);
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("mkdir");
    for entry in std::fs::read_dir(from).expect("read_dir") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("copy");
        }
    }
}

/// The Kokoro candidate bundle fixture with its runner entry selecting a
/// voice it does not declare, and the manifest's digest of the entry redone.
fn kokoro_bundle_with_undeclared_voice(dir: &Path) -> PathBuf {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../test/models/bundles/v0_1/tts_kokoro_candidate");
    let bundle = dir.join("kokoro-undeclared-voice");
    copy_dir(&source, &bundle);
    let entry_path = bundle.join("tts-kokoro-candidate.json");
    let entry = std::fs::read_to_string(&entry_path).expect("entry");
    assert!(entry.contains(r#""voice": "af_heart""#));
    let entry = entry.replace(r#""voice": "af_heart""#, r#""voice": "af_undeclared""#);
    std::fs::write(&entry_path, &entry).expect("write entry");

    let manifest_path = bundle.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).expect("manifest"))
            .expect("manifest JSON");
    let artifact = manifest["artifacts"]
        .as_array_mut()
        .expect("artifacts")
        .iter_mut()
        .find(|a| a["path"] == "tts-kokoro-candidate.json")
        .expect("entry artifact");
    artifact["digest"] = format!("sha256:{}", hex::encode(Sha256::digest(entry.as_bytes()))).into();
    artifact["byte_size"] = entry.len().into();
    std::fs::write(&manifest_path, manifest.to_string()).expect("write manifest");
    bundle
}

/// The reproduction through the real serving worker and Python sidecar,
/// for a host that has both: set `TP_TEST_SERVING_BINARY` to a built
/// `tensorplate-serving` and make `tensorplate_pytorch_backend` importable
/// by the interpreter the worker starts.
#[test]
#[ignore = "needs a built tensorplate-serving and the Python sidecar module"]
fn real_worker_and_sidecar_report_an_undeclared_voice_as_unsupported() {
    let serving = std::env::var_os("TP_TEST_SERVING_BINARY").expect("TP_TEST_SERVING_BINARY");
    let td = TempDir::new().expect("td");
    let state_dir = td.path().join("state");
    let mut capabilities = BTreeMap::new();
    capabilities.insert(
        "python_pytorch".to_string(),
        BackendCapability {
            async_: true,
            streaming: false,
            generation: false,
            kv_cache: false,
            fixed_shape: false,
            deterministic_latency: true,
            control_loop_integration: true,
            supported_precision: vec!["auto".into(), "fp32".into()],
            supported_artifact_kinds: vec![
                "python_pytorch_entry".into(),
                "weights".into(),
                "auxiliary".into(),
            ],
        },
    );
    let mut config = AgentConfig {
        schema_version: SCHEMA_VERSION.to_string(),
        transport: ControlTransport::UnixSocket,
        socket_path: Some(td.path().join("agent.sock")),
        tcp_bind_host: "127.0.0.1".into(),
        tcp_bind_port: 0,
        state_dir: state_dir.clone(),
        staging_dir: td.path().join("staging"),
        available_backends: vec!["python_pytorch".into()],
        backend_capabilities: capabilities,
        memory_admission: None,
        device_memory_bytes: None,
        device_family: DeviceFamily::Any,
        admission_posture: None,
        worker: Default::default(),
        supervision: None,
        runtime_version: Some("0.3.1".into()),
    };
    config.worker.mode = WorkerControlMode::Process;
    config.worker.serving_binary_path = Some(PathBuf::from(serving));
    config.worker.serving_config_dir = Some(td.path().join("worker-configs"));
    config.worker.serving_bind_port = unused_port();
    config.worker.serving_candidate_bind_port = unused_port();
    let config = config.validate().expect("valid config");
    let warm_timeout = Duration::from_millis(config.worker.warm_timeout_ms);
    let store = Arc::new(StateStore::open(&state_dir).expect("open store"));
    let worker = Arc::new(ProcessWorkerControl::new(&config).expect("worker"));
    let coord = Arc::new(Coordinator::new(config, store, worker));
    let bundle = kokoro_bundle_with_undeclared_voice(td.path());

    let started = Instant::now();
    let response = dispatch(
        &coord,
        ControlRequest::deploy(
            None,
            DeployRequest {
                bundle_path: bundle.display().to_string(),
                deployment_id: "kokoro-undeclared-voice".into(),
                ..DeployRequest::default()
            },
        ),
    )
    .expect("dispatch deploy");
    let elapsed = started.elapsed();

    let error = response.error.expect("typed error");
    assert_eq!(error.code, ErrorCode::Unsupported, "{}", error.message);
    assert!(
        error
            .message
            .contains("the selected language or voice is not declared"),
        "{}",
        error.message
    );
    assert!(
        elapsed < warm_timeout / 2,
        "deploy took {elapsed:?} of a {warm_timeout:?} warm timeout"
    );
}
