// SPDX-License-Identifier: Apache-2.0
//
// V01-E11-F04-T01: `tensorplate deploy` — bundle submission and
// transaction polling.
//
// The CLI never verifies, stages, warms, promotes, or quarantines the
// bundle locally. The deploy command's only job is to (a) validate the
// local bundle path so the agent sees a readable directory and (b)
// project the agent's transaction phase onto operator-friendly progress
// output. Failures returned by the agent are surfaced *unmodified*
// (error code + message + context) so operators do not have to consult
// two sources of truth.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use tensorplate_protocol::agent_control::{
    is_valid_deployment_id, ControlRequest, ControlResponse, DeployRequest, DeployStatus,
    ResponseStatus,
};
use tensorplate_protocol::deploy_transaction::DeployState;

use crate::args::DeployArgs;
use crate::client::AgentClient;
use crate::error::{CliError, CliResult};
use crate::output::Renderer;
use crate::profile::ResolvedProfile;

/// Default polling cadence while waiting for an in-flight deploy.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Run the `deploy` command.
///
/// # Errors
///
/// Returns:
/// - [`CliError::Usage`] when the bundle path does not exist or is not a directory.
/// - [`CliError::Agent`] / [`CliError::Busy`] / [`CliError::Unavailable`]
///   for typed agent failures.
/// - [`CliError::Timeout`] when `--wait-timeout-ms` expires before the
///   transaction reaches a terminal state.
pub fn run<W: Write, E: Write>(
    renderer: &Renderer,
    profile: &ResolvedProfile,
    client: &dyn AgentClient,
    args: &DeployArgs,
    out: &mut W,
    stderr: &mut E,
) -> CliResult<()> {
    validate_local_bundle(&args.bundle_path)?;
    let correlation = crate::new_correlation_id();
    let deployment_id = args
        .deployment_id
        .clone()
        .unwrap_or_else(|| format!("deploy-{}", uuid::Uuid::new_v4()));
    if !is_valid_deployment_id(&deployment_id) {
        return Err(CliError::Usage(
            "deployment id must be 1 to 128 bytes and contain only ASCII letters, digits, `-`, `_`, or `.`; `.` and `..` are reserved".into(),
        ));
    }
    let labels = args
        .labels
        .iter()
        .cloned()
        .collect::<std::collections::BTreeMap<_, _>>();
    let canonical_bundle = canonicalize(&args.bundle_path)?;
    let deploy_payload = DeployRequest {
        bundle_path: canonical_bundle.display().to_string(),
        deployment_id: deployment_id.clone(),
        expected_bundle_digest: args.expected_digest.clone(),
        labels,
    };
    let request = ControlRequest::deploy(Some(correlation.clone()), deploy_payload);
    renderer.info(
        stderr,
        &format!(
            "deploy: submitting bundle `{}` as `{deployment_id}` to profile `{}`",
            canonical_bundle.display(),
            profile.name,
        ),
    )?;
    let response = client.send_or_map_error(request)?;
    let initial_status = response.deploy_status.clone();
    let transaction_id = response.transaction_id.clone().or_else(|| {
        initial_status
            .as_ref()
            .and_then(|s| s.transaction_id.clone())
    });
    if !args.wait {
        let payload = build_payload(
            &response,
            transaction_id.as_deref(),
            initial_status.as_ref(),
        );
        let human = format!(
            "deploy submitted (transaction_id={}, phase={})",
            transaction_id.as_deref().unwrap_or("<unknown>"),
            initial_status
                .as_ref()
                .map_or("<unknown>", |s| state_label(s.phase)),
        );
        return renderer.ok(
            out,
            "deploy",
            &human,
            payload,
            Some(&correlation),
            transaction_id.as_deref(),
        );
    }
    let final_status = wait_for_terminal(
        client,
        args.wait_timeout_ms,
        renderer,
        stderr,
        &correlation,
        initial_status,
    )?;
    let payload = build_payload(&response, transaction_id.as_deref(), Some(&final_status));
    let human_block = render_human(profile, transaction_id.as_deref(), &final_status);
    match final_status.phase {
        DeployState::Active => renderer.ok(
            out,
            "deploy",
            &human_block,
            payload,
            Some(&correlation),
            transaction_id.as_deref(),
        ),
        DeployState::Failed | DeployState::RolledBack => {
            renderer.ok(
                out,
                "deploy",
                &human_block,
                payload,
                Some(&correlation),
                transaction_id.as_deref(),
            )?;
            Err(failure_error(&final_status))
        }
        other => {
            // We waited and the transaction is somehow non-terminal. This
            // can only happen with a future agent that adds non-terminal
            // states we do not yet model; surface it as a typed error so
            // shell scripts do not assume success.
            Err(CliError::Agent {
                code: tensorplate_protocol::ErrorCode::Unsupported,
                message: format!(
                    "deploy ended in non-terminal phase `{}`",
                    state_label(other)
                ),
                context: Some(format!(
                    "transaction_id={}",
                    transaction_id.unwrap_or_else(|| "<unknown>".into())
                )),
                hint: Some("upgrade the CLI to match the agent's protocol version".into()),
            })
        }
    }
}

