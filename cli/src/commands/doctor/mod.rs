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

use tensorplate_platform::{
    HostIdentity, HostReport, PlatformProbeError, PlatformRegistry, PlatformRegistryError,
    ProfileSelection, SystemHostProbe,
};

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
    findings.extend(probe_agent_config());
    findings.extend(probe_host_profile());
    findings.extend(probe_ros2_health_stub());
    // packaging install probes: filesystem layout, configs,
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

/// Render the host section from an already-detected host.
///
/// Pure: no filesystem, no subprocesses, no network. Detection and
/// registry loading happen in [`probe_host_profile`], so this can be
/// driven from the committed host-identity fixtures and asserted against
/// goldens without the machine running the test leaking into the answer.
#[must_use]
pub fn render_host_section(
    detected: Result<&HostReport, &PlatformProbeError>,
    registry: Result<&PlatformRegistry, &PlatformRegistryError>,
) -> Vec<Finding> {
    let report = match detected {
        Ok(report) => report,
        Err(err) => {
            // A source that could not be read is not a platform that is
            // unsupported. This is a warning, not a failure: `doctor` is
            // what an operator runs to find out why their access is wrong,
            // and a sandbox that blocks `sysctl` must not make it exit
            // non-zero.
            // The two failures need different remedies, so they get
            // different hints. Telling someone whose `/etc/nv_tegra_release`
            // is malformed to re-run as root wastes their next ten minutes.
            let hint = match err {
                PlatformProbeError::Unreadable { .. } => {
                    "a detection source could not be read — re-run as a user that can read /etc and /proc"
                }
                PlatformProbeError::Unrecognized { .. } => {
                    "a detection source was readable but not interpretable — the named source is malformed on this image; attach `tensorplate doctor --output json`"
                }
            };
            return vec![
                Finding::warn(
                    FindingId::HostFacts,
                    Severity::Warning,
                    format!("host identity could not be detected: {err}"),
                    Some(hint.into()),
                ),
                Finding::skipped(
                    FindingId::HostOs,
                    Severity::Info,
                    "skipped: host identity undetected",
                    None,
                ),
                Finding::skipped(
                    FindingId::PlatformProfile,
                    Severity::Info,
                    "skipped: host identity undetected",
                    None,
                ),
            ];
        }
    };

    let identity = &report.identity;
    let mut findings = vec![Finding::ok(
        FindingId::HostFacts,
        Severity::Info,
        format!(
            "host arch={} vendor={}",
            identity.architecture.as_reported(),
            identity.vendor.as_reported()
        ),
        None,
    )];

    // The exact strings alongside the row-comparable ones: an operator
    // filing evidence needs the build, and matching deliberately does not.
    let mut os = format!("{} {}", identity.os_name, identity.os_version);
    if let Some(image) = identity.image_identity.as_deref() {
        os.push_str(&format!(" ({image})"));
    }
    if let Some(machine_type) = identity.machine_type.as_deref() {
        os.push_str(&format!(" on {machine_type}"));
    }
    let exact = [
        report
            .exact
            .os_version
            .as_deref()
            .map(|v| format!("version {v}")),
        report
            .exact
            .os_build
            .as_deref()
            .map(|b| format!("build {b}")),
        report
            .exact
            .l4t_release
            .as_deref()
            .map(|l| format!("L4T {l}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if !exact.is_empty() {
        os.push_str(&format!(" [exact: {}]", exact.join(", ")));
    }
    findings.push(Finding::ok(FindingId::HostOs, Severity::Info, os, None));

    findings.push(match registry {
        Ok(registry) => render_platform_profile(registry, identity),
        // Deliberately does not say the registry is absent: it may be
        // installed and merely unreadable by this account, and sending an
        // operator to reinstall a package that is already there is the
        // wrong next step. `platform_registry` owns that diagnosis.
        Err(err) => Finding::skipped(
            FindingId::PlatformProfile,
            Severity::Info,
            format!("skipped: the platform registry could not be loaded ({err})"),
            Some("see the platform_registry finding for what to do about it".into()),
        ),
    });
    findings
}

/// Which support rows this host could be.
///
/// Deliberately a *set*: rows sharing an OS and CPU profile differ only by
/// accelerator, so host identity alone cannot name one. Reporting a single
/// row here would assert a match that has not been established.
fn render_platform_profile(registry: &PlatformRegistry, identity: &HostIdentity) -> Finding {
    match registry.select_profile(identity) {
        ProfileSelection::Candidates(rows) => {
            let ids = rows
                .iter()
                .map(|row| row.row_id())
                .collect::<Vec<_>>()
                .join(", ");
            let hint = (rows.len() > 1).then(|| {
                "several rows share this host profile; accelerator identity is needed to name one"
                    .to_string()
            });
            Finding::ok(
                FindingId::PlatformProfile,
                Severity::Info,
                format!(
                    "host matches {} candidate support row(s): {ids}",
                    rows.len()
                ),
                hint,
            )
        }
        // Not a failure of this machine or of doctor: an off-matrix host is
        // a normal, reportable state, and the typed reason says which
        // dimension put it there.
        // The host is one machine shape away from rows it otherwise
        // matches. Saying "unsupported" without qualification would be
        // read as "this hardware is not supported", which is not the
        // claim: the hardware is, this chassis is not.
        // Unsupported is still the right status -- no support claim covers
        // this chassis. It no longer means undeployable, though: where the
        // matched row records its chassis signals as context rather than
        // gates, the agent admits such a machine on technical
        // prerequisites and reports it as unvalidated. Saying only
        // "unsupported" would now read as "this will not run", which is a
        // different and wrong claim. Which of the two applies depends on
        // the row the accelerator resolves to, and this is the host-level
        // answer, taken before any accelerator is identified -- so it
        // states both outcomes rather than guessing at one.
        ProfileSelection::OutsideValidatedEnvironment => Finding::unsupported(
            FindingId::PlatformProfile,
            Severity::Warning,
            "host hardware matches this release, but no row's evidence covers the machine shape it is running on"
                .to_string(),
            Some(
                "no support claim covers this chassis, which does not by itself mean it will not run: a managed-datacenter row admits the machine on technical prerequisites and reports it as unvalidated, while a row whose thermal, power or throttle gates are load-bearing requires evidence that covers the machine. The agent prints the resolved posture at startup. See the Validated on column in docs/release/support-matrix.md"
                    .into(),
            ),
        ),
        ProfileSelection::NoMatch(reason) => Finding::unsupported(
            FindingId::PlatformProfile,
            Severity::Warning,
            format!("host matches no support row ({})", reason.as_str()),
            Some(
                "see docs/release/support-matrix.md for the platforms this release validates"
                    .into(),
            ),
        ),
    }
}

/// The agent config schema, compiled into the CLI.
///
/// Embedded rather than read from disk on purpose: this check exists to be
/// run BEFORE an upgrade, when the schema on the host is still the old
/// package's. The copy that matters is the one belonging to the version
/// about to be installed, which is the one this binary was built with.
const AGENT_CONFIG_SCHEMA: &str = include_str!("../../../../config/schemas/agent.json");

/// Whether the installed agent config still satisfies its schema.
///
/// `cli/` may not depend on `agent/`, so this validates against the schema
/// rather than by calling the agent's own loader. That is sound only
/// because the two are held in agreement by a contract test; if they drift
/// this check drifts with them, which is the reason that test exists.
fn probe_agent_config() -> Vec<Finding> {
    let path = std::path::Path::new(tensorplate_protocol::install_paths::AGENT_CONFIG_PATH);
    let body = match std::fs::read_to_string(path) {
        Ok(body) => body,
        // Absent is not a fault here: a CLI-only install has no agent
        // config, and `agent_reachable` already reports a missing agent.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return vec![Finding::skipped(
                FindingId::AgentConfigValid,
                Severity::Info,
                format!("no agent config at {}", path.display()),
                None,
            )]
        }
        Err(err) => {
            return vec![Finding::unsupported(
                FindingId::AgentConfigValid,
                Severity::Warning,
                format!("cannot read {}: {err}", path.display()),
                Some("the config is root:tensorplate 0640; run as root or a member of the tensorplate group".into()),
            )]
        }
    };

    let document: serde_json::Value = match serde_json::from_str(&body) {
        Ok(document) => document,
        Err(err) => {
            return vec![Finding::unsupported(
                FindingId::AgentConfigValid,
                Severity::Critical,
                format!("{} is not valid JSON: {err}", path.display()),
                Some("the agent will refuse to start until this parses".into()),
            )]
        }
    };

    // The schema is compiled in, so a failure here is a build-time defect
    // rather than anything about this host -- reported, not panicked, so
    // one broken embed cannot take out every other check doctor runs.
    let Ok(schema) = serde_json::from_str::<serde_json::Value>(AGENT_CONFIG_SCHEMA) else {
        return vec![Finding::skipped(
            FindingId::AgentConfigValid,
            Severity::Warning,
            "the embedded agent config schema is not valid JSON".to_string(),
            None,
        )];
    };
    let Ok(compiled) = jsonschema::JSONSchema::compile(&schema) else {
        return vec![Finding::skipped(
            FindingId::AgentConfigValid,
            Severity::Warning,
            "the embedded agent config schema did not compile".to_string(),
            None,
        )];
    };

    // Collected into owned strings before returning: the error iterator
    // borrows both the compiled schema and the document, and neither
    // outlives this function.
    let problems: Option<Vec<String>> = match compiled.validate(&document) {
        Ok(()) => None,
        // Every problem, not the first. An operator fixing one key at a
        // time across repeated upgrade attempts is the failure this exists
        // to replace.
        Err(errors) => Some(
            errors
                .map(|e| {
                    let at = e.instance_path.to_string();
                    if at.is_empty() {
                        e.to_string()
                    } else {
                        format!("{at}: {e}")
                    }
                })
                .collect(),
        ),
    };

    match problems {
        None => vec![Finding::ok(
            FindingId::AgentConfigValid,
            Severity::Info,
            format!("{} satisfies the agent config schema", path.display()),
            None,
        )],
        Some(detail) => vec![Finding::unsupported(
            FindingId::AgentConfigValid,
            Severity::Critical,
            format!(
                "{} does not satisfy the agent config schema: {}",
                path.display(),
                detail.join("; ")
            ),
            Some("the agent refuses a config it cannot validate, so fix this BEFORE upgrading; unknown keys are rejected rather than ignored".into()),
        )],
    }
}

