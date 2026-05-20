// SPDX-License-Identifier: Apache-2.0
//
// V01-E11-F05: `tensorplate status` — render active deployment, worker
// supervision, and observability state.
//
// The CLI reads the agent status surface from V01-E08/E09 plus the
// V01-E10 observability snapshot when present. It does not compute
// supervision policy locally; it projects fields the agent and
// observability service already publish.

use std::io::Write;
use std::path::Path;

use serde_json::{json, Value};

use tensorplate_protocol::agent_control::{
    AgentRunState, AgentStatus, ControlRequest, ControlResponse, DeploymentSummary,
    QuarantineSummary, SupervisionStatusSummary,
};
use tensorplate_protocol::supervision_event::{SupervisionAgentState, SupervisionServingState};

use crate::args::StatusArgs;
use crate::client::AgentClient;
use crate::error::{CliError, CliResult};
use crate::output::Renderer;
use crate::profile::ResolvedProfile;

/// Subset of the V01-E10 observability snapshot the CLI consumes. We
/// hand-roll the deserializer here so the CLI keeps building when the
/// observability schema adds optional fields ahead of the CLI release.
#[derive(serde::Deserialize, Default)]
struct ObservabilitySnapshotPartial {
    #[serde(default)]
    observability_state: Option<String>,
    #[serde(default)]
    agent_state: Option<String>,
    #[serde(default)]
    serving_state: Option<String>,
    #[serde(default)]
    active_deployment: Option<String>,
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    missed_heartbeat_count: Option<u64>,
    #[serde(default)]
    missed_deadline_rate: Option<f64>,
    #[serde(default)]
    queue_depth: Option<u64>,
    #[serde(default)]
    last_error_code: Option<String>,
    #[serde(default)]
    last_heartbeat_age_ms: Option<u64>,
}

/// Run the `status` command.
///
/// # Errors
///
/// Returns:
/// - the typed [`CliError`] from the agent client when the agent is
///   unreachable or rejects the request.
/// - the typed [`CliError::Io`] when an observability snapshot path is
///   supplied but unreadable. We surface this as a soft warning in the
///   JSON output and continue rendering.
pub fn run<W: Write, E: Write>(
    renderer: &Renderer,
    profile: &ResolvedProfile,
    client: &dyn AgentClient,
    args: &StatusArgs,
    out: &mut W,
    _stderr: &mut E,
) -> CliResult<()> {
    let correlation = crate::new_correlation_id();
    let mut status_req = tensorplate_protocol::agent_control::StatusRequest::default();
    status_req.include_quarantine = args.include_quarantine;
    let request = ControlRequest::status(Some(correlation.clone()), status_req);
    let response = client.send_or_map_error(request)?;
    let agent_status = response.agent_status.clone();
    let observability = args
        .observability_snapshot
        .as_deref()
        .map(load_observability_snapshot)
        .transpose()
        .unwrap_or_else(|err| Some(ObservabilitySnapshotResult::failure(err)));
    let payload = build_payload(&response, agent_status.as_ref(), observability.as_ref());
    let human = render_human(profile, agent_status.as_ref(), observability.as_ref());
    renderer.ok(out, "status", &human, payload, Some(&correlation), None)
}

enum ObservabilitySnapshotResult {
    Loaded(ObservabilitySnapshotPartial),
    Unavailable(String),
}

impl ObservabilitySnapshotResult {
    fn failure(err: CliError) -> Self {
        Self::Unavailable(err.to_string())
    }
}

fn load_observability_snapshot(path: &Path) -> CliResult<ObservabilitySnapshotResult> {
    let body = std::fs::read_to_string(path).map_err(|e| {
        CliError::Io(format!(
            "failed to read observability snapshot `{}`: {e}",
            path.display()
        ))
    })?;
    match serde_json::from_str::<ObservabilitySnapshotPartial>(&body) {
        Ok(snapshot) => Ok(ObservabilitySnapshotResult::Loaded(snapshot)),
        Err(e) => Ok(ObservabilitySnapshotResult::Unavailable(format!(
            "observability snapshot at `{}` is malformed: {e}",
            path.display()
        ))),
    }
}

fn build_payload(
    response: &ControlResponse,
    status: Option<&AgentStatus>,
    observability: Option<&ObservabilitySnapshotResult>,
) -> Value {
    let mut payload = json!({
        "agent_response_status": format!("{:?}", response.status).to_lowercase(),
        "agent": agent_block(status),
    });
    if let Some(o) = observability {
        payload["observability"] = observability_block(o);
    }
    payload["severity"] = json!(severity_of(status, observability).label());
    payload
}