pub(crate) fn validate_local_bundle(path: &Path) -> CliResult<()> {
    if !path.exists() {
        return Err(CliError::Usage(format!(
            "deploy: bundle path `{}` does not exist",
            path.display()
        )));
    }
    let meta = std::fs::metadata(path)
        .map_err(|e| CliError::Usage(format!("deploy: cannot stat `{}`: {e}", path.display())))?;
    if !meta.is_dir() {
        return Err(CliError::Usage(format!(
            "deploy: bundle path `{}` is not a directory; bundles are directories rooted at `manifest.json`",
            path.display()
        )));
    }
    let manifest = path.join("manifest.json");
    if !manifest.exists() {
        return Err(CliError::Usage(format!(
            "deploy: bundle `{}` is missing `manifest.json`",
            path.display()
        )));
    }
    Ok(())
}

fn canonicalize(path: &Path) -> CliResult<PathBuf> {
    std::fs::canonicalize(path).map_err(|e| {
        CliError::Usage(format!(
            "deploy: failed to canonicalize `{}`: {e}",
            path.display()
        ))
    })
}

fn wait_for_terminal(
    client: &dyn AgentClient,
    timeout_ms: u64,
    renderer: &Renderer,
    stderr: &mut dyn Write,
    correlation: &str,
    initial: Option<DeployStatus>,
) -> CliResult<DeployStatus> {
    if let Some(s) = initial.as_ref() {
        if s.phase.is_terminal() {
            return Ok(s.clone());
        }
        renderer.info(stderr, &format!("deploy: phase={}", state_label(s.phase)))?;
    }
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut last_phase = initial.as_ref().map(|s| s.phase);
    loop {
        if Instant::now() >= deadline {
            return Err(CliError::Timeout {
                timeout_ms,
                hint: Some(
                    "the agent's deploy transaction may still complete; re-run `tensorplate status` to check".into(),
                ),
            });
        }
        let status_resp = client.send_or_map_error(ControlRequest::status(
            Some(correlation.to_string()),
            Default::default(),
        ))?;
        let in_flight = status_resp
            .agent_status
            .as_ref()
            .and_then(|s| s.in_flight_transaction.clone());
        if let Some(latest) = in_flight {
            if Some(latest.phase) != last_phase {
                renderer.info(
                    stderr,
                    &format!("deploy: phase={}", state_label(latest.phase)),
                )?;
                last_phase = Some(latest.phase);
            }
            if latest.phase.is_terminal() {
                return Ok(latest);
            }
        } else {
            // No in-flight transaction. The deploy either landed Active
            // already or the agent retired the record; consult agent_status.active.
            if let Some(active) = status_resp
                .agent_status
                .as_ref()
                .and_then(|s| s.active.clone())
            {
                renderer.info(
                    stderr,
                    &format!(
                        "deploy: active deployment `{}` (backend={})",
                        active.deployment_id,
                        active.backend_hint.as_deref().unwrap_or("<unknown>")
                    ),
                )?;
                return Ok(DeployStatus {
                    phase: DeployState::Active,
                    transaction_id: None,
                    deployment_id: Some(active.deployment_id),
                    bundle_digest: Some(active.bundle_digest),
                    started_monotonic_ns: None,
                    last_transition_monotonic_ns: active.promoted_monotonic_ns,
                    failure: None,
                });
            }
        }
        sleep(DEFAULT_POLL_INTERVAL);
    }
}

