// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F01: Local control API I/O loop.
//
// Wire format: newline-delimited JSON over a Unix domain socket. One
// request per connection, one response, then close. Concurrent
// connections are accepted on dedicated worker threads; mutating
// operations are serialized by the [`Coordinator`]'s internal state
// store so a second `deploy` while one is in flight returns
// `agent_busy` deterministically.
//
// The agent always binds the socket inside its `state_dir`, removes any
// stale file before binding, and sets group-accessible permissions (`0o660`)
// so `tensorplate`-group members can reach it without world access.
// Loopback TCP is supported only as an escape hatch and is gated by an
// explicit config opt-in.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use tensorplate_protocol::agent_control::{
    ControlRequest, ControlResponse, ResponseError, ResponseStatus,
};
use tensorplate_protocol::{decode_with_version_check, DecodeError, ErrorCode};

use crate::config::{AgentConfig, ControlTransport};
use crate::control::dispatch;
use crate::coordinator::Coordinator;
use crate::error::{AgentError, AgentResult};

/// Running server handle. Dropping the [`Server`] stops the listener
/// thread and unlinks the socket file (UDS transport only).
pub struct Server {
    transport: ServerTransport,
    coordinator: Arc<Coordinator>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Resolved local address. For UDS this is the socket file path; for
    /// loopback TCP this is `host:port` of the bound socket.
    pub address: String,
}

enum ServerTransport {
    UnixSocket { path: PathBuf },
    LoopbackTcp,
}

impl Server {
    /// Bind the configured transport and start the listener thread.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Io`] for bind failures and
    /// [`AgentError::Config`] for transport configuration errors that
    /// were not caught by [`AgentConfig::validate`].
    pub fn start(config: &AgentConfig, coordinator: Arc<Coordinator>) -> AgentResult<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        match config.transport {
            ControlTransport::UnixSocket => {
                let path = config
                    .socket_path
                    .clone()
                    .ok_or_else(|| AgentError::Config("socket_path missing".into()))?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                if path.exists() {
                    std::fs::remove_file(&path)?;
                }
                #[cfg(unix)]
                let listener = UnixListener::bind(&path)?;
                #[cfg(unix)]
                {
                    // Group-accessible (0o660): CLI clients reach the agent by
                    // membership in the `tensorplate` group, without making the
                    // socket world-writable. See install_paths::mode::SOCKET_0660.
                    let perms = std::fs::Permissions::from_mode(
                        tensorplate_protocol::install_paths::mode::SOCKET_0660,
                    );
                    std::fs::set_permissions(&path, perms)?;
                    listener.set_nonblocking(true)?;
                }
                #[cfg(not(unix))]
                {
                    let _ = listener_unused();
                    return Err(AgentError::Config(
                        "unix_socket transport is only supported on unix platforms".into(),
                    ));
                }
                let address = path.display().to_string();
                let thread_path = path.clone();
                let coord_for_thread = coordinator.clone();
                let stop_for_thread = stop.clone();
                #[cfg(unix)]
                let thread = std::thread::spawn(move || {
                    accept_loop_unix(&listener, &coord_for_thread, &stop_for_thread, &thread_path);
                });
                Ok(Self {
                    transport: ServerTransport::UnixSocket { path },
                    coordinator,
                    stop,
                    thread: Some(thread),
                    address,
                })
            }
            ControlTransport::LoopbackTcp => {
                let bind = format!("{}:{}", config.tcp_bind_host, config.tcp_bind_port);
                let listener = TcpListener::bind(bind.as_str())?;
                listener.set_nonblocking(true)?;
                let addr = listener.local_addr()?.to_string();
                let coord_for_thread = coordinator.clone();
                let stop_for_thread = stop.clone();
                let thread = std::thread::spawn(move || {
                    accept_loop_tcp(&listener, &coord_for_thread, &stop_for_thread);
                });
                Ok(Self {
                    transport: ServerTransport::LoopbackTcp,
                    coordinator,
                    stop,
                    thread: Some(thread),
                    address: addr,
                })
            }
        }
    }

    /// Synchronously stop the server. The listener thread joins and the
    /// socket file (UDS) is unlinked.
    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
        if let ServerTransport::UnixSocket { path } = &self.transport {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Coordinator handle. Useful for tests that want to drive deploys
    /// directly while the server is running.
    #[must_use]
    pub fn coordinator(&self) -> &Arc<Coordinator> {
        &self.coordinator
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(unix)]
fn accept_loop_unix(
    listener: &UnixListener,
    coordinator: &Arc<Coordinator>,
    stop: &Arc<AtomicBool>,
    socket_path: &Path,
) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _addr)) => {
                let coord = coordinator.clone();
                let _ = stream.set_nonblocking(false);
                std::thread::spawn(move || handle_unix_connection(stream, &coord));
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
    let _ = std::fs::remove_file(socket_path);
}

#[cfg(unix)]
fn handle_unix_connection(stream: UnixStream, coordinator: &Arc<Coordinator>) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => return,
        Ok(_) => {}
    }
    let resp = handle_request(&line, coordinator);
    write_response(&mut writer, &resp);
}