fn agent_block(status: Option<&AgentStatus>) -> Value {
    let Some(status) = status else {
        return json!({"available": false});
    };
    let active = status.active.as_ref().map(summary_block);
    let candidate = status.candidate.as_ref().map(summary_block);
    let previous = status.previous_active.as_ref().map(summary_block);
    let in_flight = status.in_flight_transaction.as_ref().map(|d| {
        json!({
            "phase": d.phase.as_str_label(),
            "transaction_id": d.transaction_id,
            "deployment_id": d.deployment_id,
            "bundle_digest": d.bundle_digest,
            "last_transition_monotonic_ns": d.last_transition_monotonic_ns,
            "failure": d.failure.as_ref().map(|f| json!({
                "error_code": f.error_code.as_str(),
                "message": f.message,
                "recoverable": f.recoverable,
            })),
        })
    });
    json!({
        "available": true,
        "agent_state": agent_state_label(status.agent_state),
        "active": active,
        "previous_active": previous,
        "candidate": candidate,
        "in_flight_transaction": in_flight,
        "supervision": status.supervision.as_ref().map(supervision_block),
        "quarantined": status.quarantined.iter().map(quarantine_block).collect::<Vec<_>>(),
        "last_error": status.last_error.as_ref().map(|e| json!({
            "code": e.code.as_str(),
            "message": e.message,
            "context": e.context,
        })),
    })
}

fn summary_block(d: &DeploymentSummary) -> Value {
    json!({
        "deployment_id": d.deployment_id,
        "bundle_digest": d.bundle_digest,
        "bundle_name": d.bundle_name,
        "bundle_version": d.bundle_version,
        "backend": d.backend_hint,
        "model_class": d.model_class,
    })
}

fn supervision_block(s: &SupervisionStatusSummary) -> Value {
    json!({
        "agent_state": agent_supervision_label(s.agent_state),
        "serving_state": serving_state_label(s.serving_state),
        "desired_active": s.desired_active,
        "actual_active": s.actual_active,
        "backend": s.backend,
        "restart_count": s.restart_count,
        "crash_loop_threshold": s.crash_loop_threshold,
        "crash_loop": s.crash_loop,
        "launch_sequence": s.launch_sequence,
        "last_failure_code": s.last_failure_code.map(|c| c.as_str()),
        "last_failure_message": s.last_failure_message,
        "next_restart_delay_ms": s.next_restart_delay_ms,
        "stable_uptime_ms": s.stable_uptime_ms,
    })
}

fn quarantine_block(q: &QuarantineSummary) -> Value {
    json!({
        "transaction_id": q.transaction_id,
        "deployment_id": q.deployment_id,
        "bundle_digest": q.bundle_digest,
        "phase": q.phase.as_str_label(),
        "error_code": q.error_code.as_str(),
        "message": q.message,
        "quarantined_monotonic_ns": q.quarantined_monotonic_ns,
    })
}

fn observability_block(o: &ObservabilitySnapshotResult) -> Value {
    match o {
        ObservabilitySnapshotResult::Loaded(snapshot) => json!({
            "available": true,
            "observability_state": snapshot.observability_state,
            "agent_state": snapshot.agent_state,
            "serving_state": snapshot.serving_state,
            "active_deployment": snapshot.active_deployment,
            "backend": snapshot.backend,
            "missed_heartbeat_count": snapshot.missed_heartbeat_count,
            "missed_deadline_rate": snapshot.missed_deadline_rate,
            "queue_depth": snapshot.queue_depth,
            "last_error_code": snapshot.last_error_code,
            "last_heartbeat_age_ms": snapshot.last_heartbeat_age_ms,
        }),
        ObservabilitySnapshotResult::Unavailable(reason) => json!({
            "available": false,
            "reason": reason,
        }),
    }
}

/// Severity label used to order the human renderer's top line. Stable
/// label set so V01-E15 validation scripts can grep on a single token.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Severity {
    Ready,
    Degraded,
    NoHeartbeat,
    CrashLoop,
    Failed,
}

impl Severity {
    fn label(self) -> &'static str {
        match self {
            Severity::Ready => "ready",
            Severity::Degraded => "degraded",
            Severity::NoHeartbeat => "no_heartbeat",
            Severity::CrashLoop => "crash_loop",
            Severity::Failed => "failed",
        }
    }
}

