// SPDX-License-Identifier: Apache-2.0
//
// Byte-level golden for the agent control API on a singleton deployment.
//
// `protocol/rust/tests/fixtures/agent_control_singleton_lifecycle.jsonl`
// holds the request and response lines of a deploy, a second deploy, a
// status query and a rollback, exactly as they cross the socket, recorded
// from the agent and the CLI's request builders before the set-mutation
// fields were added. This test replays the same sequence against today's
// agent and requires the same bytes, so a default (`replace`) deploy of a
// singleton deployment and its responses stay what every existing client
// already reads. Values that differ per run (transaction ids, monotonic
// timestamps, the temporary directory) are normalized in both the
// recording and the replay. Run with `UPDATE_GOLDEN=1` to re-record.
//
// `agent_control_member_status.jsonl` beside it holds two status exchanges
// recorded the same way from an agent that runs the member registry.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::default_trait_access
)]

mod common;
#[path = "support/registry_agent.rs"]
mod registry_agent;

use common::{vision_bundle, Harness};
use registry_agent::RegistryAgent;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use tensorplate_agent::server::Server;
use tensorplate_protocol::agent_control::{
    ControlRequest, DeployRequest, RollbackRequest, StatusRequest,
};

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../protocol/rust/tests/fixtures")
        .join(name)
}

fn golden_path() -> PathBuf {
    fixture_path("agent_control_singleton_lifecycle.jsonl")
}

/// The harness's temporary directory as it appears in a response. The
/// canonical form first: on macOS a temporary directory under `/var`
/// canonicalizes to `/private/var`.
fn tempdirs(h: &Harness) -> [String; 2] {
    [
        h.td.path()
            .canonicalize()
            .expect("canonical tempdir")
            .display()
            .to_string(),
        h.td.path().display().to_string(),
    ]
}

/// Require `lines` to be the recording at `path`, or write them there.
fn assert_recorded(path: &Path, lines: &[String]) {
    let replay = lines.join("\n") + "\n";
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(path, &replay).expect("write golden");
    }
    let golden = std::fs::read_to_string(path).expect("read golden");
    for (index, (want, got)) in golden.lines().zip(replay.lines()).enumerate() {
        assert_eq!(got, want, "line {} differs from the golden", index + 1);
    }
    assert_eq!(replay.lines().count(), golden.lines().count());
}

/// Send one request line and return the raw request and response lines.
fn exchange(socket: &Path, req: &ControlRequest) -> (String, String) {
    let request = serde_json::to_string(req).expect("ser");
    let mut stream = UnixStream::connect(socket).expect("connect");
    stream
        .write_all(format!("{request}\n").as_bytes())
        .expect("write");
    stream.flush().expect("flush");
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response).expect("read");
    assert!(response.ends_with('\n'), "a response is one line");
    response.pop();
    (request, response)
}

/// Replace every value that differs between runs with a fixed one, in
/// place in the raw text so key order and spelling stay exactly as sent.
fn normalize(line: &str, tempdirs: &[String], tx_ids: &mut Vec<String>) -> String {
    let mut out = line.to_string();
    for tempdir in tempdirs {
        out = out.replace(tempdir.as_str(), "/srv/golden");
    }
    // Transaction ids: `tx-` and a UUID, numbered by first appearance.
    let mut rebuilt = String::new();
    let mut rest = out.as_str();
    while let Some(at) = rest.find("\"tx-") {
        let start = at + 1;
        let end = start + 3 + 36;
        let id = &rest[start..end];
        let index = tx_ids
            .iter()
            .position(|seen| seen == id)
            .unwrap_or_else(|| {
                tx_ids.push(id.to_string());
                tx_ids.len() - 1
            });
        rebuilt.push_str(&rest[..start]);
        rebuilt.push_str(&format!("tx-00000000-0000-4000-8000-{:012}", index + 1));
        rest = &rest[end..];
    }
    rebuilt.push_str(rest);
    out = rebuilt;
    // Monotonic timestamps and uptimes: the digits after a `_ns` or `_ms` key.
    let mut rebuilt = String::new();
    let mut rest = out.as_str();
    loop {
        let next = ["_ns\":", "_ms\":"]
            .iter()
            .filter_map(|key| rest.find(key).map(|at| at + key.len()))
            .min();
        let Some(value_at) = next else { break };
        rebuilt.push_str(&rest[..value_at]);
        let digits = rest[value_at..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count();
        rebuilt.push_str(if digits > 0 { "0" } else { "" });
        rest = &rest[value_at + digits..];
    }
    rebuilt.push_str(rest);
    rebuilt
}

fn deploy_request(correlation: &str, bundle: &Path, deployment_id: &str) -> ControlRequest {
    // Built the way `tensorplate deploy` builds it.
    let mut labels = BTreeMap::new();
    labels.insert("team".to_string(), "vision".to_string());
    ControlRequest::deploy(
        Some(correlation.into()),
        DeployRequest {
            bundle_path: bundle.display().to_string(),
            deployment_id: deployment_id.into(),
            expected_bundle_digest: None,
            labels,
            ..DeployRequest::default()
        },
    )
}

#[test]
fn a_singleton_lifecycle_crosses_the_socket_byte_for_byte() {
    let h = Harness::new();
    let socket = h.config.socket_path.clone().expect("socket");
    let mut server = Server::start(&h.config, h.coord.clone()).expect("start");
    let tempdirs = tempdirs(&h);

    let b1 = vision_bundle(h.td.path(), "d1");
    let b2 = vision_bundle(h.td.path(), "d2");
    let requests = [
        deploy_request("ctl-000001", &b1, "d1"),
        deploy_request("ctl-000002", &b2, "d2"),
        ControlRequest::status(Some("ctl-000003".into()), StatusRequest::default()),
        ControlRequest::rollback(
            Some("ctl-000004".into()),
            RollbackRequest {
                deployment_id: None,
                reason: Some("operator rollback".into()),
            },
        ),
    ];
    let mut tx_ids = Vec::new();
    let mut lines = Vec::new();
    for request in &requests {
        let (sent, received) = exchange(&socket, request);
        lines.push(normalize(&sent, &tempdirs, &mut tx_ids));
        lines.push(normalize(&received, &tempdirs, &mut tx_ids));
    }
    server.shutdown();

    assert_recorded(&golden_path(), &lines);
}

#[test]
fn the_status_of_an_agent_that_runs_the_member_registry_crosses_the_socket_as_recorded() {
    let h = Harness::new();
    let agent = RegistryAgent::start(&h);
    let socket = h.config.socket_path.clone().expect("socket");
    let mut server = Server::start(&h.config, agent.coord.clone()).expect("start");
    let tempdirs = tempdirs(&h);
    let mut tx_ids = Vec::new();
    let mut lines = Vec::new();
    let mut record_status = |correlation: &str| {
        let request = ControlRequest::status(Some(correlation.into()), StatusRequest::default());
        let (sent, received) = exchange(&socket, &request);
        lines.push(normalize(&sent, &tempdirs, &mut tx_ids));
        lines.push(normalize(&received, &tempdirs, &mut tx_ids));
    };

    agent.deploy_member(&h);
    record_status("ctl-000001");
    agent.commit_member_as_a_set(&h);
    record_status("ctl-000002");
    server.shutdown();

    assert_recorded(&fixture_path("agent_control_member_status.jsonl"), &lines);
}
