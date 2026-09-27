// SPDX-License-Identifier: Apache-2.0
//
// V01-E11-F06: `tensorplate infer` — convenience inference workflow.
//
// The CLI is *not* a replacement for the serving worker's HTTP API; it
// is a single-shot client that resolves the right endpoint, posts the
// request body, and surfaces the response. Endpoint resolution order:
//
//   1. `--serving-url <url>` flag (highest precedence)
//   2. The active profile's `serving_url` config field
//   3. Agent-discovered serving endpoint: `active.serving_url` from
//      status. A status that lists a resident set but reports no
//      `serving_url` is refused, never defaulted; only a status without
//      a resident set falls back to the v0.1 loopback default.
//
// The data-plane HTTP call uses a hand-rolled HTTP/1.1 request rather
// than a heavyweight client crate. The protocol is documented in
// `protocol/schemas/serving_http_envelope.json`.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde_json::{json, Value};

use tensorplate_protocol::agent_control::{ControlRequest, ResidentSetStatus};
use tensorplate_protocol::resident_set::MemberState;

use crate::args::InferArgs;
use crate::client::AgentClient;
use crate::error::{CliError, CliResult};
use crate::output::Renderer;
use crate::profile::ResolvedProfile;

/// Convenience inference run.
///
/// # Errors
///
/// Returns:
/// - [`CliError::Usage`] when the input cannot be read.
/// - [`CliError::Unavailable`] when there is no active deployment, or the
///   agent reports a resident set with no single loopback serving URL.
/// - [`CliError::Inference`] when the serving worker returns a typed failure.
/// - [`CliError::Transport`] / [`CliError::Timeout`] for HTTP failures.
pub fn run<W: Write, E: Write>(
    renderer: &Renderer,
    profile: &ResolvedProfile,
    client: &dyn AgentClient,
    args: &InferArgs,
    out: &mut W,
    stderr: &mut E,
) -> CliResult<()> {
    let body = read_input(args)?;
    if body.is_empty() {
        return Err(CliError::Usage(
            "infer: input is empty; provide a non-empty JSON fixture".into(),
        ));
    }
    // Validate input is parseable JSON before going to the network so a
    // malformed payload is rejected with a clear error.
    let _: Value = serde_json::from_slice(&body)
        .map_err(|e| CliError::Usage(format!("infer: input is not valid JSON: {e}")))?;
    let resolution = resolve_serving_endpoint(profile, client, args)?;
    let correlation = crate::new_correlation_id();
    renderer.info(
        stderr,
        &format!(
            "infer: posting to `{}` (correlation_id={correlation})",
            resolution.url
        ),
    )?;
    let timeout = args
        .timeout_ms
        .map_or(profile.timeout, Duration::from_millis);
    let response = post_infer(&resolution, &body, timeout, &correlation)?;
    let parsed: Value = serde_json::from_slice(&response).map_err(|e| CliError::Inference {
        code: tensorplate_protocol::ErrorCode::Internal,
        message: format!("serving worker returned non-JSON body: {e}"),
        hint: Some(
            "verify the serving endpoint is `tensorplate-serving` and not a different HTTP service"
                .into(),
        ),
    })?;
    handle_response(renderer, &resolution, &correlation, &parsed, args, out)
}

fn read_input(args: &InferArgs) -> CliResult<Vec<u8>> {
    if args.from_stdin {
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .map_err(|e| CliError::Io(format!("infer: failed to read stdin: {e}")))?;
        return Ok(buf);
    }
    let Some(path) = args.input_path.as_deref() else {
        return Err(CliError::Usage(
            "infer: --input <path> or --stdin is required".into(),
        ));
    };
    std::fs::read(path)
        .map_err(|e| CliError::Usage(format!("infer: cannot read `{}`: {e}", path.display())))
}

#[derive(Debug)]
struct EndpointResolution {
    url: String,
    host: String,
    port: u16,
    path: String,
    source: &'static str,
}