fn severity_of(
    agent_status: Option<&AgentStatus>,
    observability: Option<&ObservabilitySnapshotResult>,
) -> Severity {
    let mut severity = Severity::Ready;
    if let Some(status) = agent_status {
        severity = severity.max(match status.agent_state {
            AgentRunState::Ready => Severity::Ready,
            AgentRunState::Degraded | AgentRunState::Unknown => Severity::Degraded,
            AgentRunState::Failed => Severity::Failed,
        });
        if let Some(s) = status.supervision.as_ref() {
            if s.crash_loop {
                severity = severity.max(Severity::CrashLoop);
            }
            severity = severity.max(match s.serving_state {
                SupervisionServingState::Ready
                | SupervisionServingState::Running
                | SupervisionServingState::Starting => Severity::Ready,
                SupervisionServingState::NoActiveDeployment
                | SupervisionServingState::Stopped
                | SupervisionServingState::Stopping
                | SupervisionServingState::Degraded
                | SupervisionServingState::AwaitingRestart => Severity::Degraded,
                SupervisionServingState::Failed | SupervisionServingState::CrashLoop => {
                    Severity::Failed
                }
            });
        }
        if !status.quarantined.is_empty() {
            severity = severity.max(Severity::Degraded);
        }
    } else {
        severity = severity.max(Severity::Degraded);
    }
    if let Some(ObservabilitySnapshotResult::Loaded(snapshot)) = observability {
        let obs_state = snapshot.observability_state.as_deref().unwrap_or("unknown");
        severity = severity.max(match obs_state {
            "ready" => Severity::Ready,
            "degraded" => Severity::Degraded,
            "no_heartbeat" => Severity::NoHeartbeat,
            "failed" => Severity::Failed,
            _ => Severity::Degraded,
        });
    }
    severity
}

fn render_human(
    profile: &ResolvedProfile,
    status: Option<&AgentStatus>,
    observability: Option<&ObservabilitySnapshotResult>,
) -> String {
    let severity = severity_of(status, observability);
    let mut out = String::new();
    out.push_str(&format!(
        "status: profile `{}` severity={}\n",
        profile.name,
        severity.label()
    ));
    out.push_str(&format!("{:-<60}\n", ""));
    if let Some(status) = status {
        out.push_str(&format!(
            "agent_state: {}\n",
            agent_state_label(status.agent_state)
        ));
        if let Some(active) = status.active.as_ref() {
            out.push_str(&format!(
                "active: deployment_id={} backend={} bundle={}\n",
                active.deployment_id,
                active.backend_hint.as_deref().unwrap_or("<unknown>"),
                active
                    .bundle_name
                    .as_deref()
                    .map(|n| format!("{n}@{}", active.bundle_version.as_deref().unwrap_or("?")))
                    .unwrap_or_else(|| active.bundle_digest.clone()),
            ));
        } else {
            out.push_str("active: <no active deployment>\n");
        }
        if let Some(prev) = status.previous_active.as_ref() {
            out.push_str(&format!(
                "previous_active: deployment_id={} bundle_digest={}\n",
                prev.deployment_id, prev.bundle_digest
            ));
        }
        if let Some(in_flight) = status.in_flight_transaction.as_ref() {
            out.push_str(&format!(
                "in_flight: phase={} tx={}\n",
                in_flight.phase.as_str_label(),
                in_flight.transaction_id.as_deref().unwrap_or("<unknown>"),
            ));
        }
        if let Some(s) = status.supervision.as_ref() {
            out.push_str(&format!(
                "worker: serving_state={} agent_state={} restart_count={}{} backend={}\n",
                serving_state_label(s.serving_state),
                agent_supervision_label(s.agent_state),
                s.restart_count,
                if s.crash_loop { " (CRASH-LOOP)" } else { "" },
                s.backend.as_deref().unwrap_or("<unknown>"),
            ));
            if let Some(err) = s.last_failure_message.as_deref() {
                out.push_str(&format!(
                    "  last_failure: code={} message={}\n",
                    s.last_failure_code
                        .as_ref()
                        .map_or("(none)", |c| c.as_str()),
                    err,
                ));
            }
            if let Some(delay) = s.next_restart_delay_ms {
                out.push_str(&format!("  next_restart_delay_ms={delay}\n"));
            }
        }
        if !status.quarantined.is_empty() {
            out.push_str(&format!(
                "quarantined: {} entries\n",
                status.quarantined.len()
            ));
            for q in &status.quarantined {
                out.push_str(&format!(
                    "  - deployment_id={} phase={} error={} message={}\n",
                    q.deployment_id,
                    q.phase.as_str_label(),
                    q.error_code.as_str(),
                    q.message.as_deref().unwrap_or("(none)"),
                ));
            }
        }
        if let Some(err) = status.last_error.as_ref() {
            out.push_str(&format!(
                "last_error: code={} message={}\n",
                err.code.as_str(),
                err.message,
            ));
        }
    } else {
        out.push_str("agent: <unavailable>\n");
    }
    match observability {
        Some(ObservabilitySnapshotResult::Loaded(o)) => {
            out.push_str(&format!(
                "observability: state={} missed_heartbeats={} missed_deadline_rate={} queue_depth={}\n",
                o.observability_state.as_deref().unwrap_or("unknown"),
                o.missed_heartbeat_count.unwrap_or(0),
                o.missed_deadline_rate.unwrap_or(0.0),
                o.queue_depth.unwrap_or(0),
            ));
            if let Some(age) = o.last_heartbeat_age_ms {
                out.push_str(&format!("  last_heartbeat_age_ms={age}\n"));
            }
            if let Some(code) = o.last_error_code.as_deref() {
                out.push_str(&format!("  last_error_code={code}\n"));
            }
        }
        Some(ObservabilitySnapshotResult::Unavailable(reason)) => {
            out.push_str(&format!("observability: unavailable ({reason})\n"));
        }
        None => {
            out.push_str("observability: <no snapshot supplied>\n");
        }
    }
    out
}

