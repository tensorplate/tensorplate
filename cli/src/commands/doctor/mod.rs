// SPDX-License-Identifier: Apache-2.0
//
// V01-E11-F03: `tensorplate doctor` — operator validation command.
//
// `doctor` is a read-only command: it observes the device, the agent
// status, and the optional observability snapshot, and reports findings.
// It does not mutate desired state, restart workers, or download
// dependencies. The taxonomy lives in [`finding`]; each probe returns
// one or more findings and the runner aggregates them deterministically.

use std::io::Write;
use std::path::PathBuf;

use serde_json::json;

use tensorplate_protocol::agent_control::{
    AgentRunState, AgentStatus, ControlRequest, ResponseStatus, SupervisionStatusSummary,
};
use tensorplate_protocol::supervision_event::SupervisionServingState;

use crate::args::DoctorArgs;
use crate::client::AgentClient;
use crate::config::ProfileMode;
use crate::error::{CliError, CliResult};
use crate::output::Renderer;
use crate::profile::ResolvedProfile;

pub mod finding;
pub mod install;

use finding::{Finding, FindingId, FindingStatus, Severity};

/// Run the `doctor` command.
///
/// # Errors
///
/// Returns [`CliError::DoctorFindings`] when at least one probe returns
/// a `fail` severity. Probe-level errors (e.g. agent unreachable) become
/// findings rather than CLI errors so the operator gets a complete view.
pub fn run<W: Write, E: Write>(
    renderer: &Renderer,
    profile: &ResolvedProfile,
    client: &dyn AgentClient,
    args: &DoctorArgs,
    out: &mut W,
    _stderr: &mut E,
) -> CliResult<()> {
    let mut findings = Vec::<Finding>::new();
    findings.push(probe_cli_version());
    findings.extend(probe_profile_compatibility(profile));
    findings.extend(probe_runtime_environment());
    findings.extend(probe_ros2_health_stub());
    // V01-E14-F06 install probes: filesystem layout, configs,
    // systemd units, serving binary, backend descriptor + runtime,
    // CUDA/TensorRT/LibTorch.
    findings.extend(install::run(&install::InstallProbeOptions::default()));
    if !args.skip_agent {
        findings.extend(probe_agent(client, profile));
    } else {
        findings.push(Finding::skipped(
            FindingId::AgentReachable,
            Severity::Info,
            "agent probe skipped via --skip-agent",
            None,
        ));
    }
    let (total, failing) = summarize(&findings);
    let payload = json!({
        "profile": profile.name,
        "mode": profile.mode.as_str(),
        "total": total,
        "failing": failing,
        "findings": findings,
    });
    let human = render_human(profile, &findings);
    renderer.ok(out, "doctor", &human, payload, None, None)?;
    if failing > 0 {
        return Err(CliError::DoctorFindings {
            failing,
            total: u32::try_from(findings.len()).unwrap_or(u32::MAX),
        });
    }
    Ok(())
}

fn summarize(findings: &[Finding]) -> (u32, u32) {
    let total = u32::try_from(findings.len()).unwrap_or(u32::MAX);
    let failing = findings
        .iter()
        .filter(|f| matches!(f.status, FindingStatus::Fail))
        .count();
    (total, u32::try_from(failing).unwrap_or(u32::MAX))
}

fn render_human(profile: &ResolvedProfile, findings: &[Finding]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "tensorplate doctor — profile `{}` ({})\n",
        profile.name,
        profile.mode.as_str()
    ));
    out.push_str(&format!("{:=<60}\n", ""));
    for f in findings {
        out.push_str(&format!(
            "[{:<11}] {:<10} {} — {}",
            f.status_label(),
            f.severity_label(),
            f.id_label(),
            f.message,
        ));
        out.push('\n');
        if let Some(hint) = f.hint.as_deref() {
            out.push_str(&format!("              hint: {hint}\n"));
        }
    }
    let (total, failing) = summarize(findings);
    out.push_str(&format!("\n{total} checks — {failing} failing\n"));
    out
}

fn probe_cli_version() -> Finding {
    Finding::ok(
        FindingId::CliVersion,
        Severity::Info,
        format!(
            "tensorplate-cli {} (protocol {})",
            crate::version(),
            tensorplate_protocol::PROTOCOL_VERSION
        ),
        None,
    )
}

