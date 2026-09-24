// SPDX-License-Identifier: Apache-2.0
//
// `tensorplate bundle provision`: every outcome against the committed
// provisioning manifest fixture, whose one bundle is the committed
// `test/models/bundles/v0_1/smolvla_python_pytorch` bundle file for file.
// Each failure is checked for its own reason, and for leaving nothing a
// deploy could pick up: no destination and no partial root.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tensorplate_cli::args::ProvisionArgs;
use tensorplate_cli::commands::bundle::{provision, ProvisionError};
use tensorplate_cli::{CliError, ExitCode};
use tensorplate_protocol::bundle::parse_bundle;

const NAME: &str = "smolvla-fixture";

fn repo(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(relative)
}

fn source() -> PathBuf {
    repo("test/models/bundles/v0_1/smolvla_python_pytorch")
}

fn manifest() -> PathBuf {
    repo("protocol/rust/tests/fixtures/provisioning_manifest.json")
}

/// A temporary import directory, as the agent package lays it out.
fn import_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn args(from: &Path, into: &Path, manifest: &Path) -> ProvisionArgs {
    ProvisionArgs {
        name: NAME.to_string(),
        from: from.to_path_buf(),
        manifest: Some(manifest.to_path_buf()),
        into: Some(into.to_path_buf()),
    }
}

/// A writable copy of the committed source bundle.
fn source_copy() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    copy_tree(&source(), dir.path());
    dir
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).expect("read") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            fs::create_dir_all(&target).expect("mkdir");
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

