// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F01: Request -> response handler for the local control API.
//
// Pure functions only: the I/O loop lives in `server`; this module just
// projects [`ControlRequest`] -> [`ControlResponse`] on top of the
// coordinator. Keeping it transport-agnostic lets the unit tests exercise
// the contract without spinning up a UDS listener.

use std::path::PathBuf;
use std::sync::Arc;

use tensorplate_protocol::agent_control::{
    ControlOp, ControlRequest, ControlResponse, ResponseError, ResponseStatus,
};
use tensorplate_protocol::ErrorCode;

use crate::coordinator::Coordinator;
use crate::error::{AgentError, AgentResult};
use crate::recovery;

/// Dispatch a single request. The control API contract guarantees one
/// request, one response per connection; concurrent conflicting mutating
/// requests return [`ResponseStatus::Busy`].
///
/// # Errors
///
/// Returns [`AgentError`] only when the durable state store is
/// unrecoverable. Routine errors (bad bundle, unsupported backend,
/// rollback unavailable) are projected onto [`ControlResponse::error`]
/// or [`ControlResponse::unavailable`] so the CLI sees a stable shape.
pub fn dispatch(
    coordinator: &Arc<Coordinator>,
    request: ControlRequest,
) -> AgentResult<ControlResponse> {
    let correlation = request.correlation_id.clone();
    match request.op {
        ControlOp::Health => Ok(ControlResponse {
            agent_status: Some(coordinator.status()?),
            ..ControlResponse::ok(correlation)
        }),
        ControlOp::Version => Ok(ControlResponse::ok(correlation)),
        ControlOp::Status => {
            let mut status = coordinator.status()?;
            let recovery = recovery::plan_with_worker(
                coordinator.state(),
                coordinator.worker_client().as_ref(),
            )
            .ok();
            status.recovery = recovery;
            let include_quarantine = request
                .status
                .as_ref()
                .map_or(true, |s| s.include_quarantine);
            if !include_quarantine {
                status.quarantined.clear();
            }
            Ok(ControlResponse {
                agent_status: Some(status),
                ..ControlResponse::ok(correlation)
            })
        }
        ControlOp::Deploy => {
            let Some(payload) = request.deploy else {
                return Ok(ControlResponse::error(
                    correlation,
                    ResponseError::new(ErrorCode::ConfigInvalid, "deploy payload missing"),
                ));
            };
            let path = PathBuf::from(&payload.bundle_path);
            match coordinator.deploy(
                &payload.deployment_id,
                &path,
                payload.labels,
                correlation.clone(),
                payload.expected_bundle_digest.as_deref(),
            ) {
                Ok(outcome) => {
                    let status = coordinator.status()?;
                    Ok(ControlResponse {
                        transaction_id: Some(outcome.transaction_id),
                        deploy_status: status.in_flight_transaction.clone(),
                        agent_status: Some(status),
                        ..ControlResponse::ok(correlation)
                    })
                }
                Err(err) => Ok(error_response(correlation, &err)),
            }
        }
        ControlOp::Rollback => match coordinator.rollback(correlation.clone()) {
            Ok(outcome) => {
                let status = coordinator.status()?;
                Ok(ControlResponse {
                    transaction_id: Some(outcome.transaction_id),
                    agent_status: Some(status),
                    ..ControlResponse::ok(correlation)
                })
            }
            Err(err) => Ok(error_response(correlation, &err)),
        },
    }
}

