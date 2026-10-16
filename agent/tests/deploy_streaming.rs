// SPDX-License-Identifier: Apache-2.0
//
// A deploy that asks for streaming: what the agent renders for each
// generation of it, what status reports, and the refusals before staging.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;
#[path = "support/registry_agent.rs"]
mod registry_agent;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use common::{vision_bundle, write_bundle, BundleSpec, Harness};
use registry_agent::{RegistryAgent, STREAM_RANGE};
use serde_json::json;
use tensorplate_agent::error::{AgentError, AgentResult};
use tensorplate_agent::recovery;
use tensorplate_agent::{Coordinator, WorkerControl};
use tensorplate_protocol::agent_control::DeploymentSummary;
use tensorplate_protocol::agent_state::DeploymentRecord;
use tensorplate_protocol::{ErrorCode, FEATURE_DEPLOY_STREAMING};

fn speech_bundle(td: &Path, id: &str) -> PathBuf {
    let spec = BundleSpec {
        model_class: Some("speech"),
        ..BundleSpec::default()
    };
    write_bundle(td, id, spec)
}

fn deploy(agent: &RegistryAgent, id: &str, bundle: &Path, streaming: bool) -> AgentResult<()> {
    let deployed = agent
        .coord
        .deploy_with(id, bundle, BTreeMap::new(), None, None, streaming);
    deployed.map(|_| ())
}

fn active(agent: &RegistryAgent) -> DeploymentSummary {
    agent
        .coord
        .status()
        .unwrap()
        .active
        .expect("an active deployment")
}

fn recorded_active(agent: &RegistryAgent) -> DeploymentRecord {
    let state = agent.coord.state().snapshot().unwrap();
    state.active.expect("an active record")
}

/// Where `config` breaks the serving worker's configuration schema, which
/// the worker's parser mirrors.
fn worker_config_errors(config: &serde_json::Value) -> Vec<String> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../config/schemas/serving_worker.json");
    let schema = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let compiled = jsonschema::JSONSchema::compile(&schema).expect("the schema compiles");
    let errors = compiled.validate(config).err();
    errors.map_or_else(Vec::new, |errors| errors.map(|e| e.to_string()).collect())
}

/// The stream port of the range's pair whose HTTP port is `http`.
fn stream_port_of(http: u16) -> u16 {
    let pairs = STREAM_RANGE.pairs();
    let found = pairs.into_iter().find(|(h, _)| *h == http);
    found.expect("a pair of the range").1
}

fn assert_streams(agent: &RegistryAgent, id: &str) {
    let worker = agent.workers.last();
    let port = stream_port_of(worker.port);
    let digest = recorded_active(agent).bundle_digest;
    assert_eq!(
        worker.config["streaming"],
        json!({"enabled": true, "port": port})
    );
    assert_eq!(
        worker.config["deployment"]["descriptor_digest"],
        json!(digest)
    );
    assert_eq!(worker_config_errors(&worker.config), Vec::<String>::new());
    let active = active(agent);
    assert_eq!(active.deployment_id, id);
    assert!(active.generation.is_some());
    assert_eq!(active.stream_endpoint, Some(format!("127.0.0.1:{port}")));
    assert_eq!(active.descriptor_digest, Some(digest));
}

fn refused_as_config_invalid(result: AgentResult<()>) {
    match result {
        Err(err @ AgentError::Config(_)) => {
            assert_eq!(err.to_record().code, ErrorCode::ConfigInvalid);
        }
        other => panic!("expected a config_invalid refusal, got {other:?}"),
    }
}

/// Whether anything was staged for `id`.
fn staged(h: &Harness, id: &str) -> bool {
    h.config.staging_dir.join(id).exists()
}

#[test]
fn a_streaming_deploy_renders_its_stream_listener_and_status_reports_the_endpoint() {
    let h = Harness::new();
    let agent = RegistryAgent::start_with_stream_ports(&h);
    deploy(&agent, "stt", &speech_bundle(h.td.path(), "stt"), true).expect("deploy");

    assert!(recorded_active(&agent).streaming);
    assert_streams(&agent, "stt");
}

