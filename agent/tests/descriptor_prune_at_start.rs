// SPDX-License-Identifier: Apache-2.0
//
// The agent binary removes the descriptor documents and the staged trees no
// state record names when it starts, before its recovery writes the next
// document.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tensorplate_agent::coordinator::new_transaction_id;
use tensorplate_agent::descriptor_store::DescriptorStore;
use tensorplate_agent::state::StateStore;
use tensorplate_protocol::agent_state::DeploymentRecord;
use tensorplate_protocol::backend_descriptor::BackendDescriptor;
use tensorplate_protocol::{
    parse_bundle, AdmissionMode, DeploymentDescriptor, DescriptorInputs, MemberQuota,
};

fn repo(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(relative)
}

/// The speech-to-text fixture bundle staged under `dir`, and its descriptor
/// at `generation`.
fn staged_and_described(dir: &Path, generation: u64) -> DeploymentDescriptor {
    let fixture = repo("test/models/bundles/v0_2/speech_stt_streaming");
    let staged = dir.join("staging").join("stt");
    std::fs::create_dir_all(&staged).unwrap();
    for file in ["manifest.json", "entry.json"] {
        std::fs::copy(fixture.join(file), staged.join(file)).unwrap();
    }
    let profiles = repo("protocol/rust/tests/fixtures/backend_descriptor_runner_profiles.json");
    let text = std::fs::read_to_string(&profiles).unwrap();
    let installed = BackendDescriptor::parse_with_path(&text, &profiles).unwrap();
    let inputs = DescriptorInputs {
        deployment_id: "stt",
        generation,
        admission_mode: AdmissionMode::Production,
        staged_path: staged.to_str().unwrap(),
        quota: MemberQuota::default(),
        runtime_version: "0.3.1",
        runner_profiles: &installed.runner_profiles,
        acceptance_profile_digest: None,
    };
    DeploymentDescriptor::derive(&parse_bundle(&fixture).unwrap(), &inputs).unwrap()
}

/// A state whose `active` names `descriptor`, with its document.
fn record_active(state_dir: &Path, descriptor: &DeploymentDescriptor) {
    let store = StateStore::open(state_dir).unwrap();
    let configuration = &descriptor.configuration;
    let active = DeploymentRecord {
        deployment_id: descriptor.deployment_id.clone(),
        bundle_digest: configuration.bundle.bundle_digest.clone(),
        bundle_name: configuration.bundle.name.clone(),
        bundle_version: configuration.bundle.version.clone(),
        backend_hint: configuration.backend_hint.clone(),
        model_class: configuration.model_class.as_str().into(),
        staged_path: descriptor.staged_path.clone(),
        promoted_monotonic_ns: Some(1),
        labels: BTreeMap::new(),
        generation: Some(descriptor.generation),
        descriptor_digest: Some(descriptor.descriptor_digest.clone()),
        configuration_digest: Some(descriptor.configuration_digest.clone()),
    };
    let record = |state: &mut tensorplate_protocol::agent_state::AgentState| {
        assert_eq!(state.allocate_generation(1), Ok(descriptor.generation));
        state.active = Some(active);
        DescriptorStore::new(state_dir).write(descriptor)
    };
    store.update(record).unwrap();
}

/// Ends the agent however the test ends.
struct Started(std::process::Child);

