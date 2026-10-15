// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use tempfile::TempDir;
use tensorplate_protocol::agent_state::{decode_agent_state, DeploymentRecord};

use crate::coordinator::new_transaction_id;

/// A bundle with an artifact in a directory of its own.
const FIXTURE: &str = "test/models/bundles/v0_1/smolvla_python_pytorch";
const STAGED: [&str; 4] = [
    "assets/tokenizer.json",
    "manifest.json",
    "policy.py",
    "policy_weights.safetensors",
];

fn repo(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(relative)
}

fn verified(root: &Path) -> VerifiedBundle {
    tensorplate_protocol::parse_bundle(root).unwrap().into()
}

/// The files under `dir`, as paths relative to it.
fn files(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in fs::read_dir(next).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let relative = path.strip_prefix(dir).unwrap();
                found.push(relative.display().to_string());
            }
        }
    }
    found.sort();
    found
}

fn record(id: &str, staged_path: &Path) -> DeploymentRecord {
    serde_json::from_value(serde_json::json!({
        "deployment_id": id,
        "bundle_digest": "sha256:00",
        "staged_path": staged_path,
        "bundle_name": "b",
        "bundle_version": "1",
        "backend_hint": "mock",
        "model_class": "vision",
    }))
    .unwrap()
}

/// A tree for a deploy of `id`, as [`stage`] leaves one.
fn tree(staging: &Path, id: &str) -> PathBuf {
    let tree = staging.join(id).join(new_transaction_id());
    fs::create_dir_all(tree.join("warmup")).unwrap();
    fs::write(tree.join("manifest.json"), "{}").unwrap();
    fs::write(tree.join("warmup/segment.txt"), "").unwrap();
    tree
}

struct Dirs {
    _td: TempDir,
    staging: PathBuf,
    store: StateStore,
}

fn dirs() -> Dirs {
    let td = TempDir::new().unwrap();
    let staging = td.path().join("staging");
    let store = StateStore::open(td.path().join("state")).unwrap();
    Dirs {
        _td: td,
        staging,
        store,
    }
}

#[test]
fn only_a_transaction_id_as_a_deploy_takes_one_names_a_tree() {
    let id = new_transaction_id();
    assert!(names_a_tree(&id));
    let refused = [
        id.trim_start_matches(TRANSACTION_PREFIX).to_owned(),
        id.to_uppercase().replacen("TX-", "tx-", 1),
        format!("{id}.tmp"),
        id.replacen(TRANSACTION_PREFIX, "tx_", 1),
        format!("tx-{}", uuid::Uuid::new_v4().simple()),
        "manifest.json".into(),
        "7".into(),
    ];
    for name in refused {
        assert!(!names_a_tree(&name), "{name}");
    }
}

#[test]
fn a_deploy_stages_the_manifest_and_its_artifacts_into_a_tree_of_its_own() {
    let d = dirs();
    let transaction = new_transaction_id();

    let staged = stage(&d.staging, "stt", &transaction, &verified(&repo(FIXTURE))).unwrap();

    assert_eq!(staged, d.staging.join("stt").join(&transaction));
    assert_eq!(files(&staged), STAGED);
    for file in STAGED {
        let read = |root: &Path| fs::read(root.join(file)).unwrap();
        assert_eq!(read(&staged), read(&repo(FIXTURE)), "{file}");
    }
}

#[test]
fn a_tree_that_is_there_is_not_written() {
    let d = dirs();
    let transaction = new_transaction_id();
    let there = d.staging.join("stt").join(&transaction);
    fs::create_dir_all(&there).unwrap();
    fs::write(there.join("manifest.json"), "kept").unwrap();

    let error = stage(&d.staging, "stt", &transaction, &verified(&repo(FIXTURE))).unwrap_err();

    assert!(matches!(error, AgentError::Io(_)), "{error}");
    assert!(error.to_string().contains(&there.display().to_string()));
    assert_eq!(files(&there), ["manifest.json"]);
    assert_eq!(
        fs::read_to_string(there.join("manifest.json")).unwrap(),
        "kept"
    );
}

#[test]
fn a_copy_that_fails_removes_its_tree_and_names_it() {
    let d = dirs();
    let source = d.staging.parent().unwrap().join("source");
    fs::create_dir_all(source.join("assets")).unwrap();
    for file in STAGED {
        fs::copy(repo(FIXTURE).join(file), source.join(file)).unwrap();
    }
    let bundle = verified(&source);
    fs::remove_file(source.join("policy_weights.safetensors")).unwrap();
    let transaction = new_transaction_id();

    let error = stage(&d.staging, "stt", &transaction, &bundle).unwrap_err();

    let tree = d.staging.join("stt").join(&transaction);
    let named = format!("staging `{}`: ", tree.display());
    assert!(error.to_string().starts_with(&named), "{error}");
    assert!(!tree.exists());
}

#[test]
fn nothing_is_staged_for_an_id_or_a_transaction_that_cannot_name_a_tree() {
    let d = dirs();
    let bundle = verified(&repo(FIXTURE));
    for (id, transaction) in [("../stt", new_transaction_id()), ("stt", "tx".into())] {
        let error = stage(&d.staging, id, &transaction, &bundle).unwrap_err();
        assert!(matches!(error, AgentError::Internal(_)), "{error}");
    }
    assert!(!d.staging.exists());
}