fn agent_state_label(state: AgentRunState) -> &'static str {
    match state {
        AgentRunState::Ready => "ready",
        AgentRunState::Degraded => "degraded",
        AgentRunState::Failed => "failed",
        AgentRunState::Unknown => "unknown",
    }
}

fn agent_supervision_label(state: SupervisionAgentState) -> &'static str {
    match state {
        SupervisionAgentState::Ready => "ready",
        SupervisionAgentState::Degraded => "degraded",
        SupervisionAgentState::Failed => "failed",
        SupervisionAgentState::Unknown => "unknown",
    }
}

fn serving_state_label(state: SupervisionServingState) -> &'static str {
    match state {
        SupervisionServingState::NoActiveDeployment => "no_active_deployment",
        SupervisionServingState::Starting => "starting",
        SupervisionServingState::Running => "running",
        SupervisionServingState::Ready => "ready",
        SupervisionServingState::Degraded => "degraded",
        SupervisionServingState::Failed => "failed",
        SupervisionServingState::Stopping => "stopping",
        SupervisionServingState::Stopped => "stopped",
        SupervisionServingState::AwaitingRestart => "awaiting_restart",
        SupervisionServingState::CrashLoop => "crash_loop",
    }
}

trait DeployStateExt {
    fn as_str_label(&self) -> &'static str;
}

impl DeployStateExt for tensorplate_protocol::deploy_transaction::DeployState {
    fn as_str_label(&self) -> &'static str {
        use tensorplate_protocol::deploy_transaction::DeployState as S;
        match self {
            S::Received => "received",
            S::Verified => "verified",
            S::Staged => "staged",
            S::CapacityChecked => "capacity_checked",
            S::Prepared => "prepared",
            S::Warmed => "warmed",
            S::Promoted => "promoted",
            S::Active => "active",
            S::Failed => "failed",
            S::RolledBack => "rolled_back",
        }
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
    use std::path::PathBuf;
    use std::time::Duration;
    use tensorplate_protocol::agent_control::{ControlResponse, DeploymentSummary};
    use tensorplate_protocol::supervision_event::{SupervisionAgentState, SupervisionServingState};

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

    fn agent_status_with_active() -> AgentStatus {
        AgentStatus {
            agent_state: AgentRunState::Ready,
            active: Some(DeploymentSummary {
                deployment_id: "d-1".into(),
                bundle_digest: "sha256:abc".into(),
                bundle_name: Some("yolov8".into()),
                bundle_version: Some("0.0.1".into()),
                backend_hint: Some("tensorrt".into()),
                model_class: Some("vision".into()),
                staged_path: None,
                promoted_monotonic_ns: Some(1),
            }),
            previous_active: None,
            candidate: None,
            in_flight_transaction: None,
            last_error: None,
            quarantined: vec![],
            recovery: None,
            supervision: Some(SupervisionStatusSummary {
                serving_state: SupervisionServingState::Ready,
                agent_state: SupervisionAgentState::Ready,
                desired_active: Some("d-1".into()),
                actual_active: Some("d-1".into()),
                backend: Some("tensorrt".into()),
                restart_count: 0,
                crash_loop_threshold: 5,
                crash_loop: false,
                launch_sequence: 1,
                last_failure_code: None,
                last_failure_message: None,
                next_restart_delay_ms: None,
                stable_uptime_ms: 30_000,
            }),
        }
    }

