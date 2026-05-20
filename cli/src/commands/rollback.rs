// SPDX-License-Identifier: Apache-2.0
//
// V01-E11-F04-T02: `tensorplate rollback` — restore the previous active
// deployment by calling the agent rollback transaction API.
//
// The CLI does not implement rollback semantics locally. It calls
// `ControlOp::Rollback` and projects the response onto operator-friendly
// output, surfacing the typed `Unavailable` response when no previous
// active deployment exists.

use std::io::Write;

use serde_json::{json, Value};

use tensorplate_protocol::agent_control::{ControlRequest, ControlResponse, RollbackRequest};

use crate::args::RollbackArgs;
use crate::client::AgentClient;
use crate::error::CliResult;
use crate::output::Renderer;
use crate::profile::ResolvedProfile;

/// Run the `rollback` command.
///
/// # Errors
///
/// Returns the typed [`crate::error::CliError`] from
/// [`AgentClient::send_or_map_error`] (which already translates
/// `Unavailable` into [`crate::error::CliError::Unavailable`]).
pub fn run<W: Write, E: Write>(
    renderer: &Renderer,
    profile: &ResolvedProfile,
    client: &dyn AgentClient,
    args: &RollbackArgs,
    out: &mut W,
    stderr: &mut E,
) -> CliResult<()> {
    let correlation = crate::new_correlation_id();
    let payload = RollbackRequest {
        reason: args.reason.clone(),
    };
    let request = ControlRequest::rollback(Some(correlation.clone()), payload);
    renderer.info(
        stderr,
        &format!(
            "rollback: requesting rollback on profile `{}` (reason={})",
            profile.name,
            args.reason.as_deref().unwrap_or("<unspecified>"),
        ),
    )?;
    let response = client.send_or_map_error(request)?;
    let transaction_id = response.transaction_id.clone();
    let payload = build_payload(&response);
    let human = render_human(profile, &response);
    renderer.ok(
        out,
        "rollback",
        &human,
        payload,
        Some(&correlation),
        transaction_id.as_deref(),
    )
}

fn render_human(profile: &ResolvedProfile, response: &ControlResponse) -> String {
    let tx = response.transaction_id.as_deref().unwrap_or("<unknown>");
    let mut out = format!(
        "rollback: profile `{}` transaction_id={tx} status=ok\n",
        profile.name
    );
    if let Some(active) = response
        .agent_status
        .as_ref()
        .and_then(|s| s.active.as_ref())
    {
        out.push_str(&format!(
            "  restored active deployment: {} (backend={})\n",
            active.deployment_id,
            active.backend_hint.as_deref().unwrap_or("<unknown>")
        ));
    }
    out
}

fn build_payload(response: &ControlResponse) -> Value {
    let active = response
        .agent_status
        .as_ref()
        .and_then(|s| s.active.as_ref());
    json!({
        "transaction_id": response.transaction_id,
        "restored_deployment_id": active.map(|a| a.deployment_id.clone()),
        "restored_bundle_digest": active.map(|a| a.bundle_digest.clone()),
        "restored_backend": active.and_then(|a| a.backend_hint.clone()),
    })
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
    use crate::error::CliError;
    use crate::profile::{ResolvedProfile, Transport};
    use std::path::PathBuf;
    use std::time::Duration;
    use tensorplate_protocol::agent_control::{
        AgentRunState, AgentStatus, ControlResponse, DeploymentSummary,
    };

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

    fn restored_response() -> ControlResponse {
        let restored = DeploymentSummary {
            deployment_id: "d-prev".into(),
            bundle_digest: "sha256:prev".into(),
            bundle_name: None,
            bundle_version: None,
            backend_hint: Some("mock".into()),
            model_class: None,
            staged_path: None,
            promoted_monotonic_ns: Some(7),
        };
        let agent_status = AgentStatus {
            agent_state: AgentRunState::Ready,
            active: Some(restored),
            previous_active: None,
            candidate: None,
            in_flight_transaction: None,
            last_error: None,
            quarantined: vec![],
            recovery: None,
            supervision: None,
        };
        ControlResponse {
            transaction_id: Some("tx-rollback".into()),
            agent_status: Some(agent_status),
            ..ControlResponse::ok(Some("corr".into()))
        }
    }

    #[test]
    fn rollback_success_renders_restored_deployment() {
        let client = MockAgentClient::new();
        client.enqueue_ok(restored_response());
        let args = RollbackArgs::default();
        let r = Renderer::new(OutputMode::Json);
        let mut out = Vec::new();
        let mut err = Vec::new();
        run(&r, &profile(), &client, &args, &mut out, &mut err).unwrap();
        let parsed: Value = serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
        assert_eq!(parsed["payload"]["transaction_id"], "tx-rollback");
        assert_eq!(parsed["payload"]["restored_deployment_id"], "d-prev");
        let history = client.history();
        assert_eq!(history.len(), 1);
        assert!(history[0].rollback.is_some());
    }

    #[test]
    fn rollback_unavailable_is_typed_unavailable() {
        let client = MockAgentClient::new();
        client.enqueue_ok(ControlResponse::unavailable(
            Some("c".into()),
            "no previous active deployment",
        ));
        let args = RollbackArgs::default();
        let r = Renderer::new(OutputMode::Human);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = run(&r, &profile(), &client, &args, &mut out, &mut err);
        match result {
            Err(CliError::Unavailable { message, .. }) => {
                assert!(message.contains("no previous active"))
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }
}
