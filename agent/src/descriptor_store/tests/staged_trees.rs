// SPDX-License-Identifier: Apache-2.0

use super::deploy::{started_after_an_update_that_did_not_complete, FORMAT_0_1, STT, TTS};
use super::one_pass::another_format_0_1_bundle;
use super::start_again::{host, Host, Worker};
use super::*;

use crate::recovery::apply_startup;

/// The digest of the bundle staged where `reference` says.
fn staged_digest(reference: &CandidateRef) -> String {
    let staged = crate::bundle::staged_manifest(Path::new(&reference.staged_path));
    staged.unwrap().bundle_digest
}

fn digest_of(bundle: &str) -> String {
    parse_bundle(&repo(bundle)).unwrap().manifest_digest
}

#[test]
fn a_rollback_after_a_deploy_of_another_bundle_under_the_same_id_starts_the_earlier_bundle() {
    let host = host();
    let (agent, _) = host.start(Worker::default());
    agent.deploy("stt", STT).unwrap();
    drop(agent.asked());
    agent.deploy("stt", TTS).unwrap();
    drop(agent.asked());

    agent.coordinator.rollback(None).unwrap();

    let asked = agent.asked();
    assert_eq!(staged_digest(&asked.reference), digest_of(STT));
    let resolved = asked.resolved.unwrap();
    assert_eq!(resolved.configuration.bundle.bundle_digest, digest_of(STT));
    assert_eq!(host.view(), ["stt@3", "stt@2", "-", "4"]);
    assert_eq!(host.last_error(), None);
}

#[test]
fn a_rollback_to_a_format_0_1_record_after_an_update_under_its_id_starts_the_earlier_bundle() {
    let host = host();
    let (agent, _) = host.start(Worker::default());
    agent.deploy("smoke", FORMAT_0_1).unwrap();
    drop(agent.asked());
    let update = another_format_0_1_bundle(&host);
    let coordinator = &agent.coordinator;
    let updated = coordinator.deploy("smoke", &update, BTreeMap::new(), None, None);
    updated.unwrap();
    drop(agent.asked());

    agent.coordinator.rollback(None).unwrap();

    let reference = agent.asked().reference;
    assert_eq!(reference.bundle_digest, digest_of(FORMAT_0_1));
    assert_eq!(staged_digest(&reference), digest_of(FORMAT_0_1));
}

#[test]
fn a_start_after_a_same_id_update_that_did_not_complete_restores_the_record_s_own_bundle() {
    let host = host();

    let next = started_after_an_update_that_did_not_complete(&host);

    let asked = next.asked();
    assert_eq!(staged_digest(&asked.reference), digest_of(STT));
    assert_eq!(host.view(), ["stt@2", "-", "-", "3"]);
    assert_eq!(host.last_error(), None);
}

/// The staged path of the record in `slot`.
fn staged_path(host: &Host, slot: fn(AgentState) -> Option<DeploymentRecord>) -> PathBuf {
    let state = StateStore::open(host.state_dir()).unwrap().snapshot();
    PathBuf::from(slot(state.unwrap()).unwrap().staged_path)
}

fn active(state: AgentState) -> Option<DeploymentRecord> {
    state.active
}

fn previous(state: AgentState) -> Option<DeploymentRecord> {
    state.previous_active
}

/// The files of the tree at `root`, with what each holds.
fn contents(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let entries = fs::read_dir(root).unwrap().map(|entry| entry.unwrap());
    let files = entries.filter(|entry| entry.file_type().unwrap().is_file());
    let read = |entry: fs::DirEntry| {
        (
            entry.file_name().into_string().unwrap(),
            fs::read(entry.path()).unwrap(),
        )
    };
    files.map(read).collect()
}

#[test]
fn each_deploy_stages_a_tree_of_its_own_named_by_its_transaction() {
    let host = host();
    let (agent, _) = host.start(Worker::default());
    let deploy = |bundle: &str| {
        let deployed = agent
            .coordinator
            .deploy("stt", &repo(bundle), BTreeMap::new(), None, None);
        drop(agent.asked());
        deployed.unwrap().transaction_id
    };
    let first = deploy(STT);
    let earlier = contents(&staged_path(&host, active));

    let second = deploy(TTS);

    let tree = |transaction: &str| host.td.path().join("staging").join("stt").join(transaction);
    assert_eq!(staged_path(&host, active), tree(&second));
    assert_eq!(staged_path(&host, previous), tree(&first));
    assert_eq!(contents(&tree(&first)), earlier);
    assert_eq!(host.trees("stt"), [first, second].into_iter().collect());
}

