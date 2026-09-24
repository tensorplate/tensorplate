// SPDX-License-Identifier: Apache-2.0
//
// `tensorplate undeploy` and `tensorplate recover`: act on one resident-set
// member through the agent's `undeploy` and `recover` operations.
//
// Like `rollback`, the CLI implements no set semantics locally. An agent
// that does not execute the operation answers with a typed `unsupported`
// error, which reaches the operator unmodified; an agent that predates the
// operations refuses the unknown op, so neither is ever misread.
//
// This module also holds the check the set-mutation flags of `deploy` and
// `rollback` run first: those flags add fields to existing requests, which
// an agent that predates them would ignore and act on the rest of the
// request, so the CLI sends them only to an agent that lists the matching
// control feature in its status.

use std::io::Write;

use serde_json::json;

use tensorplate_protocol::agent_control::{
    ControlOp, ControlRequest, MemberRequest, StatusRequest,
};
use tensorplate_protocol::ErrorCode;

use crate::args::MemberArgs;
use crate::client::AgentClient;
use crate::error::{CliError, CliResult};
use crate::output::Renderer;
use crate::profile::ResolvedProfile;

/// Run `undeploy` or `recover` (`op`) for the member `args` names.
///
/// # Errors
///
/// Returns the typed [`CliError`] from [`AgentClient::send_or_map_error`].
pub fn run<W: Write, E: Write>(
    renderer: &Renderer,
    profile: &ResolvedProfile,
    client: &dyn AgentClient,
    op: ControlOp,
    args: &MemberArgs,
    out: &mut W,
    stderr: &mut E,
) -> CliResult<()> {
    let correlation = crate::new_correlation_id();
    let payload = MemberRequest {
        deployment_id: args.deployment_id.clone(),
        reason: args.reason.clone(),
    };
    let request = match op {
        ControlOp::Undeploy => ControlRequest::undeploy(Some(correlation.clone()), payload),
        ControlOp::Recover => ControlRequest::recover(Some(correlation.clone()), payload),
        other => {
            return Err(CliError::Internal(format!(
                "`{other}` is not a member operation"
            )))
        }
    };
    renderer.info(
        stderr,
        &format!(
            "{op}: requesting `{}` on profile `{}` (reason={})",
            args.deployment_id,
            profile.name,
            args.reason.as_deref().unwrap_or("<unspecified>"),
        ),
    )?;
    let response = client.send_or_map_error(request)?;
    let transaction_id = response.transaction_id.clone();
    let human = format!(
        "{op}: profile `{}` deployment_id={} transaction_id={} status=ok\n",
        profile.name,
        args.deployment_id,
        transaction_id.as_deref().unwrap_or("<unknown>"),
    );
    let payload = json!({
        "deployment_id": args.deployment_id,
        "transaction_id": transaction_id,
    });
    renderer.ok(
        out,
        op.as_str(),
        &human,
        payload,
        Some(&correlation),
        transaction_id.as_deref(),
    )
}