fn error_response(correlation: Option<String>, err: &AgentError) -> ControlResponse {
    let record = err.to_record();
    let typed = ResponseError {
        code: record.code,
        message: record.message,
        context: record.context,
    };
    match err {
        AgentError::Busy(_) => ControlResponse::busy(correlation, "agent_busy"),
        AgentError::Unavailable(msg) => ControlResponse::unavailable(correlation, msg.clone()),
        _ => ControlResponse {
            status: ResponseStatus::Error,
            error: Some(typed),
            ..ControlResponse::ok(correlation)
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_used,
        clippy::default_trait_access
    )]
    use super::dispatch;
    use crate::config::AgentConfig;
    use crate::coordinator::Coordinator;
    use crate::state::StateStore;
    use crate::worker::MockWorkerControl;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tensorplate_protocol::agent_control::{
        ControlRequest, DeployRequest, ResponseStatus, RollbackRequest,
    };
    use tensorplate_protocol::bundle_manifest::DeviceFamily;
    use tensorplate_protocol::SCHEMA_VERSION;

    fn config(state_dir: PathBuf, staging_dir: PathBuf) -> AgentConfig {
        AgentConfig {
            schema_version: SCHEMA_VERSION.to_string(),
            transport: crate::config::ControlTransport::UnixSocket,
            socket_path: Some(PathBuf::from("/tmp/agent.sock")),
            tcp_bind_host: "127.0.0.1".into(),
            tcp_bind_port: 0,
            state_dir,
            staging_dir,
            available_backends: vec!["mock".into()],
            backend_capabilities: BTreeMap::new(),
            device_memory_bytes: Some(8 * 1024 * 1024 * 1024),
            device_family: DeviceFamily::Any,
            worker: Default::default(),
            runtime_version: Some("0.1.0".into()),
        }
        .validate()
        .expect("valid")
    }

    fn write_bundle(dir: &std::path::Path, deployment: &str) -> PathBuf {
        use sha2::Digest;
        let bdir = dir.join(format!("bundle-{deployment}"));
        fs::create_dir_all(&bdir).expect("mkdir");
        let body: &[u8] = b"model-bytes";
        fs::write(bdir.join("model.engine"), body).expect("write");
        let mut h = sha2::Sha256::new();
        h.update(body);
        let dg = format!("sha256:{}", hex::encode(h.finalize()));
        let manifest = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"yolov8n","version":"{deployment}","format_version":"0.1","model_class":"vision","backend_hint":"mock","artifacts":[{{"role":"model","path":"model.engine","digest":"{dg}"}}]}}"#
        );
        fs::write(bdir.join("manifest.json"), manifest).expect("write manifest");
        bdir
    }

    #[test]
    fn deploy_status_rollback_round_trip() {
        let td = TempDir::new().expect("td");
        let state = td.path().join("state");
        let staging = td.path().join("staging");
        let cfg = config(state.clone(), staging);
        let store = Arc::new(StateStore::open(&state).expect("open"));
        let worker = Arc::new(MockWorkerControl::new());
        let coord = Arc::new(Coordinator::new(cfg, store, worker));

        // Deploy 1
        let b1 = write_bundle(td.path(), "d1");
        let r1 = dispatch(
            &coord,
            ControlRequest::deploy(
                Some("c1".into()),
                DeployRequest {
                    bundle_path: b1.display().to_string(),
                    deployment_id: "d1".into(),
                    expected_bundle_digest: None,
                    labels: Default::default(),
                },
            ),
        )
        .expect("ok");
        assert_eq!(r1.status, ResponseStatus::Ok);

        // Deploy 2
        let b2 = write_bundle(td.path(), "d2");
        let r2 = dispatch(
            &coord,
            ControlRequest::deploy(
                None,
                DeployRequest {
                    bundle_path: b2.display().to_string(),
                    deployment_id: "d2".into(),
                    expected_bundle_digest: None,
                    labels: Default::default(),
                },
            ),
        )
        .expect("ok");
        assert_eq!(r2.status, ResponseStatus::Ok);

        // Status: active=d2, previous=d1
        let s = dispatch(&coord, ControlRequest::status(None, Default::default())).expect("ok");
        let st = s.agent_status.as_ref().expect("status");
        assert_eq!(st.active.as_ref().expect("active").deployment_id, "d2");
        assert_eq!(
            st.previous_active.as_ref().expect("prev").deployment_id,
            "d1"
        );

        // Rollback: active becomes d1.
        let rb = dispatch(
            &coord,
            ControlRequest::rollback(None, RollbackRequest::default()),
        )
        .expect("ok");
        assert_eq!(rb.status, ResponseStatus::Ok);
        let st = rb.agent_status.as_ref().expect("status");
        assert_eq!(st.active.as_ref().expect("active").deployment_id, "d1");
        assert_eq!(
            st.previous_active.as_ref().expect("prev").deployment_id,
            "d2"
        );
    }

    #[test]
    fn rollback_with_no_previous_active_returns_unavailable() {
        let td = TempDir::new().expect("td");
        let state = td.path().join("state");
        let staging = td.path().join("staging");
        let cfg = config(state.clone(), staging);
        let store = Arc::new(StateStore::open(&state).expect("open"));
        let worker = Arc::new(MockWorkerControl::new());
        let coord = Arc::new(Coordinator::new(cfg, store, worker));
        let r = dispatch(
            &coord,
            ControlRequest::rollback(None, RollbackRequest::default()),
        )
        .expect("ok");
        assert_eq!(r.status, ResponseStatus::Unavailable);
    }

    #[test]
    fn deploy_with_bad_bundle_returns_typed_error() {
        let td = TempDir::new().expect("td");
        let state = td.path().join("state");
        let staging = td.path().join("staging");
        let cfg = config(state.clone(), staging);
        let store = Arc::new(StateStore::open(&state).expect("open"));
        let worker = Arc::new(MockWorkerControl::new());
        let coord = Arc::new(Coordinator::new(cfg, store, worker));
        // Bundle path doesn't exist.
        let r = dispatch(
            &coord,
            ControlRequest::deploy(
                None,
                DeployRequest {
                    bundle_path: "/does/not/exist".into(),
                    deployment_id: "d1".into(),
                    expected_bundle_digest: None,
                    labels: Default::default(),
                },
            ),
        )
        .expect("ok");
        assert_eq!(r.status, ResponseStatus::Error);
        assert!(r.error.is_some());
    }
}