fn resolve_serving_endpoint(
    profile: &ResolvedProfile,
    client: &dyn AgentClient,
    args: &InferArgs,
) -> CliResult<EndpointResolution> {
    if let Some(url) = args.serving_url.as_deref() {
        return parse_serving_url(url, "flag");
    }
    if let Some(url) = profile.serving_url.as_deref() {
        return parse_serving_url(url, "profile");
    }
    // Discover via agent status.
    let correlation = crate::new_correlation_id();
    let response = client.send_or_map_error(ControlRequest::status(
        Some(correlation),
        Default::default(),
    ))?;
    let status = response.agent_status.as_ref();
    let active = status.and_then(|s| s.active.as_ref());
    if let Some(url) = active.and_then(|a| a.serving_url.as_deref()) {
        return parse_serving_url(url, "agent-discovered");
    }
    // A status that lists a resident set comes from an agent that reports
    // every serving URL it has; without `active.serving_url` it names no
    // single unary route, and the v0.1 default could reach any member's
    // listener or one unrelated to all of them.
    if let Some(set) = status.and_then(|s| s.resident_set.as_ref()) {
        return Err(no_discoverable_endpoint(set));
    }
    if active.is_none() {
        return Err(CliError::Unavailable {
            message: "no active deployment; deploy a bundle before running `infer`".into(),
            hint: Some("run `tensorplate deploy <bundle>` and wait for status=active".into()),
        });
    }
    // A status without a resident set is a singleton agent's, whose serving
    // endpoint is loopback by default. We hard-code the v0.1.0 default; the
    // user can always override via flag or profile config if their device
    // uses a non-default port.
    parse_serving_url("http://127.0.0.1:18080", "agent-discovered")
}

/// The refusal for a resident set that projects no `active.serving_url`,
/// worded for the set's shape.
fn no_discoverable_endpoint(set: &ResidentSetStatus) -> CliError {
    const QUERY_DIRECTLY: &str = "pass `--serving-url` to query a serving worker directly";
    let (message, hint) = match set.members.as_slice() {
        [] => (
            "the agent's resident set has no members, so nothing serves `infer`".to_string(),
            QUERY_DIRECTLY.to_string(),
        ),
        [member] if member.state == MemberState::Quarantined => (
            format!(
                "the resident set's only member `{}` is quarantined and serves nothing",
                member.deployment_id
            ),
            format!("{QUERY_DIRECTLY}; `tensorplate status` shows the member's state"),
        ),
        [member] => (
            match member.unary_endpoint.as_deref() {
                None => format!(
                    "the resident set's only member `{}` has no unary endpoint to discover",
                    member.deployment_id
                ),
                Some(endpoint) => format!(
                    "the agent reports no loopback serving URL for the resident set's only member `{}` (unary endpoint `{endpoint}`)",
                    member.deployment_id
                ),
            },
            "pass `--serving-url` with the unary endpoint of the worker to query".to_string(),
        ),
        members => (
            format!(
                "the agent serves a resident set of {} members and no single active deployment",
                members.len()
            ),
            "pass `--serving-url` with the unary endpoint of the member to query (listed by `tensorplate status`)".to_string(),
        ),
    };
    CliError::Unavailable {
        message,
        hint: Some(hint),
    }
}

fn parse_serving_url(value: &str, source: &'static str) -> CliResult<EndpointResolution> {
    // We deliberately support only http://; v0.1.0 serving is loopback.
    let Some(rest) = value.strip_prefix("http://") else {
        return Err(CliError::Usage(format!(
            "infer: serving url `{value}` must start with `http://` (v0.1.0)"
        )));
    };
    let (authority, path) = rest.split_once('/').map_or((rest, "/infer"), |(a, p)| {
        (a, if p.is_empty() { "/infer" } else { p })
    });
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => {
            let port: u16 = p.parse().map_err(|_| {
                CliError::Usage(format!("infer: serving url `{value}` has non-numeric port"))
            })?;
            (h.to_string(), port)
        }
        None => (authority.to_string(), 80),
    };
    if host.is_empty() {
        return Err(CliError::Usage(format!(
            "infer: serving url `{value}` has empty host"
        )));
    }
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    Ok(EndpointResolution {
        url: format!("http://{host}:{port}{path}"),
        host,
        port,
        path,
        source,
    })
}