/// Refuse locally unless the agent lists `feature` among its control
/// features. `what` names the flag for the error.
///
/// # Errors
///
/// Returns [`CliError::Agent`] with [`ErrorCode::Unsupported`] when the
/// feature is not listed, or the typed error of the status query.
pub fn require_control_feature(
    client: &dyn AgentClient,
    feature: &str,
    what: &str,
) -> CliResult<()> {
    let request = ControlRequest::status(
        Some(crate::new_correlation_id()),
        StatusRequest {
            include_quarantine: false,
        },
    );
    let response = client.send_or_map_error(request)?;
    let listed = response
        .agent_status
        .as_ref()
        .is_some_and(|status| status.control_features.iter().any(|f| f == feature));
    if listed {
        return Ok(());
    }
    Err(CliError::Agent {
        code: ErrorCode::Unsupported,
        message: format!("the agent does not support {what}"),
        context: None,
        hint: Some(format!(
            "the agent does not list the `{feature}` control feature, so the request was not sent"
        )),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::args::OutputMode;
    use crate::client::MockAgentClient;
    use crate::config::ProfileMode;
    use crate::profile::{ResolvedProfile, Transport};
    use std::path::PathBuf;
    use std::time::Duration;
    use tensorplate_protocol::agent_control::{
        AgentRunState, AgentStatus, ControlResponse, ResponseError, FEATURE_MEMBER_ROLLBACK,
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

    fn status_with(features: &[&str]) -> ControlResponse {
        ControlResponse {
            agent_status: Some(AgentStatus {
                agent_state: AgentRunState::Ready,
                control_features: features.iter().map(ToString::to_string).collect(),
                ..AgentStatus::default()
            }),
            ..ControlResponse::ok(None)
        }
    }

    #[test]
    fn undeploy_sends_the_member_and_surfaces_unsupported() {
        let mock = MockAgentClient::new();
        mock.enqueue_ok(ControlResponse::error(
            Some("c".into()),
            ResponseError::new(
                ErrorCode::Unsupported,
                "`undeploy` is not supported by this agent yet",
            ),
        ));
        let renderer = Renderer::new(OutputMode::Json);
        let args = MemberArgs {
            deployment_id: "speech-tts".into(),
            reason: Some("retired".into()),
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let result = run(
            &renderer,
            &profile(),
            &mock,
            ControlOp::Undeploy,
            &args,
            &mut out,
            &mut err,
        );
        match result {
            Err(CliError::Agent { code, message, .. }) => {
                assert_eq!(code, ErrorCode::Unsupported);
                assert!(message.contains("not supported"), "{message}");
            }
            other => panic!("expected the agent's typed error, got {other:?}"),
        }
        let sent = mock.history();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].op, ControlOp::Undeploy);
        let payload = sent[0].undeploy.as_ref().expect("undeploy payload");
        assert_eq!(payload.deployment_id, "speech-tts");
        assert_eq!(payload.reason.as_deref(), Some("retired"));
    }

    #[test]
    fn recover_sends_a_recover_request() {
        let mock = MockAgentClient::new();
        mock.enqueue_ok(ControlResponse {
            transaction_id: Some("tx-1".into()),
            ..ControlResponse::ok(None)
        });
        let renderer = Renderer::new(OutputMode::Json);
        let args = MemberArgs {
            deployment_id: "speech-stt".into(),
            reason: None,
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        run(
            &renderer,
            &profile(),
            &mock,
            ControlOp::Recover,
            &args,
            &mut out,
            &mut err,
        )
        .expect("ok");
        let sent = mock.history();
        assert_eq!(sent[0].op, ControlOp::Recover);
        assert_eq!(
            sent[0].recover.as_ref().expect("payload").deployment_id,
            "speech-stt"
        );
        let body: serde_json::Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(body["command"], "recover");
        assert_eq!(body["payload"]["transaction_id"], "tx-1");
    }

    #[test]
    fn a_non_member_operation_is_refused_before_sending() {
        let mock = MockAgentClient::new();
        let renderer = Renderer::new(OutputMode::Json);
        let args = MemberArgs {
            deployment_id: "speech-tts".into(),
            reason: None,
        };
        let result = run(
            &renderer,
            &profile(),
            &mock,
            ControlOp::Rollback,
            &args,
            &mut Vec::new(),
            &mut Vec::new(),
        );
        assert!(matches!(result, Err(CliError::Internal(_))), "{result:?}");
        assert!(mock.history().is_empty());
    }

    #[test]
    fn a_feature_the_agent_does_not_list_is_refused_locally() {
        for features in [&[][..], &["set_operation_add"][..]] {
            let mock = MockAgentClient::new();
            mock.enqueue_ok(status_with(features));
            let result =
                require_control_feature(&mock, FEATURE_MEMBER_ROLLBACK, "`--deployment-id`");
            match result {
                Err(CliError::Agent { code, message, .. }) => {
                    assert_eq!(code, ErrorCode::Unsupported);
                    assert!(message.contains("`--deployment-id`"), "{message}");
                }
                other => panic!("expected a local refusal, got {other:?}"),
            }
            assert_eq!(mock.history()[0].op, ControlOp::Status);
        }
        let mock = MockAgentClient::new();
        mock.enqueue_ok(status_with(&[FEATURE_MEMBER_ROLLBACK]));
        require_control_feature(&mock, FEATURE_MEMBER_ROLLBACK, "`--deployment-id`")
            .expect("listed");
    }
}