fn accept_loop_tcp(listener: &TcpListener, coordinator: &Arc<Coordinator>, stop: &Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _addr)) => {
                let coord = coordinator.clone();
                let _ = stream.set_nonblocking(false);
                std::thread::spawn(move || handle_tcp_connection(stream, &coord));
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

fn handle_tcp_connection(stream: TcpStream, coordinator: &Arc<Coordinator>) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => return,
        Ok(_) => {}
    }
    let resp = handle_request(&line, coordinator);
    write_response(&mut writer, &resp);
}

fn handle_request(line: &str, coordinator: &Arc<Coordinator>) -> ControlResponse {
    let parsed: Result<ControlRequest, _> = decode_with_version_check(line.trim());
    match parsed {
        Ok(req) => match dispatch(coordinator, req) {
            Ok(resp) => resp,
            Err(err) => {
                ControlResponse::error(None, ResponseError::new(err.code(), err.to_string()))
            }
        },
        Err(DecodeError::UnsupportedSchemaVersion { got, expected }) => ControlResponse::error(
            None,
            ResponseError::new(
                ErrorCode::Unsupported,
                format!("unsupported schema_version `{got}` (expected `{expected}`)"),
            ),
        ),
        Err(err) => ControlResponse {
            status: ResponseStatus::Error,
            error: Some(ResponseError::new(
                ErrorCode::ConfigInvalid,
                err.to_string(),
            )),
            ..ControlResponse::ok(None)
        },
    }
}

fn write_response<W: Write>(writer: &mut W, resp: &ControlResponse) {
    if let Ok(mut buf) = serde_json::to_vec(resp) {
        buf.push(b'\n');
        let _ = writer.write_all(&buf);
        let _ = writer.flush();
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    #![allow(
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_used,
        clippy::default_trait_access
    )]
    use super::Server;
    use crate::config::{AgentConfig, ControlTransport};
    use crate::coordinator::Coordinator;
    use crate::state::StateStore;
    use crate::worker::MockWorkerControl;
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tensorplate_protocol::agent_control::{ControlRequest, ControlResponse};
    use tensorplate_protocol::bundle_manifest::DeviceFamily;
    use tensorplate_protocol::SCHEMA_VERSION;

    fn config(td: &std::path::Path, socket: PathBuf) -> AgentConfig {
        AgentConfig {
            schema_version: SCHEMA_VERSION.to_string(),
            transport: ControlTransport::UnixSocket,
            socket_path: Some(socket),
            tcp_bind_host: "127.0.0.1".into(),
            tcp_bind_port: 0,
            state_dir: td.join("state"),
            staging_dir: td.join("staging"),
            available_backends: vec!["mock".into()],
            backend_capabilities: BTreeMap::new(),
            device_memory_bytes: Some(8 * 1024 * 1024 * 1024),
            device_family: DeviceFamily::Any,
            worker: Default::default(),
            supervision: None,
            runtime_version: Some("0.1.0".into()),
        }
        .validate()
        .expect("valid")
    }

    fn round_trip(socket: &std::path::Path, request: &ControlRequest) -> ControlResponse {
        let mut stream = UnixStream::connect(socket).expect("connect");
        let mut raw = serde_json::to_vec(request).expect("ser");
        raw.push(b'\n');
        stream.write_all(&raw).expect("write");
        stream.flush().expect("flush");
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).expect("read");
        serde_json::from_str(&line).expect("decode response")
    }

    #[test]
    fn health_round_trips() {
        let td = TempDir::new().expect("td");
        let socket = td.path().join("agent.sock");
        let cfg = config(td.path(), socket.clone());
        let store = Arc::new(StateStore::open(cfg.state_dir.clone()).expect("open"));
        let worker = Arc::new(MockWorkerControl::new());
        let coord = Arc::new(Coordinator::new(cfg.clone(), store, worker));
        let mut server = Server::start(&cfg, coord).expect("start");
        let resp = round_trip(&socket, &ControlRequest::health(Some("c".into())));
        assert!(resp.agent_status.is_some());
        server.shutdown();
    }

    #[test]
    fn unknown_schema_version_is_rejected_typed() {
        let td = TempDir::new().expect("td");
        let socket = td.path().join("agent.sock");
        let cfg = config(td.path(), socket.clone());
        let store = Arc::new(StateStore::open(cfg.state_dir.clone()).expect("open"));
        let worker = Arc::new(MockWorkerControl::new());
        let coord = Arc::new(Coordinator::new(cfg.clone(), store, worker));
        let mut server = Server::start(&cfg, coord).expect("start");
        let mut stream = UnixStream::connect(&socket).expect("connect");
        stream
            .write_all(b"{\"schema_version\":\"99.99\",\"op\":\"health\"}\n")
            .expect("write");
        stream.flush().expect("flush");
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).expect("read");
        let resp: ControlResponse = serde_json::from_str(&line).expect("decode");
        assert_eq!(
            resp.status,
            tensorplate_protocol::agent_control::ResponseStatus::Error
        );
        assert_eq!(
            resp.error.as_ref().expect("error").code,
            tensorplate_protocol::ErrorCode::Unsupported
        );
        server.shutdown();
    }
}