impl Drop for Started {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn files(dir: &Path) -> Vec<String> {
    let entries = std::fs::read_dir(dir).unwrap();
    let mut names: Vec<_> = entries
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// Start the agent binary over `dir` and wait until it listens; returns it
/// with what it had written to stderr by then.
fn start_agent(dir: &Path) -> (Started, String) {
    let config = serde_json::json!({
        "socket_path": dir.join("agent.sock"),
        "state_dir": dir.join("state"),
        "staging_dir": dir.join("staging"),
        "available_backends": ["mock"],
        "device_memory_bytes": 8_u64 << 30,
    });
    let log = dir.join("agent.stderr");
    let agent = std::process::Command::new(env!("CARGO_BIN_EXE_tensorplate-agent"))
        .arg("--config-json")
        .arg(config.to_string())
        // Without a platform registry the start does not depend on the host.
        .env("TP_PLATFORM_REGISTRY_DIR", dir.join("no-registry"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let mut agent = Started(agent);
    let stderr = || std::fs::read_to_string(&log).unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !stderr().contains("tensorplate-agent listening on ") {
        let exited = agent.0.try_wait().unwrap();
        assert!(exited.is_none(), "exited: {exited:?}: {}", stderr());
        assert!(Instant::now() < deadline, "never listening: {}", stderr());
        std::thread::sleep(Duration::from_millis(20));
    }
    (agent, stderr())
}

#[test]
fn the_agent_removes_unnamed_documents_at_its_start_before_recovery_writes_the_next() {
    let td = TempDir::new().unwrap();
    let state_dir = td.path().join("state");
    let first = staged_and_described(td.path(), 1);
    record_active(&state_dir, &first);
    let unnamed = first.restamp(7).unwrap();
    DescriptorStore::new(&state_dir).write(&unnamed).unwrap();
    std::fs::write(state_dir.join("descriptor-stt-8.json.tmp"), "{").unwrap();
    std::fs::write(state_dir.join("descriptor-notes.txt"), "kept").unwrap();
    // Trees a deploy staged and no record names: one inside the tree of the
    // layout before trees of their own, which `active` names, and one of
    // another id.
    let kept_tree = td.path().join("staging/stt").join(new_transaction_id());
    let unnamed_tree = td.path().join("staging/tts").join(new_transaction_id());
    for tree in [&kept_tree, &unnamed_tree] {
        std::fs::create_dir_all(tree).unwrap();
        std::fs::write(tree.join("manifest.json"), "{}").unwrap();
    }

    let (_agent, stderr) = start_agent(td.path());

    // Generation 1 was named when the prune ran, so it is still there
    // beside the generation the restore then started.
    assert!(
        stderr.contains("startup recovery: RestoreActive"),
        "{stderr}"
    );
    let left = [
        "descriptor-notes.txt",
        "descriptor-stt-1.json",
        "descriptor-stt-2.json",
        "state.json",
        "state.json.bak",
    ];
    assert_eq!(files(&state_dir), left, "{stderr}");
    let says = "deployment descriptors: 2 removed that no state record names";
    assert!(stderr.contains(says), "{stderr}");
    assert!(!unnamed_tree.exists(), "{stderr}");
    let kept = kept_tree.file_name().unwrap().to_str().unwrap();
    assert_eq!(
        files(&td.path().join("staging/stt")),
        ["entry.json", "manifest.json", kept]
    );
    let says = "staged bundles: 1 removed that no state record names";
    assert!(stderr.contains(says), "{stderr}");
}

#[test]
fn a_prune_that_fails_is_logged_and_does_not_stop_the_start() {
    let td = TempDir::new().unwrap();
    let state_dir = td.path().join("state");
    StateStore::open(&state_dir)
        .unwrap()
        .update(|_| Ok(()))
        .unwrap();
    std::fs::write(state_dir.join("descriptor-stt-7.json"), "{").unwrap();
    let unnamed_tree = td.path().join("staging/stt").join(new_transaction_id());
    std::fs::create_dir_all(&unnamed_tree).unwrap();
    // A backup the agent cannot read: it cannot tell what that file names.
    std::fs::remove_file(state_dir.join("state.json.bak")).unwrap();
    std::fs::create_dir(state_dir.join("state.json.bak")).unwrap();

    let (_agent, stderr) = start_agent(td.path());

    let says = "deployment descriptors: unnamed documents were not removed: ";
    assert!(stderr.contains(says), "{stderr}");
    assert!(
        state_dir.join("descriptor-stt-7.json").is_file(),
        "{stderr}"
    );
    let says = "staged bundles: not all unnamed copies were removed: ";
    assert!(stderr.contains(says) && unnamed_tree.is_dir(), "{stderr}");
}
