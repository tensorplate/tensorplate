// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use std::collections::BTreeMap;
use std::path::Component;
use std::sync::Arc;

use tempfile::TempDir;
use tensorplate_protocol::agent_state::{decode_agent_state, DeploymentRecord};
use tensorplate_protocol::backend_descriptor::BackendDescriptor;
use tensorplate_protocol::{parse_bundle, AdmissionMode, DescriptorInputs, MemberQuota};

use crate::backend_detection::{BackendProbeState, RunnerProfileProbe};
use crate::config::AgentConfig;
use crate::state::StateStore;

mod deploy;
mod one_pass;
mod staged_trees;
mod start_again;

const STAGED: &str = "/var/lib/tensorplate/bundles/staging/stt";
/// The runtime version the agents of these tests are configured with. It
/// is not the one they run, which is what a descriptor states.
pub(crate) const CONFIGURED_RUNTIME: &str = "0.3.9";
const ELSEWHERE: &str = "/var/lib/tensorplate/bundles/staging/other";

fn repo(relative: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.join("..").join(relative)
}

/// The descriptor of one generation of the speech-to-text fixture bundle,
/// derived against the committed runner profiles.
pub(crate) fn derive(id: &str, generation: u64, staged_path: &str) -> DeploymentDescriptor {
    let profiles = repo("protocol/rust/tests/fixtures/backend_descriptor_runner_profiles.json");
    let text = fs::read_to_string(&profiles).unwrap();
    let installed = BackendDescriptor::parse_with_path(&text, &profiles).unwrap();
    let bundle = parse_bundle(&repo("test/models/bundles/v0_2/speech_stt_streaming")).unwrap();
    let inputs = DescriptorInputs {
        deployment_id: id,
        generation,
        admission_mode: AdmissionMode::Production,
        staged_path,
        quota: MemberQuota::default(),
        runtime_version: tensorplate_protocol::version(),
        runner_profiles: &installed.runner_profiles,
        acceptance_profile_digest: None,
    };
    DeploymentDescriptor::derive(&bundle, &inputs).unwrap()
}

/// The probe report of an agent on which the committed runner profiles are
/// installed and runnable: what [`derive`] derives against.
pub(crate) fn probes() -> BTreeMap<String, BackendProbeReport> {
    let path = repo("protocol/rust/tests/fixtures/backend_descriptor_runner_profiles.json");
    let text = fs::read_to_string(&path).unwrap();
    let installed = BackendDescriptor::parse_with_path(&text, &path).unwrap();
    let runnable = |profile| RunnerProfileProbe {
        profile,
        state: BackendProbeState::Runnable,
        observed: None,
    };
    let report = BackendProbeReport {
        backend_name: "python_pytorch".into(),
        descriptor_path: path,
        state: BackendProbeState::Runnable,
        install_hint: None,
        runner_profiles: installed
            .runner_profiles
            .into_iter()
            .map(runnable)
            .collect(),
    };
    BTreeMap::from([("python_pytorch".to_owned(), report)])
}

/// The configuration of an agent that keeps its state and its staged bundles
/// under `root` and can serve the fixture bundles.
pub(crate) fn agent_config(root: &Path) -> AgentConfig {
    let served = serde_json::json!({
        "supported_precision": ["fp32", "fp16"],
        "supported_artifact_kinds": ["python_pytorch_entry"],
    });
    let config = serde_json::json!({
        "socket_path": root.join("agent.sock"),
        "state_dir": root.join("state"),
        "staging_dir": root.join("staging"),
        "available_backends": ["mock", "python_pytorch"],
        "backend_capabilities": {"python_pytorch": served},
        "device_memory_bytes": 8_u64 << 30,
        "device_family": "x86_64",
        "runtime_version": CONFIGURED_RUNTIME,
    });
    let config: AgentConfig = serde_json::from_value(config).unwrap();
    config.validate().unwrap()
}