fn probe_profile_compatibility(profile: &ResolvedProfile) -> Vec<Finding> {
    let mut out = Vec::new();
    if !profile.mode.is_supported() {
        out.push(Finding::fail(
            FindingId::ProfileMode,
            Severity::Critical,
            format!(
                "profile `{}` uses reserved mode `{}` which is not implemented in v0.1.0",
                profile.name,
                profile.mode.as_str(),
            ),
            Some("use `mode: local` or `mode: url` until the platform release lands".into()),
        ));
        return out;
    }
    out.push(Finding::ok(
        FindingId::ProfileMode,
        Severity::Info,
        format!(
            "profile `{}` mode `{}` is supported",
            profile.name,
            profile.mode.as_str(),
        ),
        None,
    ));
    if matches!(profile.mode, ProfileMode::Local) {
        if let crate::profile::Transport::UnixSocket { path } = &profile.transport {
            out.push(probe_unix_socket(path));
        }
    }
    out
}

fn probe_unix_socket(path: &PathBuf) -> Finding {
    if path.exists() {
        Finding::ok(
            FindingId::AgentSocket,
            Severity::Info,
            format!("agent socket `{}` exists", path.display()),
            None,
        )
    } else {
        Finding::warn(
            FindingId::AgentSocket,
            Severity::Warning,
            format!("agent socket `{}` does not exist", path.display()),
            Some(
                "is `tensorplate-agent` running? `systemctl status tensorplate-agent` should report active"
                    .into(),
            ),
        )
    }
}

fn probe_runtime_environment() -> Vec<Finding> {
    // Host facts only. Concrete CUDA / TensorRT / LibTorch / Python /
    // PyTorch checks land in [`install::run`] (V01-E14-F06) so the
    // CLI / V01-E15 harness reads them from a single source.
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    let mut findings = vec![Finding::ok(
        FindingId::HostFacts,
        Severity::Info,
        format!("host arch={arch}, os={os}"),
        None,
    )];
    if !matches!(os, "linux") {
        findings.push(Finding::unsupported(
            FindingId::HostOs,
            Severity::Info,
            format!("v0.1.0 validation targets Linux; running on {os}"),
            Some(
                "development host checks pass; deploy to a Jetson Orin device for full validation"
                    .into(),
            ),
        ));
    } else {
        findings.push(Finding::ok(
            FindingId::HostOs,
            Severity::Info,
            "running on Linux",
            None,
        ));
    }
    findings
}

fn probe_ros2_health_stub() -> Vec<Finding> {
    // V01-E10 ships an *optional* ROS 2 health-topic publisher stub. The
    // CLI cannot observe that directly — it asks the observability
    // snapshot when the status command runs. Here we just record that
    // the check is deferred and stable.
    vec![Finding::skipped(
        FindingId::Ros2HealthStub,
        Severity::Info,
        "ROS 2 health stub status is surfaced through `tensorplate status` against the observability snapshot",
        None,
    )]
}

fn probe_agent(client: &dyn AgentClient, profile: &ResolvedProfile) -> Vec<Finding> {
    let mut out = Vec::new();
    let correlation = crate::new_correlation_id();
    let version_request = ControlRequest::version(Some(correlation.clone()));
    match client.send(version_request) {
        Ok(response) if matches!(response.status, ResponseStatus::Ok) => {
            out.push(Finding::ok(
                FindingId::AgentReachable,
                Severity::Info,
                format!(
                    "agent reachable via `{}` (correlation_id={})",
                    profile_target(profile),
                    correlation,
                ),
                None,
            ));
        }
        Ok(response) => {
            let (msg, _, _) = response_error(&response);
            out.push(Finding::fail(
                FindingId::AgentReachable,
                Severity::Critical,
                format!("agent rejected version request: {msg}"),
                Some("check protocol version compatibility".into()),
            ));
            return out;
        }
        Err(err) => {
            out.push(Finding::fail(
                FindingId::AgentReachable,
                Severity::Critical,
                format!("agent unreachable: {err}"),
                err.hint().map(str::to_string).or_else(|| {
                    Some(
                        "verify `tensorplate-agent` is running and the socket/URL is correct"
                            .into(),
                    )
                }),
            ));
            return out;
        }
    }
    let status_correlation = crate::new_correlation_id();
    let status_req = ControlRequest::status(Some(status_correlation.clone()), Default::default());
    match client.send(status_req) {
        Ok(response) if matches!(response.status, ResponseStatus::Ok) => {
            if let Some(status) = response.agent_status.as_ref() {
                out.extend(findings_from_status(status));
            } else {
                out.push(Finding::warn(
                    FindingId::AgentStatusShape,
                    Severity::Warning,
                    "agent returned ok status response but omitted agent_status",
                    Some("agent may be running an older protocol; bump the agent install".into()),
                ));
            }
        }
        Ok(response) => {
            let (msg, _, _) = response_error(&response);
            out.push(Finding::fail(
                FindingId::AgentStatusShape,
                Severity::Warning,
                format!("agent status request failed: {msg}"),
                None,
            ));
        }
        Err(err) => {
            out.push(Finding::warn(
                FindingId::AgentStatusShape,
                Severity::Warning,
                format!("agent status probe failed: {err}"),
                None,
            ));
        }
    }
    out
}

