// SPDX-License-Identifier: Apache-2.0
//
// `tensorplate bundle provision`: every outcome against the committed
// provisioning manifest fixture, whose one bundle is the committed
// `test/models/bundles/v0_1/smolvla_python_pytorch` bundle file for file.
// Each failure is checked for its own reason. A failure while copying
// leaves nothing in the import directory; a failure against an existing
// destination leaves it exactly as it was found.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};
use tensorplate_cli::args::ProvisionArgs;
use tensorplate_cli::commands::bundle::{provision, ProvisionError};
use tensorplate_cli::{CliError, ExitCode};
use tensorplate_protocol::bundle::parse_bundle;
use tensorplate_protocol::install_paths::BUNDLE_IMPORT_DIR;

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

/// One entry's kind, inode, modification time (seconds, nanoseconds), mode
/// and bytes (a link's target, a file's content).
type Entry = (String, u64, i64, i64, u32, Vec<u8>);

/// Everything under `root`, links not followed. Equal snapshots mean
/// nothing was written, replaced or removed.
fn snapshot(root: &Path) -> BTreeMap<String, Entry> {
    let mut out = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let meta = fs::symlink_metadata(&path).expect("lstat");
        let kind = meta.file_type();
        let (label, bytes) = if kind.is_symlink() {
            (
                "link",
                fs::read_link(&path)
                    .unwrap()
                    .into_os_string()
                    .into_encoded_bytes(),
            )
        } else if kind.is_dir() {
            for entry in fs::read_dir(&path).expect("read") {
                pending.push(entry.expect("entry").path());
            }
            ("dir", Vec::new())
        } else if kind.is_file() {
            ("file", fs::read(&path).unwrap())
        } else {
            ("other", Vec::new())
        };
        out.insert(
            path.strip_prefix(root).unwrap().display().to_string(),
            (
                label.to_string(),
                meta.ino(),
                meta.mtime(),
                meta.mtime_nsec(),
                meta.mode(),
                bytes,
            ),
        );
    }
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

fn mkfifo(path: &Path) {
    let status = Command::new("mkfifo").arg(path).status().expect("mkfifo");
    assert!(status.success(), "mkfifo {}", path.display());
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
    parse_bundle(&destination).expect("deploy's bundle parser accepts it");
    let entries: Vec<String> = fs::read_dir(into.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, [NAME], "no partial root is left beside it");
}

