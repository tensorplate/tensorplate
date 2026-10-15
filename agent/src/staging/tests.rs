// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use tempfile::TempDir;
use tensorplate_protocol::agent_state::{
    decode_agent_state, DeploymentRecord, TransactionKind, TransactionRecord,
};
use tensorplate_protocol::deploy_transaction::DeployState;

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
fn a_stage_that_fails_leaves_no_directory_it_made_so_the_next_one_makes_and_syncs_them() {
    let bundle = verified(&repo(FIXTURE));
    let cases = [
        ("the manifest", None),
        ("the id directory", Some("staging/stt")),
        ("staging_dir", Some("staging")),
        ("the directory that was there", Some("")),
    ];
    for (case, at) in cases {
        let d = dirs();
        let top = d.staging.parent().unwrap().to_path_buf();
        let picked = at.map(|at| top.join(at));
        crate::durable::hooks::fail_next_sync_of(move |path| match &picked {
            Some(picked) => path == picked,
            None => path.ends_with("manifest.json"),
        });

        let failed = stage(&d.staging, "stt", &new_transaction_id(), &bundle);

        assert!(failed.is_err(), "{case}");
        assert!(!d.staging.exists(), "{case}");
        drop(crate::durable::hooks::synced());
        let tree = stage(&d.staging, "stt", &new_transaction_id(), &bundle).unwrap();
        let synced = crate::durable::hooks::synced();
        let above: Vec<_> = synced.iter().filter(|dir| tree.starts_with(dir)).collect();
        let made = [&tree, &d.staging.join("stt"), &d.staging, &top];
        assert_eq!(above, made, "{case}");
    }
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

#[test]
fn a_staging_dir_that_cannot_be_read_is_reported() {
    let d = dirs();
    fs::write(&d.staging, "").unwrap();

    let error = prune(&d.staging, &d.store).unwrap_err().to_string();

    let says = format!("`{}`: ", d.staging.display());
    assert!(error.contains(&says), "{error}");
}

#[cfg(unix)]
#[test]
fn a_tree_a_record_names_is_kept_however_staging_dir_is_spelled() {
    let d = dirs();
    let root = d.staging.parent().unwrap();
    fs::create_dir_all(root.join("elsewhere")).unwrap();
    fs::create_dir_all(&d.staging).unwrap();
    std::os::unix::fs::symlink(&d.staging, root.join("alias")).unwrap();
    let spellings = [
        d.staging.clone(),
        root.join("alias"),
        root.join("elsewhere").join("..").join("staging"),
    ];
    for recorded in &spellings {
        for pruned in &spellings {
            let [named, unnamed] = ["stt", "stt"].map(|id| tree(&d.staging, id));
            let staged = recorded.join("stt").join(named.file_name().unwrap());
            let records = |state: &mut AgentState| {
                state.active = Some(record("stt", &staged));
                Ok(())
            };
            d.store.update(records).unwrap();

            let removed = prune(pruned, &d.store).unwrap();

            let spelled = format!("recorded as {recorded:?}, pruned as {pruned:?}");
            assert_eq!(removed, 1, "{spelled}");
            assert!(named.is_dir() && !unnamed.exists(), "{spelled}");
            fs::remove_dir_all(named).unwrap();
        }
    }
}

#[cfg(unix)]
#[test]
fn nothing_is_removed_while_a_record_s_path_does_not_resolve_and_a_missing_one_names_nothing() {
    let d = dirs();
    let root = d.staging.parent().unwrap();
    std::os::unix::fs::symlink(root.join("loop-b"), root.join("loop-a")).unwrap();
    std::os::unix::fs::symlink(root.join("loop-a"), root.join("loop-b")).unwrap();
    let unnamed = tree(&d.staging, "stt");
    let name = |staged: PathBuf| {
        move |state: &mut AgentState| {
            state.active = Some(record("stt", &staged));
            Ok(())
        }
    };
    let looping = root.join("loop-a").join(new_transaction_id());
    d.store.update(name(looping.clone())).unwrap();

    let error = prune(&d.staging, &d.store).unwrap_err().to_string();

    let says = format!("named path `{}` does not resolve: ", looping.display());
    assert!(error.contains(&says), "{error}");
    assert!(unnamed.is_dir());
    let missing = d.staging.join("stt").join(new_transaction_id());
    d.store.update(name(missing)).unwrap();
    assert_eq!(prune(&d.staging, &d.store).unwrap(), 1);
    assert!(!unnamed.exists());
}

#[cfg(unix)]
#[test]
fn a_prune_goes_on_past_what_it_cannot_read_or_remove_and_reports_the_first() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        eprintln!("skipped: root reads and removes a directory of mode 000");
        return;
    }
    let d = dirs();
    // In name order: an id's directory that cannot be read, then a tree that
    // cannot be removed, then trees that can.
    let unreadable = d.staging.join("aaa");
    let stuck = d
        .staging
        .join("stt")
        .join(format!("tx-{}", uuid::Uuid::nil()));
    let locked = stuck.join("locked");
    fs::create_dir_all(&unreadable).unwrap();
    fs::create_dir_all(&locked).unwrap();
    fs::write(locked.join("weights"), "").unwrap();
    let last = format!("tx-{}", uuid::Uuid::from_u128(u128::MAX));
    let removable = [d.staging.join("stt").join(last), tree(&d.staging, "tts")];
    fs::create_dir_all(&removable[0]).unwrap();
    let mode = |dir: &Path, mode: u32| {
        fs::set_permissions(dir, fs::Permissions::from_mode(mode)).unwrap();
    };
    mode(&unreadable, 0o000);
    mode(&locked, 0o000);

    let error = prune(&d.staging, &d.store).unwrap_err().to_string();

    mode(&unreadable, 0o755);
    mode(&locked, 0o755);
    let first = format!("`{}`: ", unreadable.display());
    assert!(error.contains(&first), "{error}");
    assert!(removable.iter().all(|tree| !tree.exists()));
    assert!(locked.join("weights").is_file());
    assert_eq!(prune(&d.staging, &d.store).unwrap(), 1);
}