/// Every file under `dir`, relative and sorted.
fn files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(d) = pending.pop() {
        for entry in fs::read_dir(&d).expect("read") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                pending.push(path);
            } else {
                out.push(
                    path.strip_prefix(dir)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    out.sort();
    out
}

/// Nothing deployable, and nothing half-done, is left in `into`.
fn assert_nothing_left(into: &Path, label: &str) {
    let left: Vec<String> = fs::read_dir(into)
        .expect("read")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert!(left.is_empty(), "{label}: left behind {left:?}");
}

#[test]
fn the_fixture_bundle_is_provisioned_file_for_file_and_deploy_accepts_it() {
    let into = import_dir();
    let report = provision(&args(&source(), into.path(), &manifest())).expect("provisions");
    let destination = into.path().join(NAME);
    assert_eq!(report.path, destination);
    assert_eq!(report.files, 4);
    assert!(!report.already_provisioned);
    assert_eq!(files(&destination), files(&source()));
    for file in files(&source()) {
        assert_eq!(
            fs::read(destination.join(&file)).unwrap(),
            fs::read(source().join(&file)).unwrap(),
            "{file}"
        );
    }
    parse_bundle(&destination).expect("deploy's own bundle check accepts it");
    let entries: Vec<String> = fs::read_dir(into.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, [NAME], "no partial root is left beside it");
}

#[test]
fn a_second_run_verifies_in_place_and_writes_nothing() {
    use std::os::unix::fs::MetadataExt;

    let into = import_dir();
    provision(&args(&source(), into.path(), &manifest())).expect("first run");
    let model = into.path().join(NAME).join("policy.py");
    let before = fs::metadata(&model).unwrap().ino();
    let report = provision(&args(&source(), into.path(), &manifest())).expect("second run");
    assert!(report.already_provisioned);
    assert_eq!(
        fs::metadata(&model).unwrap().ino(),
        before,
        "nothing was rewritten"
    );
}

#[test]
fn a_source_file_with_other_bytes_is_refused_and_leaves_nothing() {
    let from = source_copy();
    // Same size, different bytes: only the digest can tell.
    let path = from.path().join("policy.py");
    let mut bytes = fs::read(&path).unwrap();
    bytes[0] ^= 0x20;
    fs::write(&path, bytes).unwrap();
    let into = import_dir();
    match provision(&args(from.path(), into.path(), &manifest())) {
        Err(ProvisionError::DigestMismatch { path, .. }) => assert_eq!(path, "policy.py"),
        other => panic!("expected DigestMismatch, got {other:?}"),
    }
    assert_nothing_left(into.path(), "digest mismatch");
}

#[test]
fn a_source_file_of_another_size_is_refused_before_it_is_copied() {
    let from = source_copy();
    fs::write(from.path().join("assets/tokenizer.json"), "{}").unwrap();
    let into = import_dir();
    match provision(&args(from.path(), into.path(), &manifest())) {
        Err(ProvisionError::SizeMismatch { path, .. }) => assert_eq!(path, "assets/tokenizer.json"),
        other => panic!("expected SizeMismatch, got {other:?}"),
    }
    assert_nothing_left(into.path(), "size mismatch");
}

#[test]
fn a_missing_source_file_is_refused_and_leaves_nothing() {
    let from = source_copy();
    fs::remove_file(from.path().join("policy_weights.safetensors")).unwrap();
    let into = import_dir();
    match provision(&args(from.path(), into.path(), &manifest())) {
        Err(ProvisionError::SourceMissing { path }) => {
            assert_eq!(path, "policy_weights.safetensors");
        }
        other => panic!("expected SourceMissing, got {other:?}"),
    }
    assert_nothing_left(into.path(), "missing file");
}

#[test]
fn a_link_in_the_source_is_refused_even_to_the_right_bytes() {
    // The link points at a file with exactly the pinned bytes, so only the
    // link rule can refuse it: a copy through it would verify.
    for (label, link, target) in [
        ("a linked file", "policy.py", "policy.py"),
        ("a linked directory", "assets", "assets"),
    ] {
        let from = source_copy();
        let elsewhere = tempfile::tempdir().unwrap();
        copy_tree(&source(), elsewhere.path());
        let link_path = from.path().join(link);
        if link_path.is_dir() {
            fs::remove_dir_all(&link_path).unwrap();
        } else {
            fs::remove_file(&link_path).unwrap();
        }
        std::os::unix::fs::symlink(elsewhere.path().join(target), &link_path).unwrap();
        let into = import_dir();
        match provision(&args(from.path(), into.path(), &manifest())) {
            Err(ProvisionError::SymbolicLink { path }) => {
                assert!(path.starts_with(link), "{label}: {path}");
            }
            other => panic!("{label}: expected SymbolicLink, got {other:?}"),
        }
        assert_nothing_left(into.path(), label);
    }
}

#[test]
fn an_existing_destination_holding_more_is_refused_and_left_alone() {
    let into = import_dir();
    provision(&args(&source(), into.path(), &manifest())).expect("first run");
    let stray = into.path().join(NAME).join("notes.txt");
    fs::write(&stray, "left by hand").unwrap();
    match provision(&args(&source(), into.path(), &manifest())) {
        Err(ProvisionError::UnlistedFile { path }) => assert_eq!(path, "notes.txt"),
        other => panic!("expected UnlistedFile, got {other:?}"),
    }
    assert!(
        stray.exists(),
        "an operator's directory is never cleaned up for them"
    );
}

#[test]
fn an_existing_destination_is_hashed_and_a_tampered_or_linked_file_refused() {
    for (label, damage) in [
        ("tampered", "tamper"),
        ("a link to the right bytes", "link"),
    ] {
        let into = import_dir();
        provision(&args(&source(), into.path(), &manifest())).expect("first run");
        let file = into.path().join(NAME).join("policy.py");
        let original = fs::read(&file).unwrap();
        if damage == "tamper" {
            let mut bytes = original.clone();
            bytes[0] ^= 0x20;
            fs::write(&file, bytes).unwrap();
        } else {
            let elsewhere = tempfile::tempdir().unwrap();
            let target = elsewhere.path().join("policy.py");
            fs::write(&target, &original).unwrap();
            fs::remove_file(&file).unwrap();
            std::os::unix::fs::symlink(&target, &file).unwrap();
            match provision(&args(&source(), into.path(), &manifest())) {
                Err(ProvisionError::SymbolicLink { path }) => assert_eq!(path, "policy.py"),
                other => panic!("{label}: expected SymbolicLink, got {other:?}"),
            }
            continue;
        }
        match provision(&args(&source(), into.path(), &manifest())) {
            Err(ProvisionError::DigestMismatch { path, .. }) => assert_eq!(path, "policy.py"),
            other => panic!("{label}: expected DigestMismatch, got {other:?}"),
        }
        assert!(file.exists(), "{label}: left as it was found");
    }
}

#[test]
fn a_partial_root_from_an_interrupted_run_is_discarded() {
    let into = import_dir();
    let partial = into.path().join(format!(".{NAME}.partial"));
    fs::create_dir_all(partial.join("assets")).unwrap();
    fs::write(partial.join("policy.py"), "half a file").unwrap();
    provision(&args(&source(), into.path(), &manifest())).expect("starts over");
    assert!(!partial.exists());
    parse_bundle(&into.path().join(NAME)).expect("the result is whole");
}

#[test]
fn files_that_match_the_manifest_but_are_no_bundle_are_refused_and_leave_nothing() {
    // A provisioning manifest pinning exactly these bytes, whose bundle
    // manifest.json declares a digest its artifact does not have: every
    // file verifies, and deploy's own check is what refuses the whole.
    let from = source_copy();
    let bundle_manifest = from.path().join("manifest.json");
    let text = fs::read_to_string(&bundle_manifest).unwrap();
    let digest = format!(
        "{:x}",
        Sha256::digest(fs::read(from.path().join("policy.py")).unwrap())
    );
    fs::write(&bundle_manifest, text.replace(&digest, &"0".repeat(64))).unwrap();
    let manifest_dir = tempfile::tempdir().unwrap();
    let manifest_path = manifest_dir.path().join("manifest.json");
    let entries: Vec<serde_json::Value> = files(from.path())
        .into_iter()
        .map(|file| {
            let bytes = fs::read(from.path().join(&file)).unwrap();
            serde_json::json!({
                "path": file,
                "sha256": format!("{:x}", Sha256::digest(&bytes)),
                "size": bytes.len(),
            })
        })
        .collect();
    fs::write(
        &manifest_path,
        serde_json::json!({"schema_version": "0.1", "bundles": [{"name": NAME, "files": entries}]})
            .to_string(),
    )
    .unwrap();
    let into = import_dir();
    match provision(&args(from.path(), into.path(), &manifest_path)) {
        Err(ProvisionError::BundleRejected { reason }) => {
            assert!(reason.contains("policy.py"), "{reason}");
        }
        other => panic!("expected BundleRejected, got {other:?}"),
    }
    assert_nothing_left(into.path(), "bundle rejected");
}

#[test]
fn manifest_and_destination_problems_are_named() {
    let into = import_dir();
    let mut unknown = args(&source(), into.path(), &manifest());
    unknown.name = "whisper".to_string();
    assert!(matches!(
        provision(&unknown),
        Err(ProvisionError::UnknownBundle { .. })
    ));
    let shipped = args(
        &source(),
        into.path(),
        &repo("packaging/provisioning/manifest.json"),
    );
    match provision(&shipped) {
        Err(ProvisionError::UnknownBundle { available, .. }) => {
            assert!(available.contains("lists no bundles yet"), "{available}");
        }
        other => panic!("expected UnknownBundle, got {other:?}"),
    }
    let broken = tempfile::tempdir().unwrap();
    fs::write(
        broken.path().join("m.json"),
        "{\"schema_version\": \"0.1\"}",
    )
    .unwrap();
    assert!(matches!(
        provision(&args(&source(), into.path(), &broken.path().join("m.json"))),
        Err(ProvisionError::Manifest { .. })
    ));
    assert!(matches!(
        provision(&args(&source(), &into.path().join("absent"), &manifest())),
        Err(ProvisionError::ImportDirMissing { .. })
    ));
    assert_nothing_left(into.path(), "refused before anything was written");
}

#[test]
fn every_provisioning_failure_exits_12_with_its_code_as_context() {
    let err: CliError = ProvisionError::DigestMismatch {
        path: "policy.py".into(),
        expected: "a".repeat(64),
        actual: "b".repeat(64),
    }
    .into();
    assert_eq!(err.exit_code(), ExitCode::ProvisionFailed);
    assert_eq!(err.exit_code() as u8, 12);
    assert_eq!(err.context(), Some("digest_mismatch"));
    assert_eq!(err.protocol_code().as_str(), "load_failed");
}
