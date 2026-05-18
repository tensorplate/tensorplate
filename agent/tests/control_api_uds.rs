// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F08-T01 control-API integration tests over the Unix-domain
// socket transport. Covers:
//
//   - the agent listens on a UDS configured by AgentConfig.
//   - a single connection carries one NDJSON request, one NDJSON response.
//   - happy-path deploy + status + rollback via real socket round-trips.
//   - typed agent_busy / unsupported / unavailable responses.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::default_trait_access
)]

mod common;

use common::{vision_bundle, Harness};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use tensorplate_agent::server::Server;
use tensorplate_protocol::agent_control::{
    ControlRequest, ControlResponse, DeployRequest, ResponseStatus, RollbackRequest,
};
use tensorplate_protocol::ErrorCode;

fn rt(socket: &Path, req: &ControlRequest) -> ControlResponse {
    let mut stream = UnixStream::connect(socket).expect("connect");
    let mut raw = serde_json::to_vec(req).expect("ser");
    raw.push(b'\n');
    stream.write_all(&raw).expect("write");
    stream.flush().expect("flush");
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read");
    serde_json::from_str(&line).expect("decode")
}

#[test]
fn full_deploy_then_status_then_rollback_over_uds() {
    let h = Harness::new();
    let socket = h.config.socket_path.clone().expect("socket");
    let mut server = Server::start(&h.config, h.coord.clone()).expect("start");

    let b1 = vision_bundle(h.td.path(), "d1");
    let resp = rt(
        &socket,
        &ControlRequest::deploy(
            Some("c1".into()),
            DeployRequest {
                bundle_path: b1.display().to_string(),
                deployment_id: "d1".into(),
                expected_bundle_digest: None,
                labels: Default::default(),
            },
        ),
    );
    assert_eq!(resp.status, ResponseStatus::Ok);

    let b2 = vision_bundle(h.td.path(), "d2");
    let resp = rt(
        &socket,
        &ControlRequest::deploy(
            None,
            DeployRequest {
                bundle_path: b2.display().to_string(),
                deployment_id: "d2".into(),
                expected_bundle_digest: None,
                labels: Default::default(),
            },
        ),
    );
    assert_eq!(resp.status, ResponseStatus::Ok);

    let status = rt(&socket, &ControlRequest::status(None, Default::default()));
    let agent = status.agent_status.as_ref().expect("agent status");
    assert_eq!(agent.active.as_ref().expect("active").deployment_id, "d2");
    assert_eq!(
        agent.previous_active.as_ref().expect("prev").deployment_id,
        "d1"
    );

    let rb = rt(
        &socket,
        &ControlRequest::rollback(None, RollbackRequest::default()),
    );
    assert_eq!(rb.status, ResponseStatus::Ok);
    let agent = rb.agent_status.as_ref().expect("agent status");
    assert_eq!(agent.active.as_ref().expect("a").deployment_id, "d1");

    server.shutdown();
}

#[test]
fn rollback_without_previous_returns_unavailable() {
    let h = Harness::new();
    let socket = h.config.socket_path.clone().expect("socket");
    let mut server = Server::start(&h.config, h.coord.clone()).expect("start");
    let resp = rt(
        &socket,
        &ControlRequest::rollback(None, RollbackRequest::default()),
    );
    assert_eq!(resp.status, ResponseStatus::Unavailable);
    server.shutdown();
}

#[test]
fn unknown_schema_version_response_is_typed() {
    let h = Harness::new();
    let socket = h.config.socket_path.clone().expect("socket");
    let mut server = Server::start(&h.config, h.coord.clone()).expect("start");

    let mut stream = UnixStream::connect(socket.as_path()).expect("connect");
    stream
        .write_all(b"{\"schema_version\":\"99.99\",\"op\":\"deploy\"}\n")
        .expect("write");
    stream.flush().expect("flush");
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read");
    let resp: ControlResponse = serde_json::from_str(&line).expect("decode");
    assert_eq!(resp.status, ResponseStatus::Error);
    assert_eq!(
        resp.error.as_ref().expect("err").code,
        ErrorCode::Unsupported
    );
    server.shutdown();
}

#[test]
fn deploy_with_missing_bundle_returns_typed_error() {
    let h = Harness::new();
    let socket = h.config.socket_path.clone().expect("socket");
    let mut server = Server::start(&h.config, h.coord.clone()).expect("start");
    let resp = rt(
        &socket,
        &ControlRequest::deploy(
            None,
            DeployRequest {
                bundle_path: "/does/not/exist".into(),
                deployment_id: "d-missing".into(),
                expected_bundle_digest: None,
                labels: Default::default(),
            },
        ),
    );
    assert_eq!(resp.status, ResponseStatus::Error);
    assert_eq!(
        resp.error.as_ref().expect("err").code,
        ErrorCode::ConfigInvalid
    );
    server.shutdown();
}
