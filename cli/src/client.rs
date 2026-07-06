// SPDX-License-Identifier: Apache-2.0
//
// V01-E11-F02-T02: agent control API client used by every mutating command.
//
// The transport is newline-delimited JSON, one request per connection,
// exactly matching `agent/src/server.rs`. The CLI client is intentionally
// boring: it does not pool connections, does not retry, and does not
// re-encode payloads. Timeouts are bounded; transport failures map to
// typed [`CliError`] values that the renderer turns into actionable
// operator messages.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use tensorplate_protocol::agent_control::{ControlRequest, ControlResponse, ResponseStatus};
use tensorplate_protocol::{decode_with_version_check, DecodeError};

use crate::error::{CliError, CliResult};
use crate::profile::{ResolvedProfile, Transport};

/// Abstract control-API client. Production code uses
/// [`NetAgentClient`]; tests inject [`MockAgentClient`].
pub trait AgentClient {
    /// Send `request` and return the decoded response.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Transport`], [`CliError::Timeout`], or
    /// [`CliError::Serialization`] when the transport fails. Typed
    /// agent errors are still returned as `Ok(response)` so the caller
    /// can inspect `response.status` and `response.error` directly.
    fn send(&self, request: ControlRequest) -> CliResult<ControlResponse>;

    /// Convenience helper: send and translate non-OK responses into
    /// typed [`CliError`] variants. Most subcommands prefer this; only
    /// the deploy poller and the doctor agent-probe need the raw form.
    ///
    /// # Errors
    ///
    /// Returns a [`CliError`] variant matching `response.status`.
    fn send_or_map_error(&self, request: ControlRequest) -> CliResult<ControlResponse> {
        let response = self.send(request)?;
        match response.status {
            ResponseStatus::Ok => Ok(response),
            ResponseStatus::Busy => Err(CliError::Busy {
                hint: Some("retry once the in-flight deploy or rollback completes".into()),
            }),
            ResponseStatus::Unavailable => {
                let (msg, code, context) = response_error_parts(&response);
                Err(CliError::Unavailable {
                    message: msg,
                    hint: hint_for(code, context.as_deref()),
                })
            }
            ResponseStatus::NotFound => {
                let (msg, _, _) = response_error_parts(&response);
                Err(CliError::Agent {
                    code: tensorplate_protocol::ErrorCode::ConfigInvalid,
                    message: msg,
                    context: None,
                    hint: Some("verify the deployment id or correlation id".into()),
                })
            }
            ResponseStatus::Error => {
                let (msg, code, context) = response_error_parts(&response);
                Err(CliError::Agent {
                    code,
                    message: msg,
                    context,
                    hint: hint_for(code, None),
                })
            }
        }
    }
}

fn response_error_parts(
    response: &ControlResponse,
) -> (String, tensorplate_protocol::ErrorCode, Option<String>) {
    if let Some(err) = response.error.as_ref() {
        (err.message.clone(), err.code, err.context.clone())
    } else {
        (
            "agent returned non-OK status without typed error".into(),
            tensorplate_protocol::ErrorCode::Internal,
            None,
        )
    }
}

fn hint_for(code: tensorplate_protocol::ErrorCode, context: Option<&str>) -> Option<String> {
    use tensorplate_protocol::ErrorCode as E;
    let base = match code {
        E::ConfigInvalid => Some("check the bundle manifest and agent config"),
        E::LoadFailed => Some("inspect agent logs and re-stage the bundle"),
        E::NotReady => Some("wait for the in-flight transaction to settle"),
        E::ShapeMismatch => Some("re-check the model contract against the request"),
        E::Unsupported => Some("the backend or capability is not enabled on this device"),
        E::OomError => Some("free device memory or reduce the bundle's memory estimate"),
        E::Timeout => Some("re-run with `--timeout-ms <ms>` or inspect agent logs"),
        E::InferenceFailed => Some("inspect the serving worker logs for backend-specific detail"),
        E::Internal => Some("file a bug with the correlation id from the response"),
    };
    base.map(|hint| match context {
        Some(ctx) if !ctx.is_empty() => format!("{hint} (context: {ctx})"),
        _ => hint.to_string(),
    })
}

/// Production agent client driven by the resolved profile.
pub struct NetAgentClient {
    transport: Transport,
    timeout: Duration,
}

impl NetAgentClient {
    #[must_use]
    pub fn new(profile: &ResolvedProfile) -> Self {
        Self {
            transport: profile.transport.clone(),
            timeout: profile.timeout,
        }
    }
}

impl AgentClient for NetAgentClient {
    fn send(&self, request: ControlRequest) -> CliResult<ControlResponse> {
        let payload = serde_json::to_vec(&request)?;
        match &self.transport {
            Transport::UnixSocket { path } => send_unix(path, self.timeout, &payload),
            Transport::LoopbackTcp { host, port } => send_tcp(host, *port, self.timeout, &payload),
        }
    }
}

