// SPDX-License-Identifier: Apache-2.0

use super::deploy::{started_after_an_update_that_did_not_complete, FORMAT_0_1, STT, TTS};
use super::one_pass::another_format_0_1_bundle;
use super::start_again::{host, Host, Worker};
use super::*;

use std::cell::RefCell;
use std::rc::Rc;

use tensorplate_protocol::agent_state::TransactionKind;

use crate::coordinator::new_transaction_id;
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

#[test]
fn a_deploy_refused_as_busy_removes_no_tree() {
    let host = host();
    let (agent, _) = host.start(Worker::default());
    // Another deploy is copying its bundle; no record names the copy yet.
    let copying = new_transaction_id();
    agent.in_flight(TransactionKind::Deploy, &copying, "tts");
    let copy = host.td.path().join("staging").join("tts").join(&copying);
    fs::create_dir_all(&copy).unwrap();

    let error = agent.deploy("stt", STT).unwrap_err();

    assert!(
        matches!(&error, AgentError::Busy(tx) if *tx == copying),
        "{error}"
    );
    assert!(copy.is_dir() && agent.never_asked());
}

#[test]
fn a_deploy_that_finds_the_disk_full_as_it_stages_fails_and_leaves_every_tree_as_it_was() {
    let host = host();
    let (agent, _) = host.start(Worker::default());
    agent.deploy("stt", STT).unwrap();
    drop(agent.asked());
    let (view, trees) = (host.view(), host.trees("stt"));
    let named = staged_path(&host, active);
    let served = contents(&named);

    crate::staging::fill_disk_at_next_copy();
    let error = agent.deploy("stt", TTS).unwrap_err();

    let full = std::io::Error::from(rustix::io::Errno::NOSPC);
    let staging = host.td.path().join("staging").join("stt").join("tx-");
    let says = format!("staging `{}", staging.display());
    let message = error.to_string();
    assert!(matches!(error, AgentError::Io(_)), "{message}");
    assert!(
        message.contains(&says) && message.ends_with(&full.to_string()),
        "{message}"
    );
    assert!(agent.never_asked());
    assert_eq!((host.view(), host.trees("stt")), (view, trees));
    assert_eq!(contents(&named), served);
    let (quarantined, latest) = host.quarantined();
    assert!(
        quarantined.ends_with("@Verified") && latest,
        "{quarantined}"
    );
    let recorded = host.last_error().unwrap();
    assert_eq!(
        (recorded.code, recorded.message),
        (ErrorCode::Internal, message)
    );

    agent.deploy("stt", TTS).unwrap();
    assert_eq!(staged_digest(&agent.asked().reference), digest_of(TTS));
}

#[test]
fn a_deploy_syncs_its_tree_and_each_directory_it_adds_to_before_a_state_write_names_it() {
    let host = host();
    let (agent, _) = host.start(Worker::default());
    let (state_dir, staging) = (host.state_dir(), host.td.path().join("staging"));
    let of_staging = |path: &Path| path.starts_with(&staging) || staging.starts_with(path);
    assert!(!staging.exists());
    let first: (_, &[_], _) = (TTS, &["warmup/segment.txt", "warmup"], 3);
    let second: (_, &[_], _) = (STT, &[], 3);

    let mut earlier = None;
    for (bundle, nested, above) in [first, second] {
        // The paths the records name at each sync of a staging path.
        let named = Rc::new(RefCell::new(BTreeSet::new()));
        let (seen, dir, under) = (Rc::clone(&named), state_dir.clone(), staging.clone());
        crate::durable::hooks::fail_next_sync_of(move |path| {
            if path.starts_with(&under) || under.starts_with(path) {
                let state = StateStore::open(&dir).unwrap().snapshot().unwrap();
                let records = [state.active, state.previous_active, state.candidate];
                let paths = records.into_iter().flatten().map(|r| r.staged_path);
                seen.borrow_mut().extend(paths.map(PathBuf::from));
            }
            false
        });
        drop(crate::durable::hooks::synced());
        agent.deploy("tts", bundle).unwrap();
        drop(agent.asked());

        let synced = crate::durable::hooks::synced();
        let tree = staged_path(&host, active);
        assert!(!named.borrow().contains(&tree), "{named:?}");
        // The records read are the agent's own: they name the earlier tree.
        let seen_earlier = earlier.as_ref().map(|t| named.borrow().contains(t));
        assert_ne!(seen_earlier, Some(false), "{named:?}");
        let start = synced.iter().position(|path| of_staging(path)).unwrap();
        let block = synced[start..].iter().take_while(|path| of_staging(path));
        let block: Vec<_> = block.cloned().collect();
        let mut expected: BTreeSet<_> = ["manifest.json", "entry.json", ""]
            .iter()
            .chain(nested)
            .map(|name| tree.join(name))
            .collect();
        expected.extend(tree.ancestors().skip(1).take(above).map(Path::to_path_buf));
        let blocked: BTreeSet<_> = block.iter().cloned().collect();
        assert_eq!((blocked, block.len()), (expected.clone(), expected.len()));
        for (at, path) in block.iter().enumerate() {
            let dir = path
                .parent()
                .and_then(|dir| block.iter().position(|p| p == dir));
            assert!(
                dir.map_or(true, |dir| at < dir),
                "{path:?} after its directory: {block:?}"
            );
        }
        let rest = &synced[start + block.len()..];
        assert!(!rest.iter().any(|path| of_staging(path)), "{synced:?}");
        earlier = Some(tree);
    }
}