fn findings_from_status(status: &AgentStatus) -> Vec<Finding> {
    let mut out = Vec::new();
    let agent_state_msg = match status.agent_state {
        AgentRunState::Ready => "agent_state=ready".to_string(),
        AgentRunState::Degraded => "agent_state=degraded".to_string(),
        AgentRunState::Failed => "agent_state=failed".to_string(),
        AgentRunState::Unknown => "agent_state=unknown".to_string(),
    };
    let finding = match status.agent_state {
        AgentRunState::Ready => {
            Finding::ok(FindingId::AgentState, Severity::Info, agent_state_msg, None)
        }
        AgentRunState::Degraded => Finding::warn(
            FindingId::AgentState,
            Severity::Warning,
            agent_state_msg,
            Some("check `tensorplate status` for in-flight or quarantined deployments".into()),
        ),
        AgentRunState::Failed => Finding::fail(
            FindingId::AgentState,
            Severity::Critical,
            agent_state_msg,
            Some("inspect agent logs and re-run `tensorplate status` after recovery".into()),
        ),
        AgentRunState::Unknown => Finding::warn(
            FindingId::AgentState,
            Severity::Warning,
            agent_state_msg,
            Some("agent has not yet reached a steady state; retry doctor in a few seconds".into()),
        ),
    };
    out.push(finding);
    if let Some(active) = status.active.as_ref() {
        out.push(Finding::ok(
            FindingId::ActiveDeployment,
            Severity::Info,
            format!(
                "active deployment `{}` (backend={})",
                active.deployment_id,
                active.backend_hint.as_deref().unwrap_or("<unknown>"),
            ),
            None,
        ));
    } else {
        out.push(Finding::missing(
            FindingId::ActiveDeployment,
            Severity::Info,
            "no active deployment",
            Some("run `tensorplate deploy <bundle>` to install one".into()),
        ));
    }
    if let Some(sup) = status.supervision.as_ref() {
        out.push(supervision_finding(sup));
    }
    out
}

fn supervision_finding(sup: &SupervisionStatusSummary) -> Finding {
    if sup.crash_loop {
        return Finding::fail(
            FindingId::WorkerCrashLoop,
            Severity::Critical,
            format!(
                "serving worker is crash-looping ({}/{} restarts, state={:?})",
                sup.restart_count, sup.crash_loop_threshold, sup.serving_state
            ),
            Some("inspect agent supervision logs and the bundle being warmed".into()),
        );
    }
    match sup.serving_state {
        SupervisionServingState::Ready => Finding::ok(
            FindingId::WorkerState,
            Severity::Info,
            "serving worker ready",
            None,
        ),
        SupervisionServingState::Starting => Finding::ok(
            FindingId::WorkerState,
            Severity::Info,
            "serving worker starting",
            None,
        ),
        SupervisionServingState::Stopped => Finding::warn(
            FindingId::WorkerState,
            Severity::Warning,
            "serving worker stopped",
            Some("there is no active deployment to serve".into()),
        ),
        SupervisionServingState::Failed | SupervisionServingState::CrashLoop => Finding::fail(
            FindingId::WorkerState,
            Severity::Critical,
            format!("serving worker state={:?}", sup.serving_state),
            Some("inspect agent supervision logs and the last_failure_message".into()),
        ),
        _ => Finding::warn(
            FindingId::WorkerState,
            Severity::Warning,
            format!("serving worker state={:?}", sup.serving_state),
            None,
        ),
    }
}

fn response_error(
    response: &tensorplate_protocol::agent_control::ControlResponse,
) -> (String, tensorplate_protocol::ErrorCode, Option<String>) {
    response.error.as_ref().map_or_else(
        || {
            (
                "agent returned non-OK status without typed error".into(),
                tensorplate_protocol::ErrorCode::Internal,
                None,
            )
        },
        |e| (e.message.clone(), e.code, e.context.clone()),
    )
}

