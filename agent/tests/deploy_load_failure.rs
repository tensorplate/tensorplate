// SPDX-License-Identifier: Apache-2.0
//
// A candidate serving worker that exits while it loads: the code its runner
// raised reaches the operator, the deploy answers without waiting out the
// warm timeout, and the agent accepts the next transaction. The stand-in
// worker replays what the real one writes to stderr (see
// `fixtures/worker_stderr/PROVENANCE.md`).

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
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::vision_bundle;
use tempfile::TempDir;
use tensorplate_agent::config::{AgentConfig, ControlTransport, WorkerControlMode};
use tensorplate_agent::control::dispatch;
use tensorplate_agent::coordinator::Coordinator;
use tensorplate_agent::state::StateStore;
use tensorplate_agent::worker::ProcessWorkerControl;
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
        config.worker.warm_timeout_ms = u64::try_from(WARM_TIMEOUT.as_millis()).expect("ms");
        config.worker.status_poll_interval_ms = 10;
        let config = config.validate().expect("valid config");
        let store = Arc::new(StateStore::open(&state_dir).expect("open store"));
        let worker = Arc::new(ProcessWorkerControl::new(&config).expect("worker"));
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

    let failed = h.deploy("fail-kokoro_undeclared_voice");
    assert_eq!(
        failed.error.expect("typed error").code,
        ErrorCode::Unsupported
    );
    let snap = h.store.snapshot().expect("snapshot");
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