#[test]
fn a_second_run_verifies_in_place_and_writes_nothing() {
    let into = import_dir();
    provision(&args(&source(), into.path(), &manifest())).expect("first run");
    let before = snapshot(into.path());
    let report = provision(&args(&source(), into.path(), &manifest())).expect("second run");
    assert!(report.already_provisioned);
    assert_eq!(snapshot(into.path()), before, "nothing was written");
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
    // Larger than pinned: refused at its full size, before a byte of it is
    // read, not after reading one byte past the pin.
    let from = source_copy();
    let path = from.path().join("assets/tokenizer.json");
    let mut bytes = fs::read(&path).unwrap();
    let pinned = bytes.len() as u64;
    bytes.extend_from_slice(&[b' '; 100]);
    fs::write(&path, bytes).unwrap();
    let into = import_dir();
    match provision(&args(from.path(), into.path(), &manifest())) {
        Err(ProvisionError::SizeMismatch {
            path,
            expected,
            actual,
        }) => {
            assert_eq!(path, "assets/tokenizer.json");
            assert_eq!((expected, actual), (pinned, pinned + 100));
        }
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
fn a_source_path_that_is_not_a_regular_file_is_refused_without_opening_it() {
    // A directory is not a file; a FIFO would block an open forever, so it
    // comes second.
    for label in ["a directory", "a FIFO"] {
        let from = source_copy();
        let path = from.path().join("policy.py");
        fs::remove_file(&path).unwrap();
        if label == "a FIFO" {
            mkfifo(&path);
        } else {
            fs::create_dir(&path).unwrap();
        }
        let into = import_dir();
        match provision(&args(from.path(), into.path(), &manifest())) {
            Err(ProvisionError::NotRegular { path }) => assert_eq!(path, "policy.py", "{label}"),
            other => panic!("{label}: expected NotRegular, got {other:?}"),
        }
        assert_nothing_left(into.path(), label);
    }
}

#[test]
fn an_unreadable_source_is_an_io_failure_and_leaves_nothing() {
    // A file where the source directory should be: every listed path under
    // it fails to resolve with an error that is not "not found".
    let not_a_dir = tempfile::NamedTempFile::new().unwrap();
    let into = import_dir();
    match provision(&args(not_a_dir.path(), into.path(), &manifest())) {
        Err(ProvisionError::Io { .. }) => {}
        other => panic!("expected Io, got {other:?}"),
    }
    assert_nothing_left(into.path(), "io");
}

/// Provision the fixture into a fresh import directory, damage the
/// destination with `damage`, and return the import directory.
fn damaged_destination(damage: impl FnOnce(&Path)) -> tempfile::TempDir {
    let into = import_dir();
    provision(&args(&source(), into.path(), &manifest())).expect("first run");
    damage(&into.path().join(NAME));
    into
}

#[test]
fn an_existing_destination_that_is_not_this_bundle_is_refused_and_left_as_found() {
    let elsewhere = tempfile::tempdir().unwrap();
    copy_tree(&source(), elsewhere.path());
    let cases: Vec<(&str, tempfile::TempDir, &str)> = vec![
        (
            "an unlisted file",
            damaged_destination(|dest| fs::write(dest.join("notes.txt"), "left by hand").unwrap()),
            "`notes.txt` is not in the provisioning manifest",
        ),
        (
            // The one listed file deploy's bundle parser does not hash:
            // one byte changed, same size, still a manifest it accepts.
            "a tampered manifest.json",
            damaged_destination(|dest| {
                let path = dest.join("manifest.json");
                let text = fs::read_to_string(&path).unwrap();
                fs::write(&path, text.replacen('\n', " ", 1)).unwrap();
                parse_bundle(dest).expect("deploy's parser alone would accept this");
            }),
            "`manifest.json` has SHA-256",
        ),
        (
            "a tampered artifact",
            damaged_destination(|dest| {
                let path = dest.join("policy.py");
                let mut bytes = fs::read(&path).unwrap();
                bytes[0] ^= 0x20;
                fs::write(&path, bytes).unwrap();
            }),
            "`policy.py` has SHA-256",
        ),
        (
            "a file of another size",
            damaged_destination(|dest| {
                fs::write(dest.join("assets/tokenizer.json"), "{}").unwrap();
            }),
            "`assets/tokenizer.json` is 2 bytes",
        ),
        (
            "a missing file",
            damaged_destination(|dest| fs::remove_file(dest.join("policy.py")).unwrap()),
            "`policy.py` is missing",
        ),
        (
            "a link to the right bytes",
            damaged_destination(|dest| {
                let path = dest.join("policy.py");
                fs::remove_file(&path).unwrap();
                std::os::unix::fs::symlink(elsewhere.path().join("policy.py"), &path).unwrap();
            }),
            "`policy.py` is a symbolic link",
        ),
        (
            "a FIFO",
            damaged_destination(|dest| mkfifo(&dest.join("pipe"))),
            "`pipe` is not a regular file",
        ),
        (
            // One file whose name holds a backslash, beside the listed
            // nested file it would spell if the backslash were a separator.
            "a name with a backslash",
            damaged_destination(|dest| {
                fs::copy(
                    dest.join("assets/tokenizer.json"),
                    dest.join("assets\\tokenizer.json"),
                )
                .unwrap();
            }),
            "`assets\\tokenizer.json` is not in the provisioning manifest",
        ),
        (
            "a link to a whole, correct bundle",
            {
                let into = import_dir();
                std::os::unix::fs::symlink(elsewhere.path(), into.path().join(NAME)).unwrap();
                into
            },
            "it is a symbolic link",
        ),
        (
            "a file",
            {
                let into = import_dir();
                fs::write(into.path().join(NAME), "not a bundle").unwrap();
                into
            },
            "it is not a directory",
        ),
    ];
    for (label, into, reason) in cases {
        let before = snapshot(into.path());
        match provision(&args(&source(), into.path(), &manifest())) {
            Err(ProvisionError::DestinationMismatch { path, reason: said }) => {
                assert_eq!(path, into.path().join(NAME), "{label}");
                assert!(said.contains(reason), "{label}: {said}");
            }
            other => panic!("{label}: expected DestinationMismatch, got {other:?}"),
        }
        assert_eq!(snapshot(into.path()), before, "{label}: left as found");
    }
}

#[test]
fn a_partial_root_that_exists_is_refused_and_kept() {
    // Another run's, live or interrupted: neither used nor removed.
    let into = import_dir();
    let partial = into.path().join(format!(".{NAME}.partial"));
    fs::create_dir_all(partial.join("assets")).unwrap();
    fs::write(partial.join("policy.py"), "half a file").unwrap();
    let before = snapshot(into.path());
    match provision(&args(&source(), into.path(), &manifest())) {
        Err(ProvisionError::PartialRootExists { path }) => assert_eq!(path, partial),
        other => panic!("expected PartialRootExists, got {other:?}"),
    }
    assert_eq!(
        snapshot(into.path()),
        before,
        "the other run's root is untouched"
    );
}

/// A source whose files a provisioning manifest pins exactly, but whose
/// bundle manifest.json declares a digest its artifact does not have: every
/// file verifies, and only deploy's bundle parser refuses the whole.
/// Returns the source and the directory holding that provisioning manifest.
fn no_bundle_source() -> (tempfile::TempDir, tempfile::TempDir) {
    let from = source_copy();
    let bundle_manifest = from.path().join("manifest.json");
    let text = fs::read_to_string(&bundle_manifest).unwrap();
    let digest = format!(
        "{:x}",
        Sha256::digest(fs::read(from.path().join("policy.py")).unwrap())
    );
    fs::write(&bundle_manifest, text.replace(&digest, &"0".repeat(64))).unwrap();
    let manifest_dir = tempfile::tempdir().unwrap();
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
        manifest_dir.path().join("manifest.json"),
        serde_json::json!({"schema_version": "0.1", "bundles": [{"name": NAME, "files": entries}]})
            .to_string(),
    )
    .unwrap();
    (from, manifest_dir)
}

#[test]
fn files_that_match_the_manifest_but_are_no_bundle_are_refused_and_leave_nothing() {
    let (from, manifest_dir) = no_bundle_source();
    let into = import_dir();
    match provision(&args(
        from.path(),
        into.path(),
        &manifest_dir.path().join("manifest.json"),
    )) {
        Err(ProvisionError::BundleRejected { reason }) => {
            assert!(reason.contains("policy.py"), "{reason}");
        }
        other => panic!("expected BundleRejected, got {other:?}"),
    }
    assert_nothing_left(into.path(), "bundle rejected");
}

#[test]
fn an_existing_destination_the_bundle_parser_refuses_is_refused_and_left_as_found() {
    // Every listed file matches its pin; the bundle parser is what refuses.
    let (from, manifest_dir) = no_bundle_source();
    let into = import_dir();
    copy_tree(from.path(), &{
        let destination = into.path().join(NAME);
        fs::create_dir(&destination).unwrap();
        destination
    });
    let before = snapshot(into.path());
    match provision(&args(
        from.path(),
        into.path(),
        &manifest_dir.path().join("manifest.json"),
    )) {
        Err(ProvisionError::DestinationMismatch { reason, .. }) => {
            assert!(
                reason.contains("deploy's bundle parser refuses it"),
                "{reason}"
            );
        }
        other => panic!("expected DestinationMismatch, got {other:?}"),
    }
    assert_eq!(snapshot(into.path()), before, "left as found");
}

#[test]
fn manifest_and_import_directory_problems_are_named() {
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
        Err(ProvisionError::ImportDirMissing { given: true, .. })
    ));
    // A link to a real directory is not followed.
    let real = tempfile::tempdir().unwrap();
    let link = into.path().join("import-link");
    std::os::unix::fs::symlink(real.path(), &link).unwrap();
    assert!(matches!(
        provision(&args(&source(), &link, &manifest())),
        Err(ProvisionError::ImportDirMissing { given: true, .. })
    ));
    fs::remove_file(&link).unwrap();
    // Without --into the packaged import directory is used; on a host
    // without the agent package it is named as missing, not given.
    if !Path::new(BUNDLE_IMPORT_DIR).exists() {
        let mut default_into = args(&source(), into.path(), &manifest());
        default_into.into = None;
        assert!(matches!(
            provision(&default_into),
            Err(ProvisionError::ImportDirMissing { given: false, .. })
        ));
    }
    assert_nothing_left(into.path(), "refused before anything was written");
}