#[test]
fn a_deploy_that_does_not_ask_renders_and_reports_no_stream_listener() {
    let h = Harness::new();
    let agent = RegistryAgent::start_with_stream_ports(&h);
    deploy(&agent, "stt", &speech_bundle(h.td.path(), "stt"), false).expect("deploy");

    let worker = agent.workers.last();
    assert!(worker.config.get("streaming").is_none());
    assert!(worker.config["deployment"]
        .get("descriptor_digest")
        .is_none());
    assert_eq!(worker_config_errors(&worker.config), Vec::<String>::new());
    assert!(!recorded_active(&agent).streaming);
    let active = active(&agent);
    assert!(active.generation.is_some());
    assert_eq!(
        (active.stream_endpoint, active.descriptor_digest),
        (None, None)
    );
}

#[test]
fn a_rollback_to_a_streaming_deployment_streams_again_and_one_over_it_does_not() {
    let h = Harness::new();
    let agent = RegistryAgent::start_with_stream_ports(&h);
    deploy(&agent, "stt", &speech_bundle(h.td.path(), "stt"), true).expect("deploy");
    deploy(&agent, "tts", &speech_bundle(h.td.path(), "tts"), false).expect("deploy");
    assert!(agent.workers.last().config.get("streaming").is_none());
    assert_eq!(active(&agent).stream_endpoint, None);

    agent.reap_retired();
    agent.coord.rollback(None).expect("rollback");
    assert_streams(&agent, "stt");
}

#[test]
fn the_next_start_restores_a_streaming_deployment_with_its_stream_listener() {
    let h = Harness::new();
    let first = RegistryAgent::start_with_stream_ports(&h);
    deploy(&first, "stt", &speech_bundle(h.td.path(), "stt"), true).expect("deploy");
    first.registry.shutdown();

    let next = RegistryAgent::start_with_stream_ports(&h);
    recovery::apply_startup(&next.coord).expect("recovery");
    assert_eq!(next.workers.count(), 1, "the restore started one worker");
    assert_streams(&next, "stt");
}

#[test]
fn streaming_is_refused_before_staging_for_a_bundle_that_is_not_speech() {
    let h = Harness::new();
    let agent = RegistryAgent::start_with_stream_ports(&h);
    let refused = deploy(&agent, "det", &vision_bundle(h.td.path(), "det"), true);

    refused_as_config_invalid(refused);
    assert!(!staged(&h, "det"));
    assert_eq!(agent.workers.count(), 0);
    deploy(&agent, "det", &vision_bundle(h.td.path(), "det"), false).expect("unary deploy");
}

#[test]
fn streaming_is_refused_before_staging_by_an_agent_with_no_stream_ports() {
    let h = Harness::new();
    let agent = RegistryAgent::start_packaged(&h);
    let refused = deploy(&agent, "stt", &speech_bundle(h.td.path(), "stt"), true);

    refused_as_config_invalid(refused);
    assert!(!staged(&h, "stt"));
    assert_eq!(agent.workers.count(), 0);
    deploy(&agent, "stt", &speech_bundle(h.td.path(), "stt"), false).expect("unary deploy");
}

#[test]
fn the_agent_lists_streaming_only_where_its_configuration_gives_stream_ports() {
    let h = Harness::new();
    let features = |agent: RegistryAgent| agent.coord.status().unwrap().control_features;
    assert_eq!(
        features(RegistryAgent::start_with_stream_ports(&h)),
        [FEATURE_DEPLOY_STREAMING]
    );
    assert_eq!(
        features(RegistryAgent::start_packaged(&h)),
        Vec::<String>::new()
    );
    assert_eq!(features(RegistryAgent::start(&h)), Vec::<String>::new());
}

#[test]
fn an_agent_that_does_not_start_its_workers_refuses_streaming_and_does_not_list_it() {
    // A configuration in the default `mock` mode may name a range; nothing
    // starts a stream listener there.
    let h = Harness::new();
    let mut config = h.config.clone();
    config.worker.serving_port_range = Some(STREAM_RANGE);
    let coord = Coordinator::new(config, h.store.clone(), h.worker.clone());
    let bundle = speech_bundle(h.td.path(), "stt");
    let refused = coord.deploy_with("stt", &bundle, BTreeMap::new(), None, None, true);

    refused_as_config_invalid(refused.map(|_| ()));
    assert!(!staged(&h, "stt"));
    assert!(coord.status().unwrap().control_features.is_empty());
}