fn failure_error(status: &DeployStatus) -> CliError {
    let (code, message) = status.failure.as_ref().map_or(
        (
            tensorplate_protocol::ErrorCode::Internal,
            format!(
                "deploy ended in `{}` without typed failure metadata",
                state_label(status.phase)
            ),
        ),
        |f| {
            (
                f.error_code,
                f.message
                    .clone()
                    .unwrap_or_else(|| format!("deploy failed in `{}`", state_label(status.phase))),
            )
        },
    );
    CliError::Agent {
        code,
        message,
        context: status
            .transaction_id
            .as_deref()
            .map(|tx| format!("transaction_id={tx}")),
        hint: Some("inspect agent logs for the typed failure detail".into()),
    }
}

fn build_payload(
    response: &ControlResponse,
    transaction_id: Option<&str>,
    final_status: Option<&DeployStatus>,
) -> Value {
    let phase = final_status
        .map(|s| state_label(s.phase).to_string())
        .unwrap_or_else(|| "submitted".to_string());
    let mut payload = json!({
        "agent_response_status": status_label(response.status),
        "transaction_id": transaction_id,
        "phase": phase,
    });
    if let Some(status) = final_status {
        if let Some(d) = status.deployment_id.as_ref() {
            payload["deployment_id"] = json!(d);
        }
        if let Some(d) = status.bundle_digest.as_ref() {
            payload["bundle_digest"] = json!(d);
        }
        if let Some(f) = status.failure.as_ref() {
            payload["failure"] = json!({
                "error_code": f.error_code.as_str(),
                "message": f.message,
                "recoverable": f.recoverable,
            });
        }
    }
    payload
}

fn render_human(
    profile: &ResolvedProfile,
    transaction_id: Option<&str>,
    status: &DeployStatus,
) -> String {
    let phase = state_label(status.phase);
    let mut out = String::new();
    out.push_str(&format!(
        "deploy: profile `{}` transaction_id={} phase={phase}\n",
        profile.name,
        transaction_id.unwrap_or("<unknown>"),
    ));
    if let Some(d) = status.deployment_id.as_deref() {
        out.push_str(&format!("  deployment_id: {d}\n"));
    }
    if let Some(d) = status.bundle_digest.as_deref() {
        out.push_str(&format!("  bundle_digest: {d}\n"));
    }
    if let Some(f) = status.failure.as_ref() {
        out.push_str(&format!(
            "  failure: code={} recoverable={} message={}\n",
            f.error_code.as_str(),
            f.recoverable,
            f.message.as_deref().unwrap_or("(none)"),
        ));
    }
    out
}

fn state_label(state: DeployState) -> &'static str {
    match state {
        DeployState::Received => "received",
        DeployState::Verified => "verified",
        DeployState::Staged => "staged",
        DeployState::CapacityChecked => "capacity_checked",
        DeployState::Prepared => "prepared",
        DeployState::Warmed => "warmed",
        DeployState::Promoted => "promoted",
        DeployState::Active => "active",
        DeployState::Failed => "failed",
        DeployState::RolledBack => "rolled_back",
    }
}