fn post_infer(
    endpoint: &EndpointResolution,
    body: &[u8],
    timeout: Duration,
    correlation_id: &str,
) -> CliResult<Vec<u8>> {
    let addr = (endpoint.host.as_str(), endpoint.port)
        .to_socket_addrs()
        .map_err(|e| CliError::Transport {
            message: format!("infer: resolve {}:{}: {e}", endpoint.host, endpoint.port),
            hint: Some(
                "is the serving worker reachable? `tensorplate status` reports its address".into(),
            ),
        })?
        .next()
        .ok_or_else(|| CliError::Transport {
            message: format!(
                "infer: no addresses resolved for {}:{}",
                endpoint.host, endpoint.port
            ),
            hint: None,
        })?;
    let mut stream =
        TcpStream::connect_timeout(&addr, timeout).map_err(|e| CliError::Transport {
            message: format!("infer: connect {addr}: {e}"),
            hint: Some(
                "verify the serving endpoint is up; on remote profiles use an SSH tunnel".into(),
            ),
        })?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| infer_io_error(e, endpoint))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| infer_io_error(e, endpoint))?;
    let request_head = format!(
        "POST {} HTTP/1.1\r\nHost: {}:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nX-Correlation-Id: {}\r\nConnection: close\r\n\r\n",
        endpoint.path,
        endpoint.host,
        endpoint.port,
        body.len(),
        correlation_id,
    );
    stream
        .write_all(request_head.as_bytes())
        .map_err(|e| infer_io_error(e, endpoint))?;
    stream
        .write_all(body)
        .map_err(|e| infer_io_error(e, endpoint))?;
    stream.flush().map_err(|e| infer_io_error(e, endpoint))?;
    let mut response = Vec::with_capacity(4 * 1024);
    stream
        .read_to_end(&mut response)
        .map_err(|e| infer_io_error(e, endpoint))?;
    extract_http_body(&response, endpoint)
}

fn infer_io_error(e: std::io::Error, endpoint: &EndpointResolution) -> CliError {
    if matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ) {
        CliError::Timeout {
            timeout_ms: 0,
            hint: Some(format!(
                "serving endpoint `{}` did not respond before timeout",
                endpoint.url
            )),
        }
    } else {
        CliError::Transport {
            message: format!("infer: {}: {e}", endpoint.url),
            hint: None,
        }
    }
}

fn extract_http_body(response: &[u8], endpoint: &EndpointResolution) -> CliResult<Vec<u8>> {
    // Locate the end of headers (\r\n\r\n) without allocating a String
    // so we tolerate non-UTF-8 bodies (unlikely for our JSON payloads
    // but cheap insurance).
    let separator = b"\r\n\r\n";
    let header_end = response
        .windows(separator.len())
        .position(|w| w == separator)
        .ok_or_else(|| CliError::Transport {
            message: format!(
                "infer: serving worker returned malformed HTTP response from `{}`",
                endpoint.url
            ),
            hint: None,
        })?;
    let head = std::str::from_utf8(&response[..header_end]).map_err(|_| CliError::Transport {
        message: format!(
            "infer: serving worker returned non-utf8 headers from `{}`",
            endpoint.url
        ),
        hint: None,
    })?;
    let mut lines = head.lines();
    let status_line = lines.next().ok_or_else(|| CliError::Transport {
        message: format!("infer: missing status line from `{}`", endpoint.url),
        hint: None,
    })?;
    let mut parts = status_line.splitn(3, ' ');
    let _http = parts.next();
    let code = parts.next().unwrap_or("").parse::<u16>().unwrap_or(0);
    let body_start = header_end + separator.len();
    let body = response.get(body_start..).unwrap_or_default().to_vec();
    if !(200..300).contains(&code) {
        let detail = String::from_utf8_lossy(&body).into_owned();
        return Err(CliError::Inference {
            code: tensorplate_protocol::ErrorCode::InferenceFailed,
            message: format!(
                "serving worker returned HTTP {code} from `{}`: {detail}",
                endpoint.url
            ),
            hint: Some("see the serving worker logs for the request_id and correlation_id".into()),
        });
    }
    Ok(body)
}

