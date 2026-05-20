// SPDX-License-Identifier: Apache-2.0
//
// V01-E11-F08 CLI integration test harness.
//
// The harness spins up a deterministic agent stub on a Unix domain socket,
// lets each test queue ordered `ControlResponse` payloads, and exposes
// helpers for running the `tensorplate` binary against the stub. We also
// provide a stub serving worker for the `infer` command.
//
// The stub agent honours one request per connection (newline framed),
// matching the real agent's wire contract from V01-E08.

#![allow(
    dead_code,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::default_trait_access,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::needless_pass_by_value
)]

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use tempfile::TempDir;
use tensorplate_protocol::agent_control::{ControlRequest, ControlResponse};

/// Stub agent listening on a Unix domain socket. Queue control responses
/// with [`AgentStub::enqueue`] before the CLI issues a request.
pub struct AgentStub {
    pub td: TempDir,
    pub socket: PathBuf,
    queue: Arc<Mutex<VecDeque<ControlResponse>>>,
    history: Arc<Mutex<Vec<ControlRequest>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AgentStub {
    pub fn start() -> Self {
        let td = TempDir::new().expect("tempdir");
        let socket = td.path().join("agent.sock");
        let listener = UnixListener::bind(&socket).expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let queue: Arc<Mutex<VecDeque<ControlResponse>>> = Arc::new(Mutex::new(VecDeque::new()));
        let history = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let q_thread = queue.clone();
        let h_thread = history.clone();
        let thread = std::thread::spawn(move || {
            accept_loop(&listener, &stop_thread, &q_thread, &h_thread);
        });
        Self {
            td,
            socket,
            queue,
            history,
            stop,
            thread: Some(thread),
        }
    }

    pub fn enqueue(&self, response: ControlResponse) {
        self.queue.lock().expect("lock").push_back(response);
    }

    pub fn history(&self) -> Vec<ControlRequest> {
        self.history.lock().expect("lock").clone()
    }

    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            handle.join().expect("join");
        }
    }
}

impl Drop for AgentStub {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn accept_loop(
    listener: &UnixListener,
    stop: &Arc<AtomicBool>,
    queue: &Arc<Mutex<VecDeque<ControlResponse>>>,
    history: &Arc<Mutex<Vec<ControlRequest>>>,
) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _addr)) => {
                let _ = stream.set_nonblocking(false);
                handle_connection(stream, queue, history);
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn handle_connection(
    stream: UnixStream,
    queue: &Arc<Mutex<VecDeque<ControlResponse>>>,
    history: &Arc<Mutex<Vec<ControlRequest>>>,
) {
    let mut writer = stream.try_clone().expect("clone");
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let request: ControlRequest = match serde_json::from_str(line.trim_end()) {
        Ok(r) => r,
        Err(_) => return,
    };
    history.lock().expect("lock").push(request.clone());
    let response = queue
        .lock()
        .expect("lock")
        .pop_front()
        .expect("queue empty: enqueue a response before this request fires");
    let mut payload = serde_json::to_vec(&response).expect("serialize");
    payload.push(b'\n');
    let _ = writer.write_all(&payload);
    let _ = writer.flush();
}

/// Minimal stub serving worker. Responds with a single canned HTTP body.
pub struct ServingStub {
    pub addr: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    request_log: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl ServingStub {
    pub fn start(response_body: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        listener.set_nonblocking(true).expect("nonblocking");
        let stop = Arc::new(AtomicBool::new(false));
        let log = Arc::new(Mutex::new(Vec::new()));
        let response = response_body.to_string();
        let stop_thread = stop.clone();
        let log_thread = log.clone();
        let thread = std::thread::spawn(move || {
            while !stop_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let mut buf = vec![0u8; 8 * 1024];
                        if let Ok(n) = stream.read(&mut buf) {
                            log_thread.lock().expect("lock").push(buf[..n].to_vec());
                        }
                        let body = response.as_bytes();
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(head.as_bytes());
                        let _ = stream.write_all(body);
                        let _ = stream.flush();
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        });
        Self {
            addr,
            stop,
            thread: Some(thread),
            request_log: log,
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}/infer", self.addr)
    }

    pub fn requests(&self) -> Vec<Vec<u8>> {
        self.request_log.lock().expect("lock").clone()
    }

    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            handle.join().expect("join");
        }
    }
}

impl Drop for ServingStub {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Run the `tensorplate` binary with the supplied args against `socket`.
/// Returns `(exit_code, stdout, stderr)`.
pub fn run_cli(socket: &std::path::Path, args: &[&str]) -> (i32, String, String) {
    run_cli_with_extra_env(socket, args, &[])
}

pub fn run_cli_with_extra_env(
    socket: &std::path::Path,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> (i32, String, String) {
    let cli_config = write_default_cli_config(socket);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tensorplate"));
    cmd.env("TENSORPLATE_CLI_CONFIG", &cli_config.config_path);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.args(args);
    let out = cmd.output().expect("run cli");
    (
        out.status.code().unwrap_or(127),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

pub struct CliConfigPath {
    pub _td: TempDir,
    pub config_path: PathBuf,
}

pub fn write_default_cli_config(socket: &std::path::Path) -> CliConfigPath {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("cli.json");
    let body = format!(
        r#"{{
            "schema_version":"0.1",
            "default_profile":"local",
            "profiles":{{
                "local":{{"mode":"local","socket_path":"{}"}}
            }}
        }}"#,
        socket.display()
    );
    std::fs::write(&path, body).expect("write cli config");
    CliConfigPath {
        _td: td,
        config_path: path,
    }
}

/// Build a minimal bundle directory with a manifest.json file. Tests use
/// this when exercising the `deploy` command; the stub agent does not
/// actually validate manifest content, the CLI only checks the manifest
/// file exists before calling the agent.
pub fn write_bundle_dir() -> (TempDir, PathBuf) {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path().join("bundle");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("manifest.json"), b"{}").expect("write manifest");
    (td, dir)
}