fn status_label(status: ResponseStatus) -> &'static str {
    match status {
        ResponseStatus::Ok => "ok",
        ResponseStatus::Error => "error",
        ResponseStatus::Busy => "busy",
        ResponseStatus::NotFound => "not_found",
        ResponseStatus::Unavailable => "unavailable",
    }
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
    use crate::args::OutputMode;
    use crate::client::MockAgentClient;
    use crate::config::ProfileMode;
    use crate::profile::{ResolvedProfile, Transport};
    use std::time::Duration;
    use tempfile::TempDir;
    use tensorplate_protocol::agent_control::{
        AgentRunState, AgentStatus, ControlResponse, DeployFailureSummary, DeploymentSummary,
    };
    use tensorplate_protocol::deploy_transaction::DeployState;
    use tensorplate_protocol::ErrorCode;

    fn write_bundle(td: &TempDir) -> PathBuf {
        let dir = td.path().join("bundle");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.json"), b"{}").unwrap();
        dir
    }

    fn profile() -> ResolvedProfile {
        ResolvedProfile {
            name: "local".into(),
            mode: ProfileMode::Local,
            display_name: None,
            transport: Transport::UnixSocket {
                path: PathBuf::from("/tmp/agent.sock"),
            },
            serving_url: None,
            timeout: Duration::from_secs(5),
        }
    }

    fn deploy_args(bundle: PathBuf, wait: bool) -> DeployArgs {
        DeployArgs {
            bundle_path: bundle,
            deployment_id: Some("d-1".into()),
            expected_digest: None,
            wait,
            wait_timeout_ms: 5_000,
            labels: vec![],
        }
    }

    fn ok_response_with_phase(phase: DeployState) -> ControlResponse {
        let status = DeployStatus {
            phase,
            transaction_id: Some("tx-1".into()),
            deployment_id: Some("d-1".into()),
            bundle_digest: Some("sha256:abc".into()),
            started_monotonic_ns: Some(1),
            last_transition_monotonic_ns: Some(2),
            failure: None,
        };
        ControlResponse {
            transaction_id: Some("tx-1".into()),
            deploy_status: Some(status),
            ..ControlResponse::ok(Some("corr".into()))
        }
    }

    fn status_response_with_active() -> ControlResponse {
        let active = DeploymentSummary {
            deployment_id: "d-1".into(),
            bundle_digest: "sha256:abc".into(),
            bundle_name: None,
            bundle_version: None,
            backend_hint: Some("mock".into()),
            model_class: None,
            staged_path: None,
            promoted_monotonic_ns: Some(3),
            serving_url: None,
        };
        let agent_status = AgentStatus {
            agent_state: AgentRunState::Ready,
            active: Some(active),
            previous_active: None,
            candidate: None,
            in_flight_transaction: None,
            last_error: None,
            quarantined: vec![],
            recovery: None,
            supervision: None,
        };
        ControlResponse {
            agent_status: Some(agent_status),
            ..ControlResponse::ok(Some("corr".into()))
        }
    }

    fn failed_response_payload() -> ControlResponse {
        let status = DeployStatus {
            phase: DeployState::Failed,
            transaction_id: Some("tx-1".into()),
            deployment_id: Some("d-1".into()),
            bundle_digest: Some("sha256:abc".into()),
            started_monotonic_ns: Some(1),
            last_transition_monotonic_ns: Some(5),
            failure: Some(DeployFailureSummary {
                error_code: ErrorCode::Unsupported,
                message: Some("backend python_pytorch not available".into()),
                recoverable: false,
            }),
        };
        ControlResponse {
            transaction_id: Some("tx-1".into()),
            deploy_status: Some(status),
            ..ControlResponse::ok(Some("corr".into()))
        }
    }

    #[test]
    fn deploy_rejects_missing_bundle_path() {
        let client = MockAgentClient::new();
        let args = deploy_args(PathBuf::from("/does/not/exist"), false);
        let r = Renderer::new(OutputMode::Human);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = run(&r, &profile(), &client, &args, &mut out, &mut err);
        assert!(matches!(result, Err(CliError::Usage(_))));
    }

    #[test]
    fn deploy_rejects_unsafe_deployment_id_before_calling_agent() {
        let td = tempfile::tempdir().unwrap();
        let bundle = write_bundle(&td);
        let mut args = deploy_args(bundle, false);
        args.deployment_id = Some("../state".into());
        let client = MockAgentClient::new();
        let r = Renderer::new(OutputMode::Human);
        let mut out = Vec::new();
        let mut err = Vec::new();

        let result = run(&r, &profile(), &client, &args, &mut out, &mut err);

        assert!(matches!(result, Err(CliError::Usage(_))));
        assert!(client.history().is_empty());
    }

    #[test]
    fn deploy_submits_and_renders_no_wait_flow() {
        let td = tempfile::tempdir().unwrap();
        let bundle = write_bundle(&td);
        let args = deploy_args(bundle, false);
        let client = MockAgentClient::new();
        client.enqueue_ok(ok_response_with_phase(DeployState::Received));
        let r = Renderer::new(OutputMode::Json);
        let mut out = Vec::new();
        let mut err = Vec::new();
        run(&r, &profile(), &client, &args, &mut out, &mut err).unwrap();
        let parsed: Value = serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
        assert_eq!(parsed["payload"]["transaction_id"], "tx-1");
        assert_eq!(parsed["payload"]["phase"], "received");
        // CLI must call the agent with a deploy op.
        let history = client.history();
        assert_eq!(history.len(), 1);
        assert!(history[0].deploy.is_some());
    }

    #[test]
    fn deploy_waits_until_active_and_reports_payload() {
        let td = tempfile::tempdir().unwrap();
        let bundle = write_bundle(&td);
        let args = deploy_args(bundle, true);
        let client = MockAgentClient::new();
        // 1) Initial deploy response: Received
        client.enqueue_ok(ok_response_with_phase(DeployState::Received));
        // 2) Status poll: in-flight Warmed
        let in_flight = DeployStatus {
            phase: DeployState::Warmed,
            transaction_id: Some("tx-1".into()),
            deployment_id: Some("d-1".into()),
            bundle_digest: Some("sha256:abc".into()),
            started_monotonic_ns: Some(1),
            last_transition_monotonic_ns: Some(3),
            failure: None,
        };
        let warm = ControlResponse {
            agent_status: Some(AgentStatus {
                agent_state: AgentRunState::Ready,
                active: None,
                previous_active: None,
                candidate: None,
                in_flight_transaction: Some(in_flight),
                last_error: None,
                quarantined: vec![],
                recovery: None,
                supervision: None,
            }),
            ..ControlResponse::ok(Some("corr".into()))
        };
        client.enqueue_ok(warm);
        // 3) Status poll: in_flight = Active (terminal)
        let mut active_resp = status_response_with_active();
        let active_in_flight = DeployStatus {
            phase: DeployState::Active,
            transaction_id: Some("tx-1".into()),
            deployment_id: Some("d-1".into()),
            bundle_digest: Some("sha256:abc".into()),
            started_monotonic_ns: Some(1),
            last_transition_monotonic_ns: Some(4),
            failure: None,
        };
        if let Some(s) = active_resp.agent_status.as_mut() {
            s.in_flight_transaction = Some(active_in_flight);
        }
        client.enqueue_ok(active_resp);
        let r = Renderer::new(OutputMode::Json);
        let mut out = Vec::new();
        let mut err = Vec::new();
        run(&r, &profile(), &client, &args, &mut out, &mut err).unwrap();
        let parsed: Value = serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
        assert_eq!(parsed["status"], "ok");
        assert_eq!(parsed["payload"]["phase"], "active");
    }

    #[test]
    fn deploy_failure_surfaces_typed_agent_error() {
        let td = tempfile::tempdir().unwrap();
        let bundle = write_bundle(&td);
        let args = deploy_args(bundle, true);
        let client = MockAgentClient::new();
        client.enqueue_ok(failed_response_payload());
        let r = Renderer::new(OutputMode::Json);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = run(&r, &profile(), &client, &args, &mut out, &mut err);
        match result {
            Err(CliError::Agent { code, message, .. }) => {
                assert_eq!(code, ErrorCode::Unsupported);
                assert!(message.contains("python_pytorch"));
            }
            other => panic!("expected Agent error, got {other:?}"),
        }
    }
}