#[test]
fn entries_come_back_in_name_order_whatever_order_they_were_made_in() {
    let td = TempDir::new().unwrap();
    // Neither in name order nor in its reverse, so no file system lists
    // them in name order by itself.
    for i in (0..64).map(|i| (i * 37) % 64) {
        fs::create_dir(td.path().join(format!("{i:02}"))).unwrap();
    }

    let names: Vec<_> = entries(td.path())
        .unwrap()
        .into_iter()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();

    let ordered: Vec<_> = (0..64).map(|i| format!("{i:02}")).collect();
    assert_eq!(names, ordered);
}

#[cfg(unix)]
#[test]
fn a_tree_that_cannot_be_removed_is_named() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        eprintln!("skipped: root removes a directory of mode 000");
        return;
    }
    let d = dirs();
    let stuck = tree(&d.staging, "stt");
    let locked = stuck.join("locked");
    fs::create_dir(&locked).unwrap();
    fs::write(locked.join("weights"), "").unwrap();
    let mode = |mode: u32| {
        fs::set_permissions(&locked, fs::Permissions::from_mode(mode)).unwrap();
    };
    mode(0o000);

    let error = prune(&d.staging, &d.store).unwrap_err().to_string();

    mode(0o755);
    let named = format!("`{}`: ", stuck.display());
    assert!(error.contains(&named), "{error}");
    assert!(locked.join("weights").is_file());
}