#[test]
fn a_deploy_removes_the_trees_no_record_names_before_it_stages() {
    let host = host();
    let (agent, _) = host.start(Worker::default());
    let deploy = |bundle: &str| {
        let deployed = agent.deploy("stt", bundle);
        drop(agent.asked());
        deployed
    };
    deploy(STT).unwrap();
    // A deploy refused after it staged leaves a tree that nothing names.
    let unfinished = host.state_dir().join("descriptor-stt-2.json.tmp");
    fs::create_dir(&unfinished).unwrap();
    agent.deploy("stt", TTS).unwrap_err();
    fs::remove_dir(unfinished).unwrap();
    let after_the_refusal = host.trees("stt");
    assert_eq!(after_the_refusal.len(), 2);
    let first = staged_path(&host, active);

    deploy(TTS).unwrap();
    let mut kept = host.trees("stt");
    assert_eq!(kept.len(), 2);
    assert!(kept.contains(first.file_name().unwrap().to_str().unwrap()));
    assert!(!after_the_refusal.is_subset(&kept));

    // The record a commit drops goes when the next deploy begins.
    deploy(STT).unwrap();
    assert_eq!(host.trees("stt").len(), 3);
    assert!(first.is_dir());
    deploy(TTS).unwrap();
    assert!(!first.exists());
    kept.remove(first.file_name().unwrap().to_str().unwrap());
    let named = [active, previous].map(|slot| staged_path(&host, slot));
    let named: BTreeSet<_> = named
        .iter()
        .map(|p| p.file_name().unwrap().to_str().unwrap().to_owned())
        .collect();
    assert_eq!(host.trees("stt").len(), 3);
    assert!(named.is_subset(&host.trees("stt")) && kept.is_subset(&host.trees("stt")));
}

#[test]
fn a_prune_that_fails_does_not_fail_the_deploy() {
    let host = host();
    let (agent, _) = host.start(Worker::default());
    for bundle in [STT, TTS, STT] {
        agent.deploy("stt", bundle).unwrap();
        drop(agent.asked());
    }
    assert_eq!(host.trees("stt").len(), 3);

    crate::staging::fail_next_prune();
    agent.deploy("stt", TTS).unwrap();

    // What no record named is left for the next.
    drop(agent.asked());
    assert_eq!(host.trees("stt").len(), 4);
    assert_eq!(host.last_error(), None);
}

#[test]
fn a_tree_of_the_earlier_layout_is_started_again_and_never_written_or_removed() {
    let host = host();
    // `stt` recorded at `staging_dir/stt`, as a deploy staged before this one.
    let (agent, _) = host.restoring(Worker::default());
    let flat = host.td.path().join("staging").join("stt");
    let before = contents(&flat);

    agent.deploy("stt", TTS).unwrap();
    drop(agent.asked());
    assert_eq!(contents(&flat), before);
    assert_eq!(staged_path(&host, previous), flat);

    agent.coordinator.rollback(None).unwrap();

    let asked = agent.asked();
    assert_eq!(asked.reference.staged_path, flat.display().to_string());
    assert_eq!(staged_digest(&asked.reference), digest_of(STT));
    // Once no record names it, it is still left as it is.
    for bundle in [TTS, STT, TTS] {
        agent.deploy("stt", bundle).unwrap();
        drop(agent.asked());
    }
    drop(host.start(Worker::default()));
    assert_ne!(staged_path(&host, previous), flat);
    assert_eq!(contents(&flat), before);
    assert_eq!(host.trees("stt").len(), 2);
}

#[test]
fn a_rollback_to_a_record_whose_tree_is_gone_is_refused_and_the_active_record_stays() {
    let host = host();
    let (agent, _) = host.start(Worker::default());
    for bundle in [STT, TTS] {
        agent.deploy("stt", bundle).unwrap();
        drop(agent.asked());
    }
    let retained = staged_path(&host, previous);
    fs::remove_dir_all(&retained).unwrap();
    let before = host.view();

    let error = agent.coordinator.rollback(None).unwrap_err();

    let gone = format!(
        "previous active staged path `{}` is missing",
        retained.display()
    );
    assert!(
        matches!(&error, AgentError::Unavailable(m) if *m == gone),
        "{error}"
    );
    assert!(agent.never_asked());
    assert_eq!(host.view(), before);
    assert!(host.settled());
    // The next start restores the active record from its own tree.
    let (next, _) = host.start(Worker::default());
    apply_startup(&next.coordinator).unwrap();
    assert_eq!(staged_digest(&next.asked().reference), digest_of(TTS));
}

#[test]
fn a_removal_a_stop_interrupted_is_finished_by_the_next_start() {
    let host = host();
    let (agent, _) = host.start(Worker::default());
    for bundle in [STT, TTS, STT] {
        agent.deploy("stt", bundle).unwrap();
        drop(agent.asked());
    }
    let dropped: Vec<_> = host.trees("stt").into_iter().collect();
    let named = [active, previous].map(|slot| staged_path(&host, slot));
    let unnamed = dropped
        .iter()
        .map(|name| host.td.path().join("staging").join("stt").join(name))
        .find(|tree| !named.contains(tree))
        .unwrap();
    // The removal stopped after the manifest.
    fs::remove_file(unnamed.join("manifest.json")).unwrap();

    let (next, _) = host.start(Worker::default());
    apply_startup(&next.coordinator).unwrap();

    assert!(!unnamed.exists());
    assert!(named.iter().all(|tree| tree.is_dir()));
    assert_eq!(staged_digest(&next.asked().reference), digest_of(STT));
}
