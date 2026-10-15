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
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::vision_bundle;
use rustix::net::{self, AddressFamily, SocketAddrAny, SocketFlags, SocketType};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tensorplate_agent::config::{
    AgentConfig, BackendCapability, ControlTransport, WorkerControlMode,
};
use tensorplate_agent::control::dispatch;
use tensorplate_agent::coordinator::Coordinator;
use tensorplate_agent::registry::{durable_generations, MemberRegistry};
use tensorplate_agent::state::StateStore;
use tensorplate_agent::worker::{agent_stderr_sink, WorkerStderrSink};
use tensorplate_protocol::agent_control::{
    ControlRequest, ControlResponse, DeployRequest, ResponseStatus, RollbackRequest,
};
use tensorplate_protocol::bundle_manifest::DeviceFamily;
use tensorplate_protocol::{ErrorCode, SCHEMA_VERSION};

/// Far longer than a deploy takes on a loaded machine, so that `PROMPT`, a
/// tenth of it, is a bound that load does not reach.
const WARM_TIMEOUT: Duration = Duration::from_secs(600);
/// A deploy that answers this fast did not wait for the warm timeout.
const PROMPT: Duration = Duration::from_secs(WARM_TIMEOUT.as_secs() / 10);

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
                read_request_head(&mut stream);
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

/// Reads through the blank line that ends a request head, so the close
/// after the answer is not a reset over unread bytes.
fn read_request_head(stream: &mut TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let mut head = Vec::new();
    let mut chunk = [0_u8; 512];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut chunk) {
            Ok(n) if n > 0 => head.extend_from_slice(&chunk[..n]),
            _ => break,
        }
    }
}

/// The sockets [`unused_port`] holds.
static HELD: Mutex<Vec<OwnedFd>> = Mutex::new(Vec::new());