/// The reference a deploy hands the registry for `descriptor`.
pub(crate) fn reference(descriptor: &DeploymentDescriptor) -> CandidateRef {
    let configuration = &descriptor.configuration;
    CandidateRef {
        deployment_id: descriptor.deployment_id.clone(),
        staged_path: descriptor.staged_path.clone(),
        bundle_digest: configuration.bundle.bundle_digest.clone(),
        backend_hint: configuration.backend_hint.clone(),
        model_class: configuration.model_class.as_str().into(),
        bundle_name: Some(configuration.bundle.name.clone()),
        bundle_version: Some(configuration.bundle.version.clone()),
        artifact_relative_path: Some("entry.json".into()),
        generation: Some(descriptor.generation),
        descriptor_digest: Some(descriptor.descriptor_digest.clone()),
        configuration_digest: Some(descriptor.configuration_digest.clone()),
    }
}

/// `reference` keeping only the identity fields `kept` marks, in the order
/// generation, descriptor digest, configuration digest.
pub(crate) fn naming(reference: &CandidateRef, kept: [bool; 3]) -> CandidateRef {
    CandidateRef {
        generation: reference.generation.filter(|_| kept[0]),
        descriptor_digest: reference.descriptor_digest.clone().filter(|_| kept[1]),
        configuration_digest: reference.configuration_digest.clone().filter(|_| kept[2]),
        ..reference.clone()
    }
}

fn record(descriptor: &DeploymentDescriptor) -> DeploymentRecord {
    let named = reference(descriptor);
    DeploymentRecord {
        deployment_id: named.deployment_id,
        bundle_digest: named.bundle_digest,
        bundle_name: named.bundle_name.unwrap(),
        bundle_version: named.bundle_version.unwrap(),
        backend_hint: named.backend_hint,
        model_class: named.model_class,
        staged_path: named.staged_path,
        promoted_monotonic_ns: Some(1),
        labels: BTreeMap::new(),
        generation: named.generation,
        descriptor_digest: named.descriptor_digest,
        configuration_digest: named.configuration_digest,
    }
}

/// What a deploy that records a descriptor does, by hand: one update takes
/// the generation, names the descriptor on `active` and writes its document.
pub(crate) fn deploy(
    store: &StateStore,
    descriptors: &DescriptorStore,
    id: &str,
    staged_path: &str,
) -> AgentResult<DeploymentDescriptor> {
    store.update(|state| {
        let generation = state.allocate_generation(1).unwrap();
        let descriptor = derive(id, generation, staged_path);
        state.active = Some(record(&descriptor));
        descriptors.write(&descriptor)?;
        Ok(descriptor)
    })
}

struct Fixture {
    td: TempDir,
    store: Arc<StateStore>,
    descriptors: Arc<DescriptorStore>,
}

fn fixture() -> Fixture {
    let td = TempDir::new().unwrap();
    Fixture {
        store: Arc::new(StateStore::open(td.path()).unwrap()),
        descriptors: Arc::new(DescriptorStore::new(td.path())),
        td,
    }
}

impl Fixture {
    fn path(&self, name: &str) -> PathBuf {
        self.td.path().join(name)
    }
    fn names(&self) -> Vec<String> {
        let entries = fs::read_dir(self.td.path()).unwrap();
        let mut names: Vec<_> = entries
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }
    fn stop_at(&self, step: Option<Step>) {
        *self.descriptors.fail_at.lock().unwrap() = step;
    }
    /// The state as the next agent start reads it.
    fn on_disk(&self) -> AgentState {
        let read = StateStore::open(self.td.path()).unwrap();
        read.snapshot().unwrap()
    }
    /// Puts a directory where a state write makes its first temporary file,
    /// so the write fails before any rename, or takes it away again.
    fn refuse_state_writes(&self, refuse: bool) {
        let in_the_way = self.path("state.json.bak.tmp");
        if refuse {
            fs::create_dir(in_the_way).unwrap();
        } else {
            fs::remove_dir(in_the_way).unwrap();
        }
    }
    fn read(&self, identity: DescriptorIdentity<'_>) -> AgentResult<DeploymentDescriptor> {
        self.descriptors.read("stt", identity)
    }
}

/// The message of a refusal, which is `load_failed` and not recoverable.
fn refusal(result: AgentResult<impl std::fmt::Debug>) -> String {
    let error = result.expect_err("refused");
    let record = error.to_record();
    let typed = matches!(error, AgentError::DeploymentDescriptor { .. });
    assert!(typed && record.code == ErrorCode::LoadFailed && !record.recoverable);
    record.message
}