#[cfg(unix)]
fn send_unix(path: &PathBuf, timeout: Duration, payload: &[u8]) -> CliResult<ControlResponse> {
    let stream = UnixStream::connect(path).map_err(|e| CliError::Transport {
        message: format!("connect {}: {e}", path.display()),
        hint: Some("verify `tensorplate-agent` is running and the socket path is correct".into()),
    })?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| transport_io(path.display().to_string(), e))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| transport_io(path.display().to_string(), e))?;
    write_request(&stream, payload, &path.display().to_string())?;
    read_response(stream, &path.display().to_string(), timeout)
}

#[cfg(not(unix))]
fn send_unix(_path: &PathBuf, _timeout: Duration, _payload: &[u8]) -> CliResult<ControlResponse> {
    Err(CliError::Transport {
        message: "unix domain sockets are not available on this platform".into(),
        hint: Some("use a profile with `mode=url` instead".into()),
    })
}

fn send_tcp(
    host: &str,
    port: u16,
    timeout: Duration,
    payload: &[u8],
) -> CliResult<ControlResponse> {
    // Resolve and pick the first reachable address. Connect with a bounded
    // timeout so a wedged peer cannot hang the CLI.
    let addrs: Vec<_> = (host, port)
        .to_socket_addrs()
        .map_err(|e| CliError::Transport {
            message: format!("resolve {host}:{port}: {e}"),
            hint: Some(
                "verify the host:port is reachable; remote profiles require an SSH tunnel or VPN"
                    .into(),
            ),
        })?
        .collect();
    let mut last_err = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => {
                stream
                    .set_read_timeout(Some(timeout))
                    .map_err(|e| transport_io(format!("{host}:{port}"), e))?;
                stream
                    .set_write_timeout(Some(timeout))
                    .map_err(|e| transport_io(format!("{host}:{port}"), e))?;
                write_request(&stream, payload, &format!("{host}:{port}"))?;
                return read_response(stream, &format!("{host}:{port}"), timeout);
            }
            Err(e) => last_err = Some(e),
        }
    }
    let message = last_err
        .map(|e| format!("connect {host}:{port}: {e}"))
        .unwrap_or_else(|| format!("no addresses resolved for {host}:{port}"));
    Err(CliError::Transport {
        message,
        hint: Some(
            "remote agent profiles require a reachable loopback endpoint via SSH/VPN".into(),
        ),
    })
}

fn write_request<W: Write>(mut writer: W, payload: &[u8], peer: &str) -> CliResult<()> {
    writer.write_all(payload).map_err(|e| CliError::Transport {
        message: format!("write {peer}: {e}"),
        hint: None,
    })?;
    // Newline framing matches the agent server's `read_line`.
    writer.write_all(b"\n").map_err(|e| CliError::Transport {
        message: format!("write {peer}: {e}"),
        hint: None,
    })?;
    writer.flush().map_err(|e| CliError::Transport {
        message: format!("flush {peer}: {e}"),
        hint: None,
    })?;
    Ok(())
}

fn read_response<R: Read + ReadShutdown>(
    stream: R,
    peer: &str,
    timeout: Duration,
) -> CliResult<ControlResponse> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let bytes = reader.read_line(&mut line).map_err(|e| {
        if matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ) {
            CliError::Timeout {
                timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                hint: Some(
                    "agent did not respond within the configured timeout; retry with --timeout-ms"
                        .into(),
                ),
            }
        } else {
            CliError::Transport {
                message: format!("read {peer}: {e}"),
                hint: None,
            }
        }
    })?;
    if bytes == 0 {
        return Err(CliError::Transport {
            message: format!("read {peer}: connection closed before response"),
            hint: Some(
                "agent may be unable to authorize this client or is shutting down; check agent logs"
                    .into(),
            ),
        });
    }
    // Route the decode through the shared version check so an incompatible
    // agent is rejected with a typed error instead of a generic parse failure.
    // Fail closed: any mismatch or decode failure is a transport error.
    let response: ControlResponse =
        decode_with_version_check(line.trim_end()).map_err(|e| match e {
            DecodeError::UnsupportedSchemaVersion { got, expected } => CliError::Transport {
                message: format!(
                    "incompatible agent protocol version: got {got}, CLI expects {expected}"
                ),
                hint: Some(
                    "upgrade the device's tensorplate agent or this CLI so both speak the same protocol version"
                        .into(),
                ),
            },
            other => CliError::Transport {
                message: format!("decode {peer}: {other}"),
                hint: Some(
                    "agent returned a payload the CLI could not parse; protocol version mismatch?"
                        .into(),
                ),
            },
        })?;
    Ok(response)
}

fn transport_io(peer: String, e: std::io::Error) -> CliError {
    CliError::Transport {
        message: format!("{peer}: {e}"),
        hint: None,
    }
}

/// Marker trait used so `read_response` can accept either a `TcpStream`
/// or a `UnixStream` without dragging in conditional compilation noise
/// at every call site. The default impl is empty; the trait exists so
/// the generic bound rejects unrelated readers.
trait ReadShutdown {}

impl ReadShutdown for TcpStream {}
#[cfg(unix)]
impl ReadShutdown for UnixStream {}