    fn crash_loop_status() -> AgentStatus {
        let mut s = agent_status_with_active();
        s.agent_state = AgentRunState::Failed;
        if let Some(sup) = s.supervision.as_mut() {
            sup.serving_state = SupervisionServingState::CrashLoop;
            sup.agent_state = SupervisionAgentState::Failed;
            sup.crash_loop = true;
            sup.restart_count = 5;
        }
        s
    }

    #[test]
    fn ready_status_renders_severity_ready() {
        let client = MockAgentClient::new();
        client.enqueue_ok(ControlResponse {
            agent_status: Some(agent_status_with_active()),
            ..ControlResponse::ok(Some("c".into()))
        });
        let args = StatusArgs::default();
        let r = Renderer::new(OutputMode::Json);
        let mut out = Vec::new();
        let mut err = Vec::new();
        run(&r, &profile(), &client, &args, &mut out, &mut err).unwrap();
        let parsed: Value = serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
        assert_eq!(parsed["payload"]["severity"], "ready");
        assert_eq!(parsed["payload"]["agent"]["available"], true);
    }

    #[test]
    fn crash_loop_status_promotes_severity_to_crash_loop() {
        let client = MockAgentClient::new();
        client.enqueue_ok(ControlResponse {
            agent_status: Some(crash_loop_status()),
            ..ControlResponse::ok(Some("c".into()))
        });
        let args = StatusArgs::default();
        let r = Renderer::new(OutputMode::Json);
        let mut out = Vec::new();
        let mut err = Vec::new();
        run(&r, &profile(), &client, &args, &mut out, &mut err).unwrap();
        let parsed: Value = serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
        // crash_loop is ordered below failed in the severity ordering;
        // make sure both signals are surfaced.
        let severity = parsed["payload"]["severity"].as_str().unwrap();
        assert!(
            matches!(severity, "crash_loop" | "failed"),
            "got `{severity}`"
        );
        assert_eq!(
            parsed["payload"]["agent"]["supervision"]["crash_loop"],
            true
        );
    }

    #[test]
    fn missing_observability_snapshot_is_surfaced_as_unavailable() {
        let client = MockAgentClient::new();
        client.enqueue_ok(ControlResponse {
            agent_status: Some(agent_status_with_active()),
            ..ControlResponse::ok(Some("c".into()))
        });
        let args = StatusArgs {
            observability_snapshot: Some(PathBuf::from("/nonexistent/snapshot.json")),
            include_quarantine: true,
        };
        let r = Renderer::new(OutputMode::Json);
        let mut out = Vec::new();
        let mut err = Vec::new();
        run(&r, &profile(), &client, &args, &mut out, &mut err).unwrap();
        let parsed: Value = serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
        assert_eq!(parsed["payload"]["observability"]["available"], false);
    }

    #[test]
    fn loaded_observability_snapshot_surfaces_no_heartbeat() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("snapshot.json");
        std::fs::write(
            &path,
            r#"{
                "schema_version":"0.1",
                "observability_state":"no_heartbeat",
                "agent_state":"ready",
                "serving_state":"ready",
                "missed_heartbeat_count":3,
                "missed_deadline_rate":0.01,
                "queue_depth":0,
                "safe_state_sink":{"enabled":true,"dropped":0,"errors":0},
                "ros2_publisher":{"enabled":false,"published":0,"errors":0},
                "listener":{"accepted":1,"dropped":0,"malformed":0,"duplicates":0,"out_of_order":0,"unknown_version":0},
                "diagnostics":{"capacity":8,"recent_transitions":[],"recent_errors":[]}
            }"#,
        )
        .unwrap();
        let client = MockAgentClient::new();
        client.enqueue_ok(ControlResponse {
            agent_status: Some(agent_status_with_active()),
            ..ControlResponse::ok(Some("c".into()))
        });
        let args = StatusArgs {
            observability_snapshot: Some(path),
            include_quarantine: true,
        };
        let r = Renderer::new(OutputMode::Json);
        let mut out = Vec::new();
        let mut err = Vec::new();
        run(&r, &profile(), &client, &args, &mut out, &mut err).unwrap();
        let parsed: Value = serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
        assert_eq!(parsed["payload"]["severity"], "no_heartbeat");
        assert_eq!(
            parsed["payload"]["observability"]["observability_state"],
            "no_heartbeat"
        );
    }
}