#[test]
fn a_document_name_is_one_path_segment_that_tells_its_deployment_and_generation_apart() {
    let longest = "a".repeat(128);
    let mut seen = BTreeSet::new();
    for id in ["stt", ".hidden", "..a", "a-1", "a", "A_b.c-9", &longest] {
        for generation in [1, 12, u64::MAX] {
            let name = file_name(id, generation).expect(id);
            let segments: Vec<_> = Path::new(&name).components().collect();
            assert!(matches!(segments[..], [Component::Normal(_)]), "{name}");
            assert!(name.starts_with("descriptor-") && name.len() + UNFINISHED.len() <= 255);
            assert!(is_document(&name) && is_document(&format!("{name}{UNFINISHED}")));
            assert!(seen.insert(name));
        }
    }
    for id in ["", ".", "..", "a/b", "a\\b", "a b", "a\0", &"a".repeat(129)] {
        assert_eq!(file_name(id, 3), None, "{id:?}");
    }
}

#[test]
fn nothing_is_created_before_the_first_document_and_none_for_an_id_that_is_not_one() {
    let td = TempDir::new().unwrap();
    let absent = DescriptorStore::new(td.path().join("state"));
    let descriptor = derive("stt", 3, STAGED);
    assert_eq!(absent.prune(&AgentState::fresh()).unwrap(), 0);
    refusal(absent.read("stt", descriptor.identity()));
    assert!(!td.path().join("state").exists());

    let f = fixture();
    let message = refusal(f.descriptors.read("../stt", descriptor.identity()));
    assert!(
        message.contains("`../stt` is not a deployment id"),
        "{message}"
    );
    let mut escaping = descriptor.clone();
    escaping.deployment_id = "../stt".into();
    refusal(f.descriptors.write(&escaping));
    let mut changed = descriptor.clone();
    changed.staged_path = ELSEWHERE.into();
    let message = refusal(f.descriptors.write(&changed));
    assert!(message.contains("is not written"), "{message}");
    assert_eq!(f.descriptors.prune(&f.on_disk()).unwrap(), 0);
    assert_eq!(f.names(), [""; 0]);

    f.descriptors.write(&descriptor).unwrap();
    assert_eq!(f.names(), ["descriptor-stt-3.json"]);
}

#[test]
fn a_document_reads_back_only_as_the_descriptor_its_identity_names() {
    let f = fixture();
    let named = derive("stt", 3, STAGED);
    let path = f.path("descriptor-stt-3.json");
    let message = refusal(f.read(named.identity()));
    let missing = format!("deployment descriptor `{}` is missing", path.display());
    assert_eq!(message, missing);

    f.descriptors.write(&named).unwrap();
    assert_eq!(f.read(named.identity()).unwrap(), named);
    let written = fs::read_to_string(&path).unwrap();
    assert_eq!(DeploymentDescriptor::from_json(&written).unwrap(), named);

    let edited = written.replace(tensorplate_protocol::version(), "0.0.0");
    assert_ne!(edited, written);
    for damaged in ["{".to_owned(), edited] {
        fs::write(&path, damaged).unwrap();
        let message = refusal(f.read(named.identity()));
        assert!(message.contains("does not verify"), "{message}");
    }
    let others = [
        named.restamp(4).unwrap(),
        derive("stt", 3, ELSEWHERE),
        derive("tts", 3, STAGED),
    ];
    for other in &others {
        fs::write(&path, serde_json::to_vec(other).unwrap()).unwrap();
        let message = refusal(f.read(named.identity()));
        let both = [&other.descriptor_digest, &named.descriptor_digest];
        assert!(
            both.iter().all(|digest| message.contains(*digest)),
            "{message}"
        );
    }
    // Another deployment's descriptor, asked for by its own identity.
    let message = refusal(f.read(others[2].identity()));
    assert!(message.contains("is generation 3 of `tts`"), "{message}");
    f.descriptors.write(&named).unwrap();
    let mut identity = named.identity();
    identity.configuration_digest = &others[1].descriptor_digest;
    refusal(f.read(identity));
}

/// The message of a read refused because the document does not verify.
fn unverified(f: &Fixture, named: &DeploymentDescriptor) -> String {
    let message = refusal(f.read(named.identity()));
    assert!(message.contains("does not verify: "), "{message}");
    message
}