fn handle_response<W: Write>(
    renderer: &Renderer,
    endpoint: &EndpointResolution,
    correlation: &str,
    parsed: &Value,
    args: &InferArgs,
    out: &mut W,
) -> CliResult<()> {
    let status = parsed
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if status == "failure" {
        let error = parsed.get("error");
        let (code, message) = error
            .map(|e| {
                let code = e
                    .get("code")
                    .and_then(Value::as_str)
                    .and_then(parse_error_code)
                    .unwrap_or(tensorplate_protocol::ErrorCode::InferenceFailed);
                let message = e
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("serving worker returned an error")
                    .to_string();
                (code, message)
            })
            .unwrap_or((
                tensorplate_protocol::ErrorCode::InferenceFailed,
                "serving worker returned an error without typed detail".to_string(),
            ));
        return Err(CliError::Inference {
            code,
            message,
            hint: Some(
                "inspect the request_id in the response to correlate with serving logs".into(),
            ),
        });
    }
    if let Some(target) = args.output_path.as_deref() {
        std::fs::write(
            target,
            serde_json::to_vec_pretty(parsed).unwrap_or_default(),
        )?;
    }
    let human = render_human(parsed, endpoint);
    let payload = json!({
        "endpoint": endpoint.url,
        "endpoint_source": endpoint.source,
        "result": parsed,
    });
    renderer.ok(out, "infer", &human, payload, Some(correlation), None)
}

fn render_human(parsed: &Value, endpoint: &EndpointResolution) -> String {
    let status = parsed
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let request_id = parsed
        .get("request_id")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let mut out = format!(
        "infer: endpoint={} status={status} request_id={request_id}\n",
        endpoint.url
    );
    if let Some(outputs) = parsed.get("outputs").and_then(Value::as_array) {
        out.push_str(&format!("  outputs: {} tensor(s)\n", outputs.len()));
        for o in outputs.iter().take(8) {
            if let Some(name) = o.get("name").and_then(Value::as_str) {
                let shape = o
                    .get("tensor")
                    .and_then(|t| t.get("shape"))
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                out.push_str(&format!("    - {name} shape={shape}\n"));
            }
        }
    }
    out
}