#[test]
fn a_prune_removes_every_tree_no_record_names_and_nothing_else() {
    let d = dirs();
    let [active, previous, candidate, unnamed, other] =
        ["stt", "stt", "stt", "stt", "tts"].map(|id| tree(&d.staging, id));
    // A tree of the layout before trees of their own, and what is not a tree.
    let flat = d.staging.join("vad");
    fs::create_dir_all(flat.join("model")).unwrap();
    fs::write(flat.join("manifest.json"), "{}").unwrap();
    let not_trees = [
        d.staging.join("stt").join("7"),
        d.staging.join("stt").join("model"),
        d.staging.join("not an id").join(new_transaction_id()),
    ];
    for dir in &not_trees {
        fs::create_dir_all(dir).unwrap();
    }
    let file = d.staging.join("stt").join(new_transaction_id());
    fs::write(&file, "").unwrap();
    #[cfg(unix)]
    let link = {
        let link = d.staging.join("stt").join(new_transaction_id());
        std::os::unix::fs::symlink(&other, &link).unwrap();
        link
    };
    let records = |state: &mut AgentState| {
        state.active = Some(record("stt", &active));
        state.previous_active = Some(record("stt", &previous));
        state.candidate = Some(record("stt", &candidate));
        Ok(())
    };
    d.store.update(records).unwrap();
    let flat_record = |state: &mut AgentState| {
        state.previous_active = Some(record("vad", &flat));
        Ok(())
    };

    assert_eq!(prune(&d.staging, &d.store).unwrap(), 2);

    assert!(active.is_dir() && previous.is_dir() && candidate.is_dir());
    assert!(!unnamed.exists() && !other.exists());
    assert!(not_trees.iter().all(|dir| dir.is_dir()) && file.is_file());
    #[cfg(unix)]
    assert!(link.symlink_metadata().is_ok());
    assert_eq!(files(&flat), ["manifest.json"]);
    assert!(flat.join("model").is_dir());

    // A flat tree is kept whether a record names it or not.
    d.store.update(flat_record).unwrap();
    assert_eq!(prune(&d.staging, &d.store).unwrap(), 1);
    assert!(!previous.exists() && flat.join("manifest.json").is_file());
    assert_eq!(prune(&d.staging, &d.store).unwrap(), 0);
}

#[test]
fn a_prune_keeps_the_trees_of_a_resident_set_and_the_generations_its_members_retain() {
    let d = dirs();
    let fixture = "protocol/rust/tests/fixtures/agent_state_0_2_two_member_set.json";
    let mut set = decode_agent_state(&fs::read_to_string(repo(fixture)).unwrap()).unwrap();
    let mut kept = Vec::new();
    for member in &mut set.resident_set.as_mut().unwrap().members {
        let staged = tree(&d.staging, &member.deployment_id);
        member.staged_path = staged.display().to_string();
        kept.push(staged);
        if let Some(previous) = member.previous.as_mut() {
            let staged = tree(&d.staging, &previous.deployment_id);
            previous.staged_path = staged.display().to_string();
            kept.push(staged);
        }
    }
    assert_eq!(kept.len(), 3);
    let unnamed = tree(&d.staging, "speech-tts");
    d.store
        .update(|state| {
            *state = AgentState {
                store_version: state.store_version,
                ..set
            };
            Ok(())
        })
        .unwrap();

    assert_eq!(prune(&d.staging, &d.store).unwrap(), 1);

    assert!(kept.iter().all(|tree| tree.is_dir()) && !unnamed.exists());
}

#[test]
fn a_tree_either_state_file_names_is_kept() {
    let d = dirs();
    let [first, second] = ["stt", "stt"].map(|id| tree(&d.staging, id));
    let name = |staged: &Path| {
        let staged = staged.to_owned();
        move |state: &mut AgentState| {
            state.active = Some(record("stt", &staged));
            Ok(())
        }
    };
    d.store.update(name(&first)).unwrap();
    let state_dir = d.store.state_dir().to_owned();
    let backup = fs::read(state_dir.join("state.json")).unwrap();
    d.store.update(name(&second)).unwrap();
    // The pair as a write that stopped between its two renames leaves it.
    fs::write(state_dir.join("state.json.bak"), backup).unwrap();

    assert_eq!(prune(&d.staging, &d.store).unwrap(), 0);

    assert!(first.is_dir() && second.is_dir());
}

#[test]
fn nothing_is_removed_while_what_a_state_file_holds_cannot_be_told() {
    let d = dirs();
    let unnamed = tree(&d.staging, "stt");
    d.store.update(|_| Ok(())).unwrap();
    let backup = d.store.state_dir().join("state.json.bak");
    fs::remove_file(&backup).unwrap();
    fs::create_dir(&backup).unwrap();

    assert!(prune(&d.staging, &d.store).is_err());

    assert!(unnamed.is_dir());
}

#[test]
fn a_removal_that_was_interrupted_is_finished_by_the_next_prune() {
    let d = dirs();
    let unnamed = tree(&d.staging, "stt");
    // The manifest went before the stop; the rest of the tree is left.
    fs::remove_file(unnamed.join("manifest.json")).unwrap();

    assert_eq!(prune(&d.staging, &d.store).unwrap(), 1);

    assert!(!unnamed.exists() && d.staging.join("stt").is_dir());
}

#[test]
fn a_prune_finds_nothing_to_remove_before_anything_was_staged() {
    let d = dirs();
    assert_eq!(prune(&d.staging, &d.store).unwrap(), 0);
}