#[test]
fn a_document_that_is_not_utf8_is_a_directory_or_is_over_the_bound_does_not_verify() {
    let f = fixture();
    let named = derive("stt", 3, STAGED);
    let path = f.path("descriptor-stt-3.json");
    let whole = serde_json::to_vec_pretty(&named).unwrap();

    fs::write(&path, [&whole[..], b"\xff"].concat()).unwrap();
    let message = unverified(&f, &named);
    assert!(message.contains("it is not UTF-8"), "{message}");

    // Trailing spaces keep it the same descriptor: only its size changes.
    let bound = usize::try_from(MAX_DOCUMENT_BYTES).unwrap();
    let mut padded = whole;
    padded.resize(bound, b' ');
    fs::write(&path, &padded).unwrap();
    assert_eq!(f.read(named.identity()).unwrap(), named);
    padded.push(b' ');
    fs::write(&path, &padded).unwrap();
    let message = unverified(&f, &named);
    assert!(message.contains("more than 1048576 bytes"), "{message}");

    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let message = unverified(&f, &named);
    assert!(message.contains("it is not a regular file"), "{message}");
}

#[cfg(unix)]
#[test]
fn a_document_the_agent_may_not_read_does_not_verify_and_says_why() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        eprintln!("skipped: root reads a file of mode 000");
        return;
    }
    let f = fixture();
    let named = derive("stt", 3, STAGED);
    f.descriptors.write(&named).unwrap();
    let path = f.path("descriptor-stt-3.json");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    let denied = File::open(&path).unwrap_err().to_string();

    let message = unverified(&f, &named);

    let says = format!("it cannot be read: {denied}");
    assert!(message.contains(&says), "{message}");
}

#[cfg(unix)]
#[test]
fn a_fifo_or_a_symlink_at_a_documents_name_is_refused_without_being_opened() {
    let f = fixture();
    let named = derive("stt", 3, STAGED);
    let path = f.path("descriptor-stt-3.json");

    // A symlink to the very document is not the document.
    let elsewhere = f.path("elsewhere.json");
    fs::write(&elsewhere, serde_json::to_vec_pretty(&named).unwrap()).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
    let message = unverified(&f, &named);
    assert!(message.contains("it is not a regular file"), "{message}");
    fs::remove_file(&path).unwrap();

    // Opening a FIFO nothing writes to never returns, so the read runs on a
    // thread of its own and a read that blocks is let go before the test fails.
    let made = std::process::Command::new("mkfifo").arg(&path).status();
    assert!(made.expect("mkfifo(1) runs").success());
    let (answered, answer) = std::sync::mpsc::channel();
    let (descriptors, identity) = (f.descriptors.clone(), named.clone());
    let reader = std::thread::spawn(move || {
        let read = descriptors.read("stt", identity.identity());
        let _ = answered.send(read.map_err(|e| e.to_record()));
    });
    let read = answer.recv_timeout(std::time::Duration::from_secs(5));
    if read.is_err() {
        let _released = fs::OpenOptions::new().write(true).open(&path);
    }
    reader.join().unwrap();
    let refused = read.expect("the read returns at once").unwrap_err();
    assert!(refused.code == ErrorCode::LoadFailed && !refused.recoverable);
    let says = "does not verify: it is not a regular file";
    assert!(refused.message.contains(says), "{refused:?}");
}