#[test]
fn a_deploy_whose_tree_cannot_be_synced_fails_and_leaves_every_tree_as_it_was() {
    type Picks = Box<dyn Fn(&Path, &Path) -> bool>;
    let host = host();
    let (agent, _) = host.start(Worker::default());
    agent.deploy("stt", STT).unwrap();
    drop(agent.asked());
    let id_dir = host.td.path().join("staging").join("stt");
    let under = |name: &'static str| {
        move |path: &Path, id: &Path| path.starts_with(id) && path.ends_with(name)
    };
    let picks: [(&str, Picks); 6] = [
        ("the manifest", Box::new(under("manifest.json"))),
        ("an artifact", Box::new(under("entry.json"))),
        ("a nested artifact", Box::new(under("warmup/segment.txt"))),
        ("a nested directory", Box::new(under("warmup"))),
        ("the tree", Box::new(|path, id| path.parent() == Some(id))),
        (
            "the directory it is added to",
            Box::new(|path, id| path == id),
        ),
    ];

    for (case, picks) in picks {
        let (view, trees) = (host.view(), host.trees("stt"));
        let named = staged_path(&host, active);
        let served = contents(&named);
        let (picks, id) = (Rc::new(picks), id_dir.clone());
        let picked = Rc::clone(&picks);
        crate::durable::hooks::fail_next_sync_of(move |path| picked(path, &id));
        let error = agent.deploy("stt", TTS).unwrap_err();

        let failed = std::io::Error::from(rustix::io::Errno::IO);
        let says = format!("staging `{}", id_dir.join("tx-").display());
        let message = error.to_string();
        assert!(matches!(error, AgentError::Io(_)), "{case}: {message}");
        let told = message.contains(&says) && message.ends_with(&failed.to_string());
        assert!(told, "{case}: {message}");
        let synced = message
            .split("sync `")
            .nth(1)
            .and_then(|r| r.split("`: ").next());
        let names_it = synced.is_some_and(|path| picks(Path::new(path), &id_dir));
        assert!(names_it, "{case}: {message}");
        assert!(agent.never_asked(), "{case}");
        let left = (host.view(), host.trees("stt"));
        assert_eq!(left, (view, trees), "{case}");
        assert_eq!(contents(&named), served, "{case}");
        let (quarantined, latest) = host.quarantined();
        assert!(
            quarantined.ends_with("@Verified") && latest,
            "{case}: {quarantined}"
        );
        let recorded = host.last_error().unwrap();
        let recorded = (recorded.code, recorded.message);
        assert_eq!(recorded, (ErrorCode::Internal, message), "{case}");
    }

    agent.deploy("stt", TTS).unwrap();
    assert_eq!(staged_digest(&agent.asked().reference), digest_of(TTS));
}

#[test]
fn a_deploy_from_a_tree_no_record_names_stages_that_tree_s_bundle() {
    let host = host();
    let (agent, _) = host.start(Worker::default());
    for bundle in [STT, TTS, STT] {
        agent.deploy("stt", bundle).unwrap();
        drop(agent.asked());
    }
    let named = [active, previous].map(|slot| staged_path(&host, slot));
    let staging = host.td.path().join("staging").join("stt");
    let trees = host.trees("stt").into_iter().map(|name| staging.join(name));
    let unnamed = trees
        .into_iter()
        .find(|tree| !named.contains(tree))
        .unwrap();
    let earlier = contents(&unnamed);

    let deployed = agent
        .coordinator
        .deploy("stt", &unnamed, BTreeMap::new(), None, None);

    deployed.unwrap();
    assert_eq!(staged_digest(&agent.asked().reference), digest_of(STT));
    assert_eq!(contents(&staged_path(&host, active)), earlier);
    assert_eq!(contents(&unnamed), earlier);
    // Once no transaction reads it, the next deploy removes it.
    agent.deploy("stt", TTS).unwrap();
    drop(agent.asked());
    assert!(!unnamed.exists());
}