fn profile_target(profile: &ResolvedProfile) -> String {
    match &profile.transport {
        crate::profile::Transport::UnixSocket { path } => path.display().to_string(),
        crate::profile::Transport::LoopbackTcp { host, port } => format!("{host}:{port}"),
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
    use crate::args::{DoctorArgs, OutputMode};
    use crate::client::MockAgentClient;
    use crate::config::{CliConfig, ProfileMode};
    use crate::profile::{ResolvedProfile, Transport};
    use serde_json::Value;
    use std::time::Duration;
    use tensorplate_protocol::agent_control::{AgentStatus, ControlResponse, DeploymentSummary};

    fn profile() -> ResolvedProfile {
        ResolvedProfile {
            name: "local".into(),
            mode: ProfileMode::Local,
            display_name: None,
            transport: Transport::UnixSocket {
                path: PathBuf::from("/nonexistent/agent.sock"),
            },
            serving_url: None,
            timeout: Duration::from_secs(5),
        }
    }

    fn ok_status_response(active: bool) -> ControlResponse {
        let mut status = AgentStatus {
            agent_state: AgentRunState::Ready,
            active: None,
            previous_active: None,
            candidate: None,
            in_flight_transaction: None,
            last_error: None,
            quarantined: vec![],
            recovery: None,
            supervision: None,
        };
        if active {
            status.active = Some(DeploymentSummary {
                deployment_id: "d-1".into(),
                bundle_digest: "sha256:abc".into(),
                bundle_name: Some("yolov8".into()),
                bundle_version: Some("0.0.1".into()),
                backend_hint: Some("tensorrt".into()),
                model_class: Some("vision".into()),
                staged_path: None,
                promoted_monotonic_ns: Some(1),
            });
        }
        ControlResponse {
            agent_status: Some(status),
            ..ControlResponse::ok(Some("corr".into()))
        }
    }

    #[test]
    fn doctor_reports_skipped_agent_probe() {
        let _ = CliConfig::default().validate().unwrap();
        let client = MockAgentClient::new();
        let args = DoctorArgs { skip_agent: true };
        let r = Renderer::new(OutputMode::Json);
        let mut out = Vec::new();
        let mut err = Vec::new();
        // We expect the call to succeed because all probes are info/warn
        // (the agent-socket warning does not flip to fail).
        let result = run(&r, &profile(), &client, &args, &mut out, &mut err);
        assert!(result.is_ok());
        let parsed: Value = serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
        let findings = parsed["payload"]["findings"].as_array().unwrap();
        let agent_finding = findings
            .iter()
            .find(|f| f["id"] == "agent_reachable")
            .unwrap();
        assert_eq!(agent_finding["status"], "skipped");
    }

    #[test]
    fn doctor_reports_failing_finding_when_agent_unreachable() {
        let client = MockAgentClient::new();
        client.enqueue_err(CliError::Transport {
            message: "connection refused".into(),
            hint: Some("start the agent".into()),
        });
        let args = DoctorArgs::default();
        let r = Renderer::new(OutputMode::Human);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = run(&r, &profile(), &client, &args, &mut out, &mut err);
        match result {
            Err(CliError::DoctorFindings { failing, .. }) => {
                assert!(failing >= 1);
            }
            other => panic!("expected DoctorFindings, got {other:?}"),
        }
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("agent unreachable"));
    }

    #[test]
    fn doctor_reports_active_deployment_when_present() {
        let client = MockAgentClient::new();
        // first call: version (Ok)
        client.enqueue_ok(ControlResponse::ok(Some("v".into())));
        // second call: status (Ok with active)
        client.enqueue_ok(ok_status_response(true));
        let args = DoctorArgs::default();
        let r = Renderer::new(OutputMode::Json);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = run(&r, &profile(), &client, &args, &mut out, &mut err);
        assert!(result.is_ok());
        let parsed: Value = serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
        let findings = parsed["payload"]["findings"].as_array().unwrap();
        assert!(findings
            .iter()
            .any(|f| f["id"] == "active_deployment" && f["status"] == "ok"));
    }

    #[test]
    fn doctor_flags_unsupported_profile() {
        let mut p = profile();
        p.mode = ProfileMode::Relay;
        let client = MockAgentClient::new();
        let args = DoctorArgs { skip_agent: true };
        let r = Renderer::new(OutputMode::Json);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = run(&r, &p, &client, &args, &mut out, &mut err);
        match result {
            Err(CliError::DoctorFindings { .. }) => {}
            other => panic!("expected DoctorFindings, got {other:?}"),
        }
    }
}