#[test]
fn a_write_that_stops_leaves_the_document_it_replaces_or_none_and_no_temporary_file() {
    let (old, new) = (derive("stt", 3, STAGED), derive("stt", 3, ELSEWHERE));
    let whole = serde_json::to_vec_pretty(&new).unwrap();
    let steps = [Step::SyncFile, Step::Rename, Step::SyncDirectory];
    for (step, replaces) in steps.into_iter().flat_map(|s| [(s, false), (s, true)]) {
        let f = fixture();
        if replaces {
            f.descriptors.write(&old).unwrap();
        }
        f.stop_at(Some(step));

        let error = f.descriptors.write(&new).unwrap_err();

        let at = f.path("descriptor-stt-3.json");
        let names_it = error.to_string().contains(&at.display().to_string());
        assert!(matches!(error, AgentError::Io(_)) && names_it, "{error}");
        if step == Step::SyncDirectory {
            assert_eq!(f.names(), ["descriptor-stt-3.json"], "{step:?}");
            assert_eq!(fs::read(&at).unwrap(), whole, "{step:?}");
        } else {
            let left = replaces.then_some("descriptor-stt-3.json");
            assert_eq!(f.names(), left.into_iter().collect::<Vec<_>>(), "{step:?}");
            let kept = f.read(old.identity()).ok();
            assert_eq!(kept, replaces.then(|| old.clone()), "{step:?}");
        }

        f.stop_at(None);
        f.descriptors.write(&new).unwrap();
        assert_eq!(f.names(), ["descriptor-stt-3.json"]);
        assert_eq!(f.read(new.identity()).unwrap(), new);
    }
}

#[test]
fn a_write_syncs_the_directory_it_renames_the_document_into_and_fails_when_it_cannot() {
    let f = fixture();
    let new = derive("stt", 3, STAGED);
    drop(crate::durable::hooks::synced());

    f.descriptors.write(&new).unwrap();

    assert_eq!(crate::durable::hooks::synced(), [f.td.path()]);
    let dir = f.td.path().to_path_buf();
    crate::durable::hooks::fail_next_sync_of(move |path| path == dir);
    let error = f.descriptors.write(&new).unwrap_err();
    assert!(matches!(error, AgentError::Io(_)), "{error}");
}

#[test]
fn a_state_write_that_fails_after_the_document_leaves_a_document_no_record_names() {
    let f = fixture();
    let before = f.store.snapshot().unwrap();
    f.refuse_state_writes(true);

    let error = deploy(&f.store, &f.descriptors, "stt", STAGED).unwrap_err();

    assert!(matches!(error, AgentError::Io(_)) && !f.store.is_indeterminate());
    let first = derive("stt", 1, STAGED);
    assert_eq!(f.read(first.identity()).unwrap(), first);
    // As after a kill between the two writes: no generation was spent and
    // nothing names the document.
    f.refuse_state_writes(false);
    assert_eq!(f.on_disk(), before);
    assert_eq!(before.next_generation, None);

    // The next attempt is handed the same generation and replaces the file.
    let second = deploy(&f.store, &f.descriptors, "stt", ELSEWHERE).unwrap();
    assert_eq!(second.generation, first.generation);
    assert_eq!(f.read(second.identity()).unwrap(), second);
    refusal(f.read(first.identity()));
    let named = f.on_disk().active.unwrap();
    assert_eq!(named.descriptor_identity(), Ok(Some(second.identity())));
    assert_eq!(f.descriptors.prune(&f.on_disk()).unwrap(), 0);
}

/// Not documents: prune leaves every one of them.
const OTHERS: &str = "state.json state.json.bak machine-type.json serving-speech-stt-3.json \
    descriptor-notes.txt descriptor-speech-stt-03.json descriptor-speech-stt-+3.json \
    descriptor-speech-stt-.json descriptor--3.json descriptor-a+b-3.json \
    descriptor-speech-stt-3.json.bak descriptor-speech-stt-3.json.tmp.tmp";
/// A directory with a document's name, which prune leaves too.
const DIRECTORY: &str = "descriptor-speech-stt-9.json";

/// A state directory holding `documents` and everything that is not one.
fn directory_with(documents: &[&str]) -> Fixture {
    let f = fixture();
    for name in documents.iter().copied().chain(OTHERS.split_whitespace()) {
        fs::write(f.path(name), "{}").unwrap();
    }
    fs::create_dir(f.path(DIRECTORY)).unwrap();
    f
}

/// The documents left, after checking that nothing else was removed.
fn left(f: &Fixture) -> Vec<String> {
    let others: Vec<_> = OTHERS.split_whitespace().chain([DIRECTORY]).collect();
    let is_other = |name: &String| others.contains(&name.as_str());
    let (kept, documents): (Vec<_>, Vec<_>) = f.names().into_iter().partition(is_other);
    assert_eq!(kept.len(), others.len(), "{kept:?}");
    documents
}