/// The host section for the machine this is running on.
fn probe_host_profile() -> Vec<Finding> {
    let detected = SystemHostProbe::new().detect();
    let registry = PlatformRegistry::load_installed();
    render_host_section(detected.as_ref(), registry.as_ref())
}

fn probe_ros2_health_stub() -> Vec<Finding> {
    let path = std::path::Path::new(tensorplate_protocol::install_paths::OBSERVABILITY_CONFIG_PATH);
    let Ok(body) = std::fs::read_to_string(path) else {
        return vec![Finding::missing(
            FindingId::Ros2HealthStub,
            Severity::Info,
            "observability config absent; ROS 2 health stub configuration not checked",
            None,
        )];
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) else {
        return vec![Finding::fail(
            FindingId::Ros2HealthStub,
            Severity::Warning,
            "observability config is not valid JSON; ROS 2 health stub configuration unreadable",
            Some("restore `/etc/tensorplate/observability.json` from the package".into()),
        )];
    };
    let Some(ros2) = value.get("ros2_health") else {
        return vec![Finding::fail(
            FindingId::Ros2HealthStub,
            Severity::Warning,
            "observability config omits the ROS 2 health stub section",
            Some(
                "restore the packaged observability config or add `ros2_health` explicitly".into(),
            ),
        )];
    };
    let Some(enabled) = ros2.get("enabled").and_then(serde_json::Value::as_bool) else {
        return vec![Finding::fail(
            FindingId::Ros2HealthStub,
            Severity::Warning,
            "ROS 2 health stub section has no boolean enabled flag",
            None,
        )];
    };
    vec![Finding::ok(
        FindingId::Ros2HealthStub,
        Severity::Info,
        format!("ROS 2 health stub config present (enabled={enabled})"),
        Some(
            "runtime health publication appears in `tensorplate status` snapshots when enabled"
                .into(),
        ),
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
                serving_url: None,
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

#[cfg(test)]
mod agent_config_check_tests {
    // No test asserts the embedded schema matches the committed file.
    // `include_str!` registers that file as a rebuild dependency, so any
    // edit forces a recompile and the constant cannot go stale -- such a
    // test can never fail, and one that cannot fail reads as cover it is
    // not providing. Verified by editing the schema and watching the
    // comparison still pass.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_config_with_an_unknown_key_is_reported_not_passed() {
        // The case the upgrade guidance is about: a stray key used to be
        // ignored and now refuses to start the agent, so doctor has to say
        // so BEFORE the package swap rather than after.
        let schema: serde_json::Value = serde_json::from_str(AGENT_CONFIG_SCHEMA).unwrap();
        let compiled = jsonschema::JSONSchema::compile(&schema).unwrap();
        let document: serde_json::Value = serde_json::from_str(
            r#"{"schema_version":"0.1","state_dir":"/v","staging_dir":"/s","socket_path":"/k","worker":{"mod":"process"}}"#,
        )
        .unwrap();
        assert!(
            compiled.validate(&document).is_err(),
            "a misspelled key must be reported; the agent will refuse it"
        );
    }

    #[test]
    fn the_shipped_config_passes_the_check() {
        // Whatever else this reports, it must not condemn the config the
        // package installs.
        let schema: serde_json::Value = serde_json::from_str(AGENT_CONFIG_SCHEMA).unwrap();
        let compiled = jsonschema::JSONSchema::compile(&schema).unwrap();
        let shipped: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../packaging/conf/agent.json"),
            )
            .expect("read the shipped config"),
        )
        .unwrap();
        assert!(compiled.validate(&shipped).is_ok());
    }

    #[test]
    fn a_missing_config_is_skipped_not_failed() {
        // A CLI-only install has no agent config, and `agent_reachable`
        // already reports a missing agent. Failing here would make every
        // workstation install look broken.
        let findings = probe_agent_config();
        let finding = findings.first().expect("one finding");
        assert_eq!(finding.id, FindingId::AgentConfigValid);
        if !std::path::Path::new(tensorplate_protocol::install_paths::AGENT_CONFIG_PATH).exists() {
            assert_eq!(finding.status, FindingStatus::Skipped);
        }
    }
}