/// Deterministic mock client used by unit and integration tests.
pub struct MockAgentClient {
    inner: RefCell<MockInner>,
}

struct MockInner {
    queue: VecDeque<MockResponse>,
    history: Vec<ControlRequest>,
}

enum MockResponse {
    Ok(ControlResponse),
    Err(CliError),
}

impl MockAgentClient {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: RefCell::new(MockInner {
                queue: VecDeque::new(),
                history: Vec::new(),
            }),
        }
    }

    /// Enqueue a normal control response to be returned on the next call.
    pub fn enqueue_ok(&self, response: ControlResponse) {
        self.inner
            .borrow_mut()
            .queue
            .push_back(MockResponse::Ok(response));
    }

    /// Enqueue a transport-level failure for the next call.
    pub fn enqueue_err(&self, error: CliError) {
        self.inner
            .borrow_mut()
            .queue
            .push_back(MockResponse::Err(error));
    }

    /// Record of every request the test issued. Useful for asserting on
    /// the exact shape of the mutating call the CLI made.
    #[must_use]
    pub fn history(&self) -> Vec<ControlRequest> {
        self.inner.borrow().history.clone()
    }

    /// Pending queue length, used by tests that drain after the run.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.inner.borrow().queue.len()
    }
}

impl Default for MockAgentClient {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentClient for MockAgentClient {
    fn send(&self, request: ControlRequest) -> CliResult<ControlResponse> {
        let mut inner = self.inner.borrow_mut();
        inner.history.push(request);
        match inner.queue.pop_front() {
            Some(MockResponse::Ok(response)) => Ok(response),
            Some(MockResponse::Err(err)) => Err(err),
            None => Err(CliError::Internal(
                "MockAgentClient: response queue exhausted".into(),
            )),
        }
    }
}

/// Trait covering convenience inference calls. Implementations live in
/// [`crate::commands::infer`] so the data-plane HTTP client is local to
/// that command rather than a shared abstraction; the trait exists for
/// test injection only.
pub trait ServingClient {
    /// Send a single inference request body to the resolved serving URL.
    /// Returns the raw response body on success.
    ///
    /// # Errors
    ///
    /// Returns a typed [`CliError`] for transport, timeout, and HTTP
    /// status failures.
    fn infer(&self, endpoint: &str, body: &[u8]) -> CliResult<Vec<u8>>;
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::default_trait_access,
        clippy::needless_pass_by_value,
        clippy::semicolon_if_nothing_returned,
        clippy::field_reassign_with_default,
        clippy::large_enum_variant,
        clippy::no_effect_underscore_binding,
        clippy::redundant_clone,
        clippy::redundant_closure_for_method_calls
    )]

    use super::*;
    use tensorplate_protocol::agent_control::{ControlOp, ResponseError};
    use tensorplate_protocol::ErrorCode;

    fn ok_response(correlation: Option<&str>) -> ControlResponse {
        ControlResponse::ok(correlation.map(str::to_string))
    }

    #[test]
    fn mock_records_history_and_returns_responses() {
        let mock = MockAgentClient::new();
        mock.enqueue_ok(ok_response(Some("corr-1")));
        let req = ControlRequest::health(Some("corr-1".into()));
        let resp = mock.send(req.clone()).unwrap();
        assert_eq!(resp.correlation_id.as_deref(), Some("corr-1"));
        let history = mock.history();
        assert_eq!(history.len(), 1);
        assert!(matches!(history[0].op, ControlOp::Health));
        assert_eq!(mock.pending(), 0);
    }

    #[test]
    fn send_or_map_error_translates_busy() {
        let mock = MockAgentClient::new();
        mock.enqueue_ok(ControlResponse::busy(Some("c".into()), "agent_busy"));
        let err = mock
            .send_or_map_error(ControlRequest::status(None, Default::default()))
            .unwrap_err();
        assert!(matches!(err, CliError::Busy { .. }));
    }

    #[test]
    fn send_or_map_error_translates_unavailable() {
        let mock = MockAgentClient::new();
        mock.enqueue_ok(ControlResponse::unavailable(
            Some("c".into()),
            "no previous active",
        ));
        let err = mock
            .send_or_map_error(ControlRequest::rollback(None, Default::default()))
            .unwrap_err();
        match err {
            CliError::Unavailable { message, .. } => {
                assert!(message.contains("no previous active"))
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn send_or_map_error_preserves_typed_agent_error_code() {
        let mock = MockAgentClient::new();
        mock.enqueue_ok(ControlResponse::error(
            Some("c".into()),
            ResponseError::new(
                ErrorCode::Unsupported,
                "bundle declares unsupported backend",
            ),
        ));
        let err = mock
            .send_or_map_error(ControlRequest::status(None, Default::default()))
            .unwrap_err();
        match err {
            CliError::Agent { code, message, .. } => {
                assert_eq!(code, ErrorCode::Unsupported);
                assert!(message.contains("unsupported backend"));
            }
            other => panic!("expected Agent, got {other:?}"),
        }
    }
}