/// Parse a wire error code through the protocol enum's own serde names, so
/// every code the protocol crate knows is recognised.
fn parse_error_code(code: &str) -> Option<tensorplate_protocol::ErrorCode> {
    serde_json::from_value(Value::String(code.to_owned())).ok()
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

    #[test]
    fn parse_error_code_knows_every_protocol_code() {
        for code in tensorplate_protocol::ErrorCode::ALL {
            assert_eq!(parse_error_code(code.as_str()), Some(code));
        }
        assert_eq!(parse_error_code("canceled"), None);
        assert_eq!(parse_error_code(""), None);
    }

    use super::*;
    use crate::args::OutputMode;
    use crate::client::MockAgentClient;
    use crate::config::ProfileMode;
    use crate::error::ExitCode;
    use crate::profile::{ResolvedProfile, Transport};
    use std::path::PathBuf;
    use std::time::Duration;
    use tensorplate_protocol::agent_control::{
        AgentRunState, AgentStatus, ControlResponse, DeploymentSummary, MemberStatus,
    };
    use tensorplate_protocol::{AdmissionMode, MemberQuota};

    fn profile_with_serving(url: Option<&str>) -> ResolvedProfile {
        ResolvedProfile {
            name: "local".into(),
            mode: ProfileMode::Local,
            display_name: None,
            transport: Transport::UnixSocket {
                path: PathBuf::from("/tmp/agent.sock"),
            },
            serving_url: url.map(str::to_string),
            timeout: Duration::from_secs(5),
        }
    }

    fn args_with_input(path: PathBuf) -> InferArgs {
        InferArgs {
            input_path: Some(path),
            from_stdin: false,
            serving_url: None,
            timeout_ms: None,
            output_path: None,
        }
    }

    fn write_fixture(text: &str) -> (tempfile::TempDir, PathBuf) {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("input.json");
        std::fs::write(&p, text).unwrap();
        (td, p)
    }

    #[test]
    fn rejects_non_json_input() {
        let (_td, p) = write_fixture("not-json");
        let args = args_with_input(p);
        let client = MockAgentClient::new();
        let r = Renderer::new(OutputMode::Human);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = run(
            &r,
            &profile_with_serving(None),
            &client,
            &args,
            &mut out,
            &mut err,
        );
        assert!(matches!(result, Err(CliError::Usage(_))));
    }

    #[test]
    fn unavailable_without_active_deployment() {
        let (_td, p) = write_fixture(r#"{"inputs":[]}"#);
        let args = args_with_input(p);
        let client = MockAgentClient::new();
        // Agent status with no active deployment.
        client.enqueue_ok(ControlResponse {
            agent_status: Some(AgentStatus {
                resident_set: None,
                control_features: Vec::new(),
                agent_state: AgentRunState::Ready,
                active: None,
                previous_active: None,
                candidate: None,
                in_flight_transaction: None,
                last_error: None,
                quarantined: vec![],
                recovery: None,
                supervision: None,
                platform_telemetry: None,
            }),
            ..ControlResponse::ok(Some("c".into()))
        });
        let r = Renderer::new(OutputMode::Human);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = run(
            &r,
            &profile_with_serving(None),
            &client,
            &args,
            &mut out,
            &mut err,
        );
        match result {
            Err(CliError::Unavailable { message, .. }) => {
                assert!(message.contains("no active deployment"))
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn endpoint_resolution_prefers_explicit_flag() {
        let (_td, p) = write_fixture(r#"{"inputs":[]}"#);
        let args = InferArgs {
            input_path: Some(p),
            from_stdin: false,
            serving_url: Some("http://10.0.0.5:30000/infer".into()),
            timeout_ms: None,
            output_path: None,
        };
        let client = MockAgentClient::new();
        let res = resolve_serving_endpoint(&profile_with_serving(None), &client, &args).unwrap();
        assert_eq!(res.source, "flag");
        assert_eq!(res.host, "10.0.0.5");
        assert_eq!(res.port, 30000);
        assert_eq!(res.path, "/infer");
        // Agent should not have been called.
        assert_eq!(client.history().len(), 0);
    }

    #[test]
    fn endpoint_resolution_falls_back_to_profile() {
        let (_td, p) = write_fixture(r#"{"inputs":[]}"#);
        let args = args_with_input(p);
        let client = MockAgentClient::new();
        let prof = profile_with_serving(Some("http://127.0.0.1:18080/infer"));
        let res = resolve_serving_endpoint(&prof, &client, &args).unwrap();
        assert_eq!(res.source, "profile");
        assert_eq!(client.history().len(), 0);
    }

    #[test]
    fn endpoint_resolution_uses_agent_discovered_when_no_overrides() {
        let (_td, p) = write_fixture(r#"{"inputs":[]}"#);
        let args = args_with_input(p);
        let client = MockAgentClient::new();
        client.enqueue_ok(ControlResponse {
            agent_status: Some(AgentStatus {
                resident_set: None,
                control_features: Vec::new(),
                agent_state: AgentRunState::Ready,
                active: Some(DeploymentSummary {
                    deployment_id: "d-1".into(),
                    bundle_digest: "sha256:abc".into(),
                    bundle_name: None,
                    bundle_version: None,
                    backend_hint: Some("mock".into()),
                    model_class: None,
                    staged_path: None,
                    promoted_monotonic_ns: Some(1),
                    serving_url: None,
                }),
                previous_active: None,
                candidate: None,
                in_flight_transaction: None,
                last_error: None,
                quarantined: vec![],
                recovery: None,
                supervision: None,
                platform_telemetry: None,
            }),
            ..ControlResponse::ok(Some("c".into()))
        });
        let res = resolve_serving_endpoint(&profile_with_serving(None), &client, &args).unwrap();
        assert_eq!(res.source, "agent-discovered");
        assert_eq!(res.host, "127.0.0.1");
        assert_eq!(res.port, 18080);
    }

    #[test]
    fn endpoint_resolution_uses_agent_serving_url_when_reported() {
        let (_td, p) = write_fixture(r#"{"inputs":[]}"#);
        let args = args_with_input(p);
        let client = MockAgentClient::new();
        client.enqueue_ok(ControlResponse {
            agent_status: Some(AgentStatus {
                resident_set: None,
                control_features: Vec::new(),
                agent_state: AgentRunState::Ready,
                active: Some(DeploymentSummary {
                    deployment_id: "d-1".into(),
                    bundle_digest: "sha256:abc".into(),
                    bundle_name: None,
                    bundle_version: None,
                    backend_hint: Some("mock".into()),
                    model_class: None,
                    staged_path: None,
                    promoted_monotonic_ns: Some(1),
                    serving_url: Some("http://127.0.0.1:18081/infer".into()),
                }),
                previous_active: None,
                candidate: None,
                in_flight_transaction: None,
                last_error: None,
                quarantined: vec![],
                recovery: None,
                supervision: None,
                platform_telemetry: None,
            }),
            ..ControlResponse::ok(Some("c".into()))
        });
        let res = resolve_serving_endpoint(&profile_with_serving(None), &client, &args).unwrap();
        assert_eq!(res.source, "agent-discovered");
        assert_eq!(res.host, "127.0.0.1");
        assert_eq!(res.port, 18081);
        assert_eq!(res.path, "/infer");
    }

    fn summary(deployment_id: &str, serving_url: Option<&str>) -> DeploymentSummary {
        DeploymentSummary {
            deployment_id: deployment_id.into(),
            bundle_digest: "sha256:abc".into(),
            bundle_name: None,
            bundle_version: None,
            backend_hint: None,
            model_class: None,
            staged_path: None,
            promoted_monotonic_ns: None,
            serving_url: serving_url.map(str::to_string),
        }
    }

    fn member(
        deployment_id: &str,
        state: MemberState,
        unary: Option<&str>,
        stream: Option<&str>,
    ) -> MemberStatus {
        MemberStatus {
            deployment_id: deployment_id.into(),
            generation: 2,
            bundle_digest: "sha256:abc".into(),
            state,
            admission_mode: AdmissionMode::Production,
            quota: MemberQuota::default(),
            unary_endpoint: unary.map(str::to_string),
            stream_endpoint: stream.map(str::to_string),
            stream_api_version: None,
            effective_quota: None,
            staged_bytes: None,
            contact: None,
        }
    }

    /// A client answering status with a resident set of `members` and the
    /// singleton `active` the agent projected beside it.
    fn set_client(
        active: Option<DeploymentSummary>,
        members: Vec<MemberStatus>,
    ) -> MockAgentClient {
        let client = MockAgentClient::new();
        client.enqueue_ok(ControlResponse {
            agent_status: Some(AgentStatus {
                agent_state: AgentRunState::Ready,
                active,
                resident_set: Some(ResidentSetStatus {
                    set_id: "set-1".into(),
                    revision: 2,
                    members,
                }),
                ..AgentStatus::default()
            }),
            ..ControlResponse::ok(Some("c".into()))
        });
        client
    }

    /// Resolve against `client`, expecting the typed refusal; returns its
    /// message and hint.
    fn refused(client: &MockAgentClient) -> (String, String) {
        let (_td, p) = write_fixture(r#"{"inputs":[]}"#);
        let Err(err) =
            resolve_serving_endpoint(&profile_with_serving(None), client, &args_with_input(p))
        else {
            panic!("resolved, not refused");
        };
        assert_eq!(err.exit_code(), ExitCode::Unavailable, "{err:?}");
        assert_eq!(client.history().len(), 1, "one status query");
        match err {
            CliError::Unavailable { message, hint } => (message, hint.expect("hint")),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    // The tests above without a resident set are the legacy controls: the
    // default applies there and only there.

    #[test]
    fn a_stream_only_set_of_one_does_not_fall_back_to_the_default() {
        // The agent projects `active` for the serving member, but no
        // `serving_url`: its only endpoint is a stream endpoint.
        let client = set_client(
            Some(summary("vision-v2", None)),
            vec![member(
                "vision-v2",
                MemberState::Serving,
                None,
                Some("127.0.0.1:18105"),
            )],
        );
        let (message, hint) = refused(&client);
        assert!(
            message.contains("`vision-v2` has no unary endpoint"),
            "{message}"
        );
        assert!(hint.contains("--serving-url"), "{hint}");
    }

    #[test]
    fn a_set_of_one_off_loopback_does_not_fall_back_to_the_default() {
        let client = set_client(
            Some(summary("vision-v2", None)),
            vec![member(
                "vision-v2",
                MemberState::Serving,
                Some("http://192.0.2.10:18090"),
                None,
            )],
        );
        let (message, hint) = refused(&client);
        assert!(message.contains("no loopback serving URL"), "{message}");
        assert!(message.contains("`http://192.0.2.10:18090`"), "{message}");
        assert!(hint.contains("--serving-url"), "{hint}");
    }

    #[test]
    fn a_multi_member_set_is_refused_even_beside_an_active() {
        // Not a shape this agent projects; the refusal keys on the set,
        // not on `active` being absent.
        let client = set_client(
            Some(summary("speech-stt", None)),
            vec![
                member(
                    "speech-stt",
                    MemberState::Serving,
                    None,
                    Some("127.0.0.1:18103"),
                ),
                member(
                    "speech-tts",
                    MemberState::Serving,
                    None,
                    Some("127.0.0.1:18105"),
                ),
            ],
        );
        let (message, hint) = refused(&client);
        assert!(message.contains("resident set of 2 members"), "{message}");
        assert!(hint.contains("--serving-url"), "{hint}");
    }

    #[test]
    fn a_quarantined_set_of_one_names_the_quarantine() {
        let client = set_client(
            None,
            vec![member("vision-v2", MemberState::Quarantined, None, None)],
        );
        let (message, hint) = refused(&client);
        assert!(message.contains("`vision-v2` is quarantined"), "{message}");
        assert!(!message.contains("1 members"), "{message}");
        assert!(
            hint.contains("`tensorplate status` shows the member's state"),
            "{hint}"
        );
        assert!(!hint.contains("unary endpoint"), "{hint}");
    }

    #[test]
    fn an_empty_resident_set_names_no_members() {
        let client = set_client(None, Vec::new());
        let (message, hint) = refused(&client);
        assert!(message.contains("has no members"), "{message}");
        assert!(!hint.contains("unary endpoint"), "{hint}");
    }

    #[test]
    fn a_set_of_one_uses_its_projected_serving_url() {
        // Control: a set of one serving member with a loopback unary
        // endpoint resolves like a singleton.
        let client = set_client(
            Some(summary("vision-v2", Some("http://127.0.0.1:18090/infer"))),
            vec![member(
                "vision-v2",
                MemberState::Serving,
                Some("http://127.0.0.1:18090"),
                None,
            )],
        );
        let (_td, p) = write_fixture(r#"{"inputs":[]}"#);
        let res =
            resolve_serving_endpoint(&profile_with_serving(None), &client, &args_with_input(p))
                .unwrap();
        assert_eq!(res.source, "agent-discovered");
        assert_eq!(res.port, 18090);
        assert_eq!(res.path, "/infer");
    }

    #[test]
    fn the_flag_and_the_profile_still_win_over_a_resident_set() {
        // Control: overrides never ask the agent, so a set cannot refuse
        // them.
        let (_td, p) = write_fixture(r#"{"inputs":[]}"#);
        let client = set_client(None, Vec::new());
        let args = InferArgs {
            serving_url: Some("http://127.0.0.1:18091".into()),
            ..args_with_input(p.clone())
        };
        let res = resolve_serving_endpoint(&profile_with_serving(None), &client, &args).unwrap();
        assert_eq!(res.source, "flag");
        let prof = profile_with_serving(Some("http://127.0.0.1:18092/infer"));
        let res = resolve_serving_endpoint(&prof, &client, &args_with_input(p)).unwrap();
        assert_eq!(res.source, "profile");
        assert_eq!(client.history().len(), 0);
    }
}