#[test]
fn every_provisioning_failure_exits_12_with_its_token_and_protocol_code() {
    let text = String::new;
    let path = PathBuf::new;
    for (err, token, protocol) in [
        (
            ProvisionError::Manifest {
                path: path(),
                reason: text(),
            },
            "manifest_invalid",
            "config_invalid",
        ),
        (
            ProvisionError::UnknownBundle {
                name: text(),
                available: text(),
            },
            "unknown_bundle",
            "config_invalid",
        ),
        (
            ProvisionError::ImportDirMissing {
                path: path(),
                given: false,
            },
            "import_dir_missing",
            "internal",
        ),
        (
            ProvisionError::PartialRootExists { path: path() },
            "partial_root_exists",
            "load_failed",
        ),
        (
            ProvisionError::SourceMissing { path: text() },
            "source_missing",
            "load_failed",
        ),
        (
            ProvisionError::SymbolicLink { path: text() },
            "symbolic_link",
            "load_failed",
        ),
        (
            ProvisionError::NotRegular { path: text() },
            "not_regular",
            "load_failed",
        ),
        (
            ProvisionError::Changed { path: text() },
            "changed_during_run",
            "load_failed",
        ),
        (
            ProvisionError::SizeMismatch {
                path: text(),
                expected: 1,
                actual: 2,
            },
            "size_mismatch",
            "load_failed",
        ),
        (
            ProvisionError::DigestMismatch {
                path: text(),
                expected: text(),
                actual: text(),
            },
            "digest_mismatch",
            "load_failed",
        ),
        (
            ProvisionError::BundleRejected { reason: text() },
            "bundle_rejected",
            "load_failed",
        ),
        (
            ProvisionError::DestinationMismatch {
                path: path(),
                reason: text(),
            },
            "destination_mismatch",
            "load_failed",
        ),
        (
            ProvisionError::Io {
                what: text(),
                detail: text(),
            },
            "io",
            "internal",
        ),
    ] {
        let err: CliError = err.into();
        assert_eq!(err.exit_code(), ExitCode::ProvisionFailed, "{token}");
        assert_eq!(err.exit_code() as u8, 12, "{token}");
        assert_eq!(err.context(), Some(token));
        assert_eq!(err.protocol_code().as_str(), protocol, "{token}");
    }
}