#[cfg(unix)]
#[test]
fn an_id_directory_that_is_a_link_is_not_followed() {
    let d = dirs();
    let outside = d.staging.parent().unwrap().join("outside");
    let tree = outside.join(new_transaction_id());
    fs::create_dir_all(&tree).unwrap();
    fs::create_dir_all(&d.staging).unwrap();
    std::os::unix::fs::symlink(&outside, d.staging.join("stt")).unwrap();

    assert_eq!(prune(&d.staging, &d.store).unwrap(), 0);

    assert!(tree.is_dir());
    assert!(d.staging.join("stt").symlink_metadata().is_ok());
}

/// A deploy in flight that reads its bundle from `bundle_path`.
fn reading(bundle_path: &Path) -> TransactionRecord {
    TransactionRecord {
        transaction_id: new_transaction_id(),
        deployment_id: "stt".into(),
        phase: DeployState::Received,
        kind: TransactionKind::Deploy,
        bundle_digest: None,
        generation: None,
        descriptor_digest: None,
        configuration_digest: None,
        bundle_path: Some(bundle_path.display().to_string()),
        correlation_id: None,
        started_monotonic_ns: None,
        last_transition_monotonic_ns: None,
        failure: None,
    }
}

#[test]
fn a_tree_the_transaction_in_flight_reads_its_bundle_from_or_from_within_is_kept() {
    let d = dirs();
    let read_from = |bundle: Option<PathBuf>| {
        move |state: &mut AgentState| {
            state.in_flight_transaction = bundle.as_deref().map(reading);
            Ok(())
        }
    };
    let [within, unnamed] = ["stt", "tts"].map(|id| tree(&d.staging, id));
    d.store
        .update(read_from(Some(within.join("warmup"))))
        .unwrap();

    assert_eq!(prune(&d.staging, &d.store).unwrap(), 1);
    assert!(within.is_dir() && !unnamed.exists());

    // Spelled another way, as a record may spell it.
    let source = tree(&d.staging, "stt");
    let spelled = d
        .staging
        .join("tts/../stt")
        .join(source.file_name().unwrap());
    d.store.update(read_from(Some(spelled))).unwrap();
    assert_eq!(prune(&d.staging, &d.store).unwrap(), 1);
    assert!(source.is_dir() && !within.exists());

    d.store.update(read_from(None)).unwrap();
    assert_eq!(prune(&d.staging, &d.store).unwrap(), 1);
    assert!(!source.exists());
}

/// A tree of the earlier layout at `staging_dir/<id>`, its artifacts under
/// `root` where the fixture has `assets`.
fn flat_tree(staging: &Path, id: &str, root: &str) -> PathBuf {
    let flat = staging.join(id);
    let manifest = fs::read_to_string(repo(FIXTURE).join("manifest.json")).unwrap();
    let manifest = manifest.replace("\"assets/", &format!("\"{root}/"));
    fs::create_dir_all(flat.join(root)).unwrap();
    fs::write(flat.join("manifest.json"), manifest).unwrap();
    fs::write(flat.join(root).join("tokenizer.json"), "{}").unwrap();
    let options = tensorplate_protocol::bundle::ParseOptions {
        verify_artifact_digests: false,
    };
    tensorplate_protocol::bundle::parse_bundle_with(&flat, options).unwrap();
    flat
}

#[test]
fn a_directory_a_tree_of_the_earlier_layout_lists_an_artifact_under_is_kept_named_or_not() {
    let d = dirs();
    // An artifact directory may have any safe name, a transaction id's too,
    // and the artifact may be deeper in it.
    let listed = new_transaction_id();
    let flat = flat_tree(&d.staging, "stt", &format!("{listed}/deeper"));
    let artifact = flat.join(&listed).join("deeper/tokenizer.json");
    let unnamed = tree(&d.staging, "stt");

    assert_eq!(prune(&d.staging, &d.store).unwrap(), 1);
    assert!(artifact.is_file() && !unnamed.exists());

    let flat_record = |state: &mut AgentState| {
        state.active = Some(record("stt", &flat));
        Ok(())
    };
    d.store.update(flat_record).unwrap();
    assert_eq!(prune(&d.staging, &d.store).unwrap(), 0);
    assert!(artifact.is_file());
}