fn committed_state(name: &str) -> AgentState {
    let path = repo("protocol/rust/tests/fixtures").join(name);
    decode_agent_state(&fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn prune_keeps_the_documents_the_singleton_records_and_the_transaction_name() {
    let mut state = committed_state("agent_state_0_2_descriptor_identity.json");
    let digest = state.active.as_ref().unwrap().descriptor_digest.clone();
    let previous = state.previous_active.as_mut().unwrap();
    previous.generation = Some(1);
    previous.descriptor_digest.clone_from(&digest);
    previous.configuration_digest = digest;
    state.in_flight_transaction.as_mut().unwrap().generation = Some(4);
    let named = [
        "descriptor-speech-stt-3.json",
        "descriptor-speech-tts-4.json",
        "descriptor-speech-tts-5.json",
        "descriptor-vision-v2-1.json",
    ];
    let unnamed = [
        "descriptor-speech-stt-2.json",
        "descriptor-speech-stt-4.json",
        "descriptor-speech-stt-4.json.tmp",
        "descriptor-speech-tts-5.json.tmp",
        "descriptor-vision-v2-3.json",
    ];
    let f = directory_with(&[&named[..], &unnamed[..]].concat());

    assert_eq!(f.descriptors.prune(&state).unwrap(), unnamed.len());
    assert_eq!(left(&f), named);

    // Each record keeps its own document.
    for (slot, lost) in named.into_iter().enumerate() {
        let mut state = state.clone();
        match slot {
            0 => state.active = None,
            1 => state.in_flight_transaction = None,
            2 => state.candidate = None,
            _ => state.previous_active = None,
        }
        assert_eq!(f.descriptors.prune(&state).unwrap(), 1, "{lost}");
        assert!(!f.path(lost).exists(), "{lost}");
        fs::write(f.path(lost), "{}").unwrap();
    }
    let fresh = AgentState::fresh();
    assert_eq!(f.descriptors.prune(&fresh).unwrap(), named.len());
    assert_eq!(left(&f), [""; 0]);
}

#[test]
fn prune_keeps_the_documents_of_resident_set_members_and_the_generations_they_retain() {
    let mut state = committed_state("agent_state_0_2_two_member_set.json");
    let named = [
        "descriptor-speech-stt-3.json",
        "descriptor-speech-tts-4.json",
        "descriptor-speech-tts-5.json",
    ];
    let f = directory_with(&[&named[..], &["descriptor-speech-stt-2.json"]].concat());

    assert_eq!(f.descriptors.prune(&state).unwrap(), 1);
    assert_eq!(left(&f), named);

    state.resident_set.as_mut().unwrap().members[1].previous = None;
    assert_eq!(f.descriptors.prune(&state).unwrap(), 1);
    assert_eq!(left(&f), [named[0], named[2]]);
}

#[test]
fn prune_removes_nothing_when_what_a_record_names_cannot_be_told() {
    let whole = record(&derive("stt", 3, STAGED));
    let mut partial = whole.clone();
    partial.descriptor_digest = None;
    let mut unnameable = whole;
    unnameable.deployment_id = "a/b".into();
    for active in [partial, unnameable] {
        let f = directory_with(&["descriptor-stt-2.json", "descriptor-stt-3.json"]);
        let mut state = AgentState::fresh();
        state.active = Some(active);
        f.descriptors.prune(&state).unwrap_err();
        assert_eq!(left(&f).len(), 2);
        // The record alone stood in the way.
        state.active = None;
        assert_eq!(f.descriptors.prune(&state).unwrap(), 2);
    }
}

#[test]
fn a_candidate_resolves_to_its_document_only_as_the_generation_it_names() {
    let f = fixture();
    let descriptor = derive("stt", 3, STAGED);
    f.descriptors.write(&descriptor).unwrap();
    let store = Some(&*f.descriptors);
    let named = reference(&descriptor);

    assert_eq!(resolve(store, &named, 3).unwrap(), Some(descriptor));
    let message = refusal(resolve(store, &named, 4));
    let other_generation = "`stt` generation 3 is not that of generation 4";
    assert!(message.contains(other_generation), "{message}");

    let unnamed = naming(&named, [false; 3]);
    assert_eq!(resolve(store, &unnamed, 9).unwrap(), None);
    assert_eq!(resolve(None, &unnamed, 9).unwrap(), None);
}

#[test]
fn a_candidate_resolves_to_its_document_only_when_it_states_what_the_descriptor_states() {
    let f = fixture();
    let descriptor = derive("stt", 3, STAGED);
    f.descriptors.write(&descriptor).unwrap();
    let store = Some(&*f.descriptors);
    let named = reference(&descriptor);

    // A digest differs in case only: the same bundle.
    let mut shouting = named.clone();
    shouting.bundle_digest = named
        .bundle_digest
        .to_uppercase()
        .replace("SHA256", "sha256");
    assert_ne!(shouting.bundle_digest, named.bundle_digest);
    assert_eq!(resolve(store, &shouting, 3).unwrap(), Some(descriptor));

    let zero = format!("sha256:{}", "0".repeat(64));
    let stated = [
        ("bundle_digest", zero.as_str()),
        ("staged_path", ELSEWHERE),
        ("model_class", "vision"),
        ("backend_hint", "mock"),
    ];
    for (field, other) in stated {
        let mut contradicting = named.clone();
        let changed = match field {
            "bundle_digest" => &mut contradicting.bundle_digest,
            "staged_path" => &mut contradicting.staged_path,
            "model_class" => &mut contradicting.model_class,
            _ => &mut contradicting.backend_hint,
        };
        other.clone_into(changed);
        let message = refusal(resolve(store, &contradicting, 3));
        let says = format!("of `stt` generation 3 is not the candidate's: its `{field}` is `");
        assert!(message.contains(&says), "{message}");
        let theirs = format!("and the reference's is `{other}`");
        assert!(message.contains(&theirs), "{message}");
    }
}

/// Resolve a reference to the descriptor of `stt` generation 3 that carries
/// what `edit` leaves of the fields a reference may omit.
fn resolved(edit: fn(&mut CandidateRef)) -> AgentResult<Option<DeploymentDescriptor>> {
    let f = fixture();
    let descriptor = derive("stt", 3, STAGED);
    f.descriptors.write(&descriptor).unwrap();
    let mut carried = reference(&descriptor);
    edit(&mut carried);
    resolve(Some(&*f.descriptors), &carried, 3)
}

/// The refusal of a reference whose `field` is `other`, not the descriptor's.
fn contradicted(
    field: &str,
    described: &str,
    other: &str,
    result: AgentResult<impl std::fmt::Debug>,
) {
    let message = refusal(result);
    let says = format!(
        "is not the candidate's: its `{field}` is `{described}`, and the reference's is `{other}`"
    );
    assert!(message.contains(&says), "{message}");
}

#[test]
fn a_candidate_that_carries_none_of_the_optional_fields_resolves() {
    let none = resolved(|carried| {
        carried.artifact_relative_path = None;
        carried.bundle_name = None;
        carried.bundle_version = None;
    });
    assert!(none.unwrap().is_some());
}

#[test]
fn a_candidate_that_carries_another_artifact_path_than_the_descriptors_model_is_refused() {
    let result = resolved(|carried| carried.artifact_relative_path = Some("absent.bin".into()));
    contradicted("artifact_relative_path", "entry.json", "absent.bin", result);
}

#[test]
fn a_candidate_that_carries_another_bundle_name_than_the_descriptors_is_refused() {
    let described = derive("stt", 3, STAGED).configuration.bundle.name;
    let result = resolved(|carried| carried.bundle_name = Some("another".into()));
    contradicted("bundle_name", &described, "another", result);
}

#[test]
fn a_candidate_that_carries_another_bundle_version_than_the_descriptors_is_refused() {
    let described = derive("stt", 3, STAGED).configuration.bundle.version;
    let result = resolved(|carried| carried.bundle_version = Some("9.9.9".into()));
    contradicted("bundle_version", &described, "9.9.9", result);
}

#[cfg(unix)]
mod stamp {
    use super::*;
    use crate::registry::{durable_generations, Restamp};

    /// Stamp the restart of `recipe` that replaces generation `stopped`, as
    /// the next generation; returns that generation with the answer.
    fn stamp(f: &Fixture, recipe: &CandidateRef, stopped: u64) -> (u64, AgentResult<CandidateRef>) {
        let generation = durable_generations(f.store.clone())().unwrap();
        let stamp = restart_stamp(f.store.clone(), f.descriptors.clone());
        let stamped = stamp(&Restamp {
            recipe,
            stopped_generation: stopped,
            generation,
        });
        (generation, stamped)
    }

    /// The generation and descriptor digest `active` names, as a start reads it.
    fn named_on_disk(f: &Fixture) -> (u64, String) {
        let active = f.on_disk().active.unwrap();
        let identity = active.descriptor_identity().unwrap().unwrap();
        (identity.generation, identity.descriptor_digest.to_owned())
    }

    #[test]
    fn the_stamp_refuses_a_restart_that_replaces_another_generation_than_the_recipes() {
        let f = fixture();
        let first = deploy(&f.store, &f.descriptors, "stt", STAGED).unwrap();

        let (_, refused) = stamp(&f, &reference(&first), 7);

        let message = refusal(refused);
        let says = "descriptor-stt-1.json` is not re-stamped: the restart replaces generation 7";
        assert!(message.contains(says), "{message}");
        assert_eq!(named_on_disk(&f), (1, first.descriptor_digest));
        let names = ["descriptor-stt-1.json", "state.json", "state.json.bak"];
        assert_eq!(f.names(), names);
    }

    #[test]
    fn the_stamp_refuses_a_recipe_of_another_bundle_than_its_descriptor_describes() {
        let f = fixture();
        let first = deploy(&f.store, &f.descriptors, "stt", STAGED).unwrap();
        let mut another = reference(&first);
        another.bundle_digest = format!("sha256:{}", "0".repeat(64));

        let (_, refused) = stamp(&f, &another, 1);

        let message = refusal(refused);
        let described = &first.configuration.bundle.bundle_digest;
        let says = format!("is not re-stamped: it describes bundle `{described}`");
        assert!(message.contains(&says), "{message}");
        assert!(message.contains(&another.bundle_digest), "{message}");
        assert_eq!(named_on_disk(&f), (1, first.descriptor_digest));
        let names = ["descriptor-stt-1.json", "state.json", "state.json.bak"];
        assert_eq!(f.names(), names);
    }

    #[test]
    fn the_stamp_writes_its_document_inside_the_update_that_names_it() {
        let f = fixture();
        let first = deploy(&f.store, &f.descriptors, "stt", STAGED).unwrap();
        let recipe = reference(&first);
        let unstamped = (1, first.descriptor_digest.clone());

        // The document cannot be written: nothing is named.
        f.stop_at(Some(Step::Rename));
        let (_, failed) = stamp(&f, &recipe, 1);
        assert!(matches!(failed, Err(AgentError::Io(_))), "{failed:?}");
        assert_eq!(f.store.snapshot().unwrap().active, f.on_disk().active);
        assert_eq!(named_on_disk(&f), unstamped);
        f.stop_at(None);

        // The state cannot be written: the document is there, unnamed.
        f.refuse_state_writes(true);
        let again = restart_stamp(f.store.clone(), f.descriptors.clone());
        let failed = again(&Restamp {
            recipe: &recipe,
            stopped_generation: 1,
            generation: 2,
        });
        assert!(matches!(failed, Err(AgentError::Io(_))), "{failed:?}");
        f.refuse_state_writes(false);
        let unnamed = first.restamp(2).unwrap();
        assert_eq!(f.read(unnamed.identity()).unwrap(), unnamed);
        assert_eq!(named_on_disk(&f), unstamped);

        // The next attempt starts from the same record. A start then prunes
        // the stopped generation's document and the unnamed one.
        let (generation, stamped) = stamp(&f, &recipe, 1);
        let third = first.restamp(generation).unwrap();
        let returned = stamped.unwrap();
        assert_eq!(identity_of(&returned).unwrap(), Some(third.identity()));
        assert_eq!(naming(&returned, [false; 3]), naming(&recipe, [false; 3]));
        assert_eq!(named_on_disk(&f), (3, third.descriptor_digest));
        assert_eq!(f.descriptors.prune(&f.on_disk()).unwrap(), 2);
        let names = ["descriptor-stt-3.json", "state.json", "state.json.bak"];
        assert_eq!(f.names(), names);
    }
}