/// A port bound and not listening until the binary exits, and not passed to
/// the processes it starts: a connection to it is refused, and neither
/// another case nor another process can bind it. A port closed once it was
/// picked could be given to another case's health stub, whose answer the
/// case's worker that does not listen would then get.
fn unused_port() -> u16 {
    let (inet, stream) = (AddressFamily::INET, SocketType::STREAM);
    let socket = net::socket_with(inet, stream, SocketFlags::CLOEXEC, None).expect("socket");
    net::bind_v4(&socket, &SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).expect("bind");
    let SocketAddrAny::V4(bound) = net::getsockname(&socket).expect("addr") else {
        panic!("an IPv4 socket bound to an IPv6 address");
    };
    HELD.lock().expect("lock").push(socket);
    bound.port()
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
        assert!(
            std::process::Command::new("python3")
                .arg("--version")
                .output()
                .is_ok_and(|output| output.status.success()),
            "the stand-in worker answers its control socket with python3, which is not on PATH"
        );
        let stub = fixture_dir().join("stub-worker.sh");
        Self::build_with(stub, active_port, candidate_port, warm_timeout, stderr_sink)
    }

    fn build_with(
        serving_binary: PathBuf,
        active_port: u16,
        candidate_port: u16,
        warm_timeout: Duration,
        stderr_sink: Option<WorkerStderrSink>,
    ) -> Self {
        Self::build_configured(
            serving_binary,
            active_port,
            candidate_port,
            warm_timeout,
            stderr_sink,
            |_| {},
        )
    }

    fn build_configured(
        serving_binary: PathBuf,
        active_port: u16,
        candidate_port: u16,
        warm_timeout: Duration,
        stderr_sink: Option<WorkerStderrSink>,
        configure: impl FnOnce(&mut AgentConfig),
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
        config.worker.serving_binary_path = Some(serving_binary);
        config.worker.serving_use_mock_session = true;
        config.worker.serving_config_dir = Some(td.path().join("worker-configs"));
        config.worker.serving_bind_port = active_port;
        config.worker.serving_candidate_bind_port = candidate_port;
        config.worker.warm_timeout_ms = u64::try_from(warm_timeout.as_millis()).expect("ms");
        config.worker.status_poll_interval_ms = 10;
        configure(&mut config);
        let config = config.validate().expect("valid config");
        let store = Arc::new(StateStore::open(&state_dir).expect("open store"));
        let worker = Arc::new(
            MemberRegistry::from_config(
                &config,
                durable_generations(store.clone()),
                stderr_sink.unwrap_or_else(agent_stderr_sink),
            )
            .expect("registry"),
        );
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

fn connect_to(health: &HealthPort) -> TcpStream {
    let stream = TcpStream::connect(("127.0.0.1", health.port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    stream
}

fn read_response(stream: &mut TcpStream) -> String {
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    response
}

#[test]
fn an_unused_port_refuses_a_connection_and_cannot_be_bound_again() {
    let port = unused_port();

    let connected = TcpStream::connect(("127.0.0.1", port));
    assert!(connected.is_err(), "{connected:?}");
    let bound = TcpListener::bind(("127.0.0.1", port));
    assert!(bound.is_err(), "{bound:?}");
}

#[cfg(target_os = "linux")]
#[test]
fn an_unused_port_is_not_held_by_a_process_the_tests_start() {
    unused_port();
    let fd = HELD.lock().expect("lock").last().expect("held").as_raw_fd();

    let script = format!("test ! -e /proc/$$/fd/{fd}");
    let shell = std::process::Command::new("sh")
        .args(["-c", &script])
        .status();

    assert!(
        shell.expect("sh").success(),
        "the shell holds descriptor {fd}"
    );
}

#[test]
fn the_health_stub_reads_a_request_that_arrives_in_two_writes() {
    let health = HealthPort::listen();
    let mut stream = connect_to(&health);
    stream
        .write_all(b"GET /health HTTP/1.1\r\n")
        .expect("write request line");
    std::thread::sleep(Duration::from_millis(200));
    stream
        .write_all(b"Host: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .expect("write headers");

    let response = read_response(&mut stream);
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let reset = stream.take_error().expect("socket error");
    assert!(
        reset.is_none(),
        "the stub closed before the headers arrived and the connection was reset: {reset:?}"
    );
}

#[test]
fn a_client_that_sends_nothing_does_not_hold_the_health_stub() {
    let health = HealthPort::listen();
    let _silent = connect_to(&health);
    let mut stream = connect_to(&health);
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .expect("write request");

    let response = read_response(&mut stream);
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
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
fn a_port_another_worker_listens_on_reaches_the_operator() {
    assert_typed_load_failure(
        "http_port_taken",
        ErrorCode::Unavailable,
        "http server: could not bind 127.0.0.1:45615: Address already in use",
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
    assert!(
        elapsed < PROMPT,
        "deploy took {elapsed:?} of a {WARM_TIMEOUT:?} warm timeout"
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

/// What the state file at `path` says of its version and counter.
fn version_and_counter(path: &Path) -> (String, Option<u64>) {
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).expect("read state")).expect("state json");
    (
        state["schema_version"]
            .as_str()
            .expect("version")
            .to_string(),
        state["next_generation"].as_u64(),
    )
}

/// The stand-in worker refuses a poll that names another member than its
/// config, so a deploy that succeeds was polled as the generation rendered.
#[test]
fn each_worker_is_started_as_a_new_durable_generation_and_stopped_by_request() {
    let first = HealthPort::listen();
    let second = HealthPort::listen();
    let h = ProcessHarness::new(first.port, second.port);
    let state_dir = h.td.path().join("state");
    let configs = h.td.path().join("worker-configs");

    first.serve("good-a");
    assert_eq!(h.deploy("good-a").status, ResponseStatus::Ok);
    for file in ["state.json", "state.json.bak"] {
        assert_eq!(
            version_and_counter(&state_dir.join(file)),
            ("0.2".to_string(), Some(2)),
            "{file}"
        );
    }
    assert!(configs.join("serving-good-a-1.json").is_file());

    second.serve("good-b");
    assert_eq!(h.deploy("good-b").status, ResponseStatus::Ok);
    assert_eq!(
        h.store.snapshot().expect("snapshot").next_generation,
        Some(3)
    );
    // Written by the replaced worker on SIGTERM; a kill would leave none.
    assert!(configs.join("good-a.terminated").is_file());
    assert!(!configs.join("serving-good-a-1.json").exists());
    assert!(configs.join("serving-good-b-2.json").is_file());

    // A failed start spends its generation too.
    assert!(h.deploy("exit-66").error.is_some());
    assert_eq!(
        h.store.snapshot().expect("snapshot").next_generation,
        Some(4)
    );
}

#[test]
fn a_worker_that_never_answers_its_control_socket_is_not_promoted() {
    let health = HealthPort::listen();
    health.serve("mute-worker");
    let warm_timeout = Duration::from_millis(1500);
    let h = ProcessHarness::build(health.port, unused_port(), warm_timeout, None);

    let started = Instant::now();
    let error = h.deploy("mute-worker").error.expect("typed error");
    assert!(started.elapsed() >= warm_timeout);
    assert_eq!(error.code, ErrorCode::NotReady, "{}", error.message);
    assert!(h.store.snapshot().expect("snapshot").active.is_none());
}

/// The agent's control client against the worker's own control thread. Run
/// with `TP_TEST_SERVING_BINARY` naming a built `tensorplate-serving`.
#[test]
#[ignore = "needs a built tensorplate-serving"]
fn a_real_worker_is_polled_from_its_start_and_promoted_as_a_member() {
    let serving = std::env::var_os("TP_TEST_SERVING_BINARY").expect("TP_TEST_SERVING_BINARY");
    let h = ProcessHarness::build_with(
        PathBuf::from(serving),
        unused_port(),
        unused_port(),
        Duration::from_secs(30),
        None,
    );

    assert_eq!(h.deploy("member-a").status, ResponseStatus::Ok);
    assert_eq!(h.deploy("member-b").status, ResponseStatus::Ok);
    assert_eq!(h.rollback().status, ResponseStatus::Ok);
    let snap = h.store.snapshot().expect("snapshot");
    assert_eq!(snap.active.expect("active").deployment_id, "member-a");
    assert_eq!(snap.next_generation, Some(4));
}

/// The unload of the previous active id that follows a promotion must not
/// reach the worker just promoted when the two ids are the same. Each
/// worker's own record shows the fence and activate it was sent.
#[test]
fn deploying_the_active_id_again_leaves_it_served() {
    let first = HealthPort::listen();
    let second = HealthPort::listen();
    let h = ProcessHarness::new(first.port, second.port);
    first.serve("same-id");
    second.serve("same-id");

    assert_eq!(h.deploy("same-id").status, ResponseStatus::Ok);
    let replacing = h.deploy("same-id");
    assert_eq!(replacing.status, ResponseStatus::Ok);

    let tx = replacing.transaction_id.expect("transaction id");
    let quota = serde_json::json!({"session_count": 0, "domain_bytes": {}});
    let configs = h.td.path().join("worker-configs");
    let first_ops = control_ops(&configs.join("same-id-1.control.jsonl"));
    let [activated, fenced] = first_ops.as_slice() else {
        panic!("the first worker was sent {first_ops:?}");
    };
    assert_eq!(activated["request"]["op"], "activate", "{first_ops:?}");
    assert_eq!(fenced["request"]["op"], "admission_fence", "{first_ops:?}");
    assert_eq!(fenced["request"]["transaction_id"], tx.as_str());
    assert_eq!(fenced["answer"]["status"], "ok");
    let second_ops = control_ops(&configs.join("same-id-2.control.jsonl"));
    let [activated] = second_ops.as_slice() else {
        panic!("the second worker was sent {second_ops:?}");
    };
    assert_eq!(activated["request"]["op"], "activate");
    assert_eq!(activated["request"]["transaction_id"], tx.as_str());
    assert_eq!(activated["answer"]["status"], "ok");
    assert_eq!(activated["answer"]["quota"], quota);

    let active = h.coord.status().expect("status").active.expect("active");
    assert_eq!(active.deployment_id, "same-id");
    assert_eq!(
        active.serving_url,
        Some(format!("http://127.0.0.1:{}/infer", second.port))
    );
    assert!(configs.join("serving-same-id-2.json").is_file());
    assert!(!configs.join("serving-same-id-1.json").exists());
}

/// The operations other than ledger polls that the stand-in worker recorded
/// in `record`, each with its answer.
fn control_ops(record: &Path) -> Vec<serde_json::Value> {
    let recorded = std::fs::read_to_string(record).unwrap_or_default();
    recorded
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("a recorded op"))
        .filter(|op| op["request"]["op"] != "ledger_status")
        .collect()
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

fn python_pytorch_capabilities() -> BTreeMap<String, BackendCapability> {
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
    capabilities
}

/// A format 0.1 bundle of the speech class names no speech block: the Kokoro
/// candidate deploys, and its worker is configured as the speech class.
#[test]
fn the_kokoro_candidate_is_served_as_the_speech_class() {
    let health = HealthPort::listen();
    let stub = fixture_dir().join("stub-worker.sh");
    let h = ProcessHarness::build_configured(
        stub,
        health.port,
        unused_port(),
        WARM_TIMEOUT,
        None,
        |config| {
            config.available_backends = vec!["python_pytorch".into()];
            config.backend_capabilities = python_pytorch_capabilities();
            config.runtime_version = Some("0.3.1".into());
        },
    );
    let bundle = h.td.path().join("kokoro-speech");
    copy_dir(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../test/models/bundles/v0_1/tts_kokoro_candidate"),
        &bundle,
    );
    health.serve("kokoro-speech");

    let response = dispatch(
        &h.coord,
        ControlRequest::deploy(
            None,
            DeployRequest {
                bundle_path: bundle.display().to_string(),
                deployment_id: "kokoro-speech".into(),
                ..DeployRequest::default()
            },
        ),
    )
    .expect("dispatch deploy");
    assert_eq!(response.status, ResponseStatus::Ok, "{:?}", response.error);

    let rendered: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            h.td.path()
                .join("worker-configs/serving-kokoro-speech-1.json"),
        )
        .expect("worker config"),
    )
    .expect("worker config JSON");
    assert_eq!(rendered["deployment"]["model"]["model_class"], "speech");
    let active = h
        .store
        .snapshot()
        .expect("snapshot")
        .active
        .expect("active");
    assert_eq!(active.model_class, "speech");
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
    let capabilities = python_pytorch_capabilities();
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
    let worker = Arc::new(
        MemberRegistry::from_config(
            &config,
            durable_generations(store.clone()),
            agent_stderr_sink(),
        )
        .expect("registry"),
    );
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

// A serving worker that stops after its promotion, and an agent that is told
// to stop: the registry's restart and shutdown with real processes.

fn pid_recording_worker() -> PathBuf {
    fixture_dir().join("pid-recording-worker.sh")
}

/// The process id `pid-recording-worker.sh` left for the worker of `config`.
fn recorded_pid(config: &Path) -> Option<rustix::process::Pid> {
    let recorded = std::fs::read_to_string(config.with_extension("json.pid")).ok()?;
    rustix::process::Pid::from_raw(recorded.trim().parse().ok()?)
}

fn is_gone(pid: rustix::process::Pid) -> bool {
    rustix::process::test_kill_process(pid) == Err(rustix::io::Errno::SRCH)
}

/// The names of the serving configs in `dir`, without the pid files.
fn serving_configs(dir: &Path) -> Vec<std::ffi::OsString> {
    std::fs::read_dir(dir)
        .expect("read_dir")
        .map(|entry| entry.expect("entry").file_name())
        .filter(|name| Path::new(name).extension().is_some_and(|ext| ext == "json"))
        .collect()
}

/// Polls until `done` holds; panics with `context` once `limit` has passed.
fn wait_for(limit: Duration, context: impl Fn() -> String, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !done() {
        assert!(
            Instant::now() < deadline,
            "not within {limit:?}: {}",
            context()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_killed_serving_worker_is_restarted_as_a_new_generation() {
    use rustix::process::{kill_process, Signal};
    use tensorplate_agent::worker::MemberEvent;
    use tensorplate_protocol::worker_control::MemberRef;

    let health = HealthPort::listen();
    health.serve("good-a");
    let h = ProcessHarness::build_with(
        pid_recording_worker(),
        health.port,
        unused_port(),
        WARM_TIMEOUT,
        None,
    );
    let configs = h.td.path().join("worker-configs");
    let deployed = h.deploy("good-a");
    assert_eq!(deployed.status, ResponseStatus::Ok, "{:?}", deployed.error);
    let serving_url = || {
        let active = h.coord.status().expect("status").active.expect("active");
        active.serving_url
    };
    let url = Some(format!("http://127.0.0.1:{}/infer", health.port));
    assert_eq!(serving_url(), url);

    let first = recorded_pid(&configs.join("serving-good-a-1.json")).expect("pid file");
    kill_process(first, Signal::Kill).expect("kill the worker");
    let killed = Instant::now();

    let mut events = Vec::new();
    while !matches!(events.last(), Some(MemberEvent::Serving { .. })) {
        assert!(
            killed.elapsed() < Duration::from_secs(15),
            "no restarted generation is serving: {events:?}"
        );
        events.extend(h.coord.worker_client().supervise());
        std::thread::sleep(Duration::from_millis(20));
    }

    let [MemberEvent::Stopped { error, .. }, ..] = &events[..] else {
        panic!("{events:?}");
    };
    let restarted = MemberRef::new("good-a", 2);
    let expected = [
        MemberEvent::Stopped {
            member: MemberRef::new("good-a", 1),
            error: error.clone(),
        },
        MemberEvent::RestartDue {
            deployment_id: "good-a".into(),
            after: Duration::from_secs(1),
        },
        MemberEvent::Restarted {
            member: restarted.clone(),
        },
        MemberEvent::Serving { member: restarted },
    ];
    assert_eq!(events, expected);
    assert!(killed.elapsed() >= Duration::from_secs(1));
    assert_eq!(serving_url(), url);
    assert_eq!(serving_configs(&configs), ["serving-good-a-2.json"]);
    assert_eq!(
        h.store.snapshot().expect("snapshot").next_generation,
        Some(3)
    );
}

/// Ends the agent, and the worker it started, however the test ends.
struct StartedAgent {
    agent: std::process::Child,
    /// The serving config of its worker, until the worker is seen to be gone.
    worker_config: Option<PathBuf>,
}

impl Drop for StartedAgent {
    fn drop(&mut self) {
        let _ = self.agent.kill();
        let _ = self.agent.wait();
        if let Some(worker) = self.worker_config.as_deref().and_then(recorded_pid) {
            let _ = rustix::process::kill_process(worker, rustix::process::Signal::Kill);
        }
    }
}

/// Start the agent binary on `config`, its stderr going to `log`.
fn start_agent_binary(config: &AgentConfig, dir: &Path, log: &Path) -> std::process::Child {
    std::process::Command::new(env!("CARGO_BIN_EXE_tensorplate-agent"))
        .arg("--config-json")
        .arg(serde_json::to_string(config).expect("config"))
        // Without a platform registry the start does not depend on the host.
        .env("TP_PLATFORM_REGISTRY_DIR", dir.join("no-registry"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(log).expect("log"))
        .spawn()
        .expect("start the agent")
}

fn status_over(socket: &Path) -> ControlResponse {
    let mut stream = std::os::unix::net::UnixStream::connect(socket).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    let mut request =
        serde_json::to_vec(&ControlRequest::status(None, Default::default())).expect("encode");
    request.push(b'\n');
    stream.write_all(&request).expect("write request");
    let mut answer = String::new();
    stream.read_to_string(&mut answer).expect("read answer");
    serde_json::from_str(&answer).expect("decode answer")
}

#[test]
fn an_agent_sent_sigterm_drains_its_serving_worker_and_exits_0() {
    a_signalled_agent_restarts_a_killed_worker_then_drains_it_and_exits_0(
        rustix::process::Signal::Term,
    );
}

#[test]
fn an_agent_sent_sigint_drains_its_serving_worker_and_exits_0() {
    a_signalled_agent_restarts_a_killed_worker_then_drains_it_and_exits_0(
        rustix::process::Signal::Int,
    );
}

/// The agent binary restores a deployment that an earlier agent process
/// recorded: a deploy through it needs a platform the test host may not be.
fn a_signalled_agent_restarts_a_killed_worker_then_drains_it_and_exits_0(
    signal: rustix::process::Signal,
) {
    use rustix::process::{kill_process, Pid, Signal};
    use tensorplate_agent::registry::STOP_DEADLINE;

    let health = HealthPort::listen();
    health.serve("good-a");
    let earlier = ProcessHarness::build_with(
        pid_recording_worker(),
        health.port,
        unused_port(),
        WARM_TIMEOUT,
        None,
    );
    let deployed = earlier.deploy("good-a");
    assert_eq!(deployed.status, ResponseStatus::Ok, "{:?}", deployed.error);
    let mut config = earlier.coord.config().clone();
    let ProcessHarness { td, store, coord } = earlier;
    let earlier_worker = recorded_pid(&td.path().join("worker-configs/serving-good-a-1.json"));
    drop((coord, store));
    assert!(is_gone(earlier_worker.expect("pid file")));

    let configs = td.path().join("agent-worker-configs");
    let restored_config = configs.join("serving-good-a-2.json");
    let worker_config = configs.join("serving-good-a-3.json");
    config.worker.serving_config_dir = Some(configs.clone());
    let log = td.path().join("agent.stderr");
    let mut started = StartedAgent {
        agent: start_agent_binary(&config, td.path(), &log),
        worker_config: Some(restored_config.clone()),
    };
    let stderr = || std::fs::read_to_string(&log).unwrap_or_default();
    wait_for(Duration::from_secs(30), stderr, || {
        let exited = started.agent.try_wait().expect("wait");
        assert!(exited.is_none(), "the agent exited: {}", stderr());
        stderr().contains("tensorplate-agent listening on ")
    });
    // Only the agent's own loop can replace the worker it restored.
    let restored = recorded_pid(&restored_config).expect("pid file");
    kill_process(restored, Signal::Kill).expect("kill the worker");
    started.worker_config = Some(worker_config.clone());
    wait_for(Duration::from_secs(15), stderr, || {
        stderr().contains("serving worker `good-a` generation 3 is serving")
    });
    assert!(is_gone(restored));
    let worker = recorded_pid(&worker_config).expect("pid file");
    let status = status_over(&config.socket_path.expect("socket path")).agent_status;
    let active = status.expect("agent status").active.expect("active");
    assert_eq!(
        active.serving_url,
        Some(format!("http://127.0.0.1:{}/infer", health.port))
    );
    // The handlers were installed before the listening line was written.
    kill_process(Pid::from_child(&started.agent), signal).expect("signal the agent");
    let mut exit = None;
    wait_for(STOP_DEADLINE + Duration::from_secs(5), stderr, || {
        exit = started.agent.try_wait().expect("wait");
        exit.is_some()
    });

    let stderr = stderr();
    assert_eq!(exit.and_then(|status| status.code()), Some(0), "{stderr}");
    let stopping = stderr
        .lines()
        .position(|line| line.starts_with("tensorplate-agent stopping"));
    let stopped = stderr
        .lines()
        .position(|line| line == "tensorplate-agent stopped");
    assert!(stopping.is_some() && stopping < stopped, "{stderr}");
    assert!(is_gone(worker), "the worker outlived the agent");
    started.worker_config = None;
    // Written by the worker on SIGTERM; a kill would leave none.
    assert!(configs.join("good-a.terminated").is_file());
}

/// The handlers are in place before the agent says it is listening: a
/// signal sent as soon as that line is written stops the agent, where an
/// agent without them would be killed by it.
#[test]
fn an_agent_signalled_as_soon_as_it_says_it_is_listening_stops_and_exits_0() {
    use rustix::process::{kill_process, Pid, Signal};

    let h = ProcessHarness::build_with(
        pid_recording_worker(),
        unused_port(),
        unused_port(),
        WARM_TIMEOUT,
        None,
    );
    let config = h.coord.config().clone();
    let ProcessHarness { td, store, coord } = h;
    drop((coord, store));
    let log = td.path().join("agent.stderr");
    let mut started = StartedAgent {
        agent: start_agent_binary(&config, td.path(), &log),
        worker_config: None,
    };
    let stderr = || std::fs::read_to_string(&log).unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !stderr().contains("tensorplate-agent listening on ") {
        assert!(Instant::now() < deadline, "never listening: {}", stderr());
        std::thread::yield_now();
    }

    kill_process(Pid::from_child(&started.agent), Signal::Term).expect("signal the agent");

    let mut exit = None;
    wait_for(Duration::from_secs(15), stderr, || {
        exit = started.agent.try_wait().expect("wait");
        exit.is_some()
    });
    let stderr = stderr();
    assert_eq!(exit.and_then(|status| status.code()), Some(0), "{stderr}");
    assert!(stderr.ends_with("tensorplate-agent stopped\n"), "{stderr}");
}