#[test]
fn a_deploy_s_tree_inside_a_tree_of_the_earlier_layout_is_removed_once_no_record_names_it() {
    let d = dirs();
    let flat = flat_tree(&d.staging, "stt", "assets");
    let [named, unnamed] = ["stt", "stt"].map(|id| tree(&d.staging, id));
    let records = |state: &mut AgentState| {
        state.active = Some(record("stt", &named));
        state.previous_active = Some(record("stt", &flat));
        Ok(())
    };
    d.store.update(records).unwrap();

    assert_eq!(prune(&d.staging, &d.store).unwrap(), 1);

    assert!(named.is_dir() && !unnamed.exists());
    assert!(flat.join("manifest.json").is_file());
    assert!(flat.join("assets/tokenizer.json").is_file());
}

#[test]
fn nothing_in_a_tree_of_the_earlier_layout_is_removed_while_its_manifest_cannot_be_read() {
    let d = dirs();
    let flat = flat_tree(&d.staging, "stt", "assets");
    let whole = fs::read(flat.join("manifest.json")).unwrap();
    let unnamed = tree(&d.staging, "stt");
    fs::write(flat.join("manifest.json"), "{").unwrap();

    assert_eq!(prune(&d.staging, &d.store).unwrap(), 0);
    assert!(unnamed.is_dir());

    #[cfg(unix)]
    if !rustix::process::geteuid().is_root() {
        use std::os::unix::fs::PermissionsExt;
        fs::write(flat.join("manifest.json"), &whole).unwrap();
        let mode = |mode: u32| {
            let permissions = fs::Permissions::from_mode(mode);
            fs::set_permissions(flat.join("manifest.json"), permissions).unwrap();
        };
        mode(0o000);
        let pruned = prune(&d.staging, &d.store);
        mode(0o644);
        assert_eq!(pruned.unwrap(), 0);
        assert!(unnamed.is_dir());
    }

    fs::write(flat.join("manifest.json"), whole).unwrap();
    assert_eq!(prune(&d.staging, &d.store).unwrap(), 1);
    assert!(!unnamed.exists());
}

#[cfg(unix)]
#[test]
fn an_id_directory_whose_manifest_cannot_be_looked_for_is_named_and_left() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        eprintln!("skipped: root looks into a directory of mode 400");
        return;
    }
    let d = dirs();
    let unnamed = tree(&d.staging, "stt");
    let id = d.staging.join("stt");
    let mode = |mode: u32| {
        fs::set_permissions(&id, fs::Permissions::from_mode(mode)).unwrap();
    };
    // Listed, but nothing in it can be looked up.
    mode(0o400);

    let error = prune(&d.staging, &d.store).unwrap_err().to_string();

    mode(0o755);
    let named = format!("`{}`: ", id.display());
    assert!(error.contains(&named), "{error}");
    assert!(unnamed.is_dir());
    assert_eq!(prune(&d.staging, &d.store).unwrap(), 1);
}

#[test]
fn nothing_is_removed_while_either_state_file_is_damaged() {
    let d = dirs();
    let [named, unnamed] = ["stt", "stt"].map(|id| tree(&d.staging, id));
    let name = |state: &mut AgentState| {
        state.active = Some(record("stt", &named));
        Ok(())
    };
    d.store.update(name).unwrap();
    for damaged in ["state.json", "state.json.bak"] {
        let path = d.store.state_dir().join(damaged);
        let whole = fs::read(&path).unwrap();
        fs::write(&path, "{").unwrap();

        let error = prune(&d.staging, &d.store).unwrap_err();

        fs::write(&path, whole).unwrap();
        assert!(
            matches!(error, AgentError::CorruptState(_)),
            "{damaged}: {error}"
        );
        assert!(named.is_dir() && unnamed.is_dir(), "{damaged}");
    }
    assert_eq!(prune(&d.staging, &d.store).unwrap(), 1);
}
