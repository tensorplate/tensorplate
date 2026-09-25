// SPDX-License-Identifier: Apache-2.0
//
// `tensorplate bundle provision` through the binary: argument parsing, the
// JSON envelope, exit code 12 for a provisioning failure, the refusal to
// route it to a device, and modes that do not depend on the umask it runs
// under. The outcomes themselves are covered in bundle_provision.rs.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn repo(relative: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(relative)
        .display()
        .to_string()
}

fn run(args: &[&str]) -> (i32, String, String) {
    // No agent is involved; the socket path is never opened.
    common::run_cli(std::path::Path::new("/nonexistent/agent.sock"), args)
}

#[test]
fn a_provisioned_bundle_is_reported_in_the_json_envelope() {
    let into = tempfile::tempdir().unwrap();
    let source = repo("test/models/bundles/v0_1/smolvla_python_pytorch");
    let manifest = repo("protocol/rust/tests/fixtures/provisioning_manifest.json");
    let into_path = into.path().display().to_string();
    let (code, stdout, stderr) = run(&[
        "--output",
        "json",
        "bundle",
        "provision",
        "smolvla-fixture",
        "--from",
        &source,
        "--manifest",
        &manifest,
        "--into",
        &into_path,
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let envelope: Value = serde_json::from_str(&stdout).expect("one JSON envelope");
    assert_eq!(envelope["command"], "bundle");
    assert_eq!(envelope["status"], "ok");
    assert_eq!(envelope["payload"]["outcome"], "provisioned");
    assert_eq!(envelope["payload"]["files"], 4);
}

#[test]
fn a_provisioning_failure_exits_12_with_its_code() {
    let into = tempfile::tempdir().unwrap();
    let empty_source = tempfile::tempdir().unwrap();
    let manifest = repo("protocol/rust/tests/fixtures/provisioning_manifest.json");
    let (from, into_path) = (
        empty_source.path().display().to_string(),
        into.path().display().to_string(),
    );
    let (code, _stdout, stderr) = run(&[
        "--output",
        "json",
        "bundle",
        "provision",
        "smolvla-fixture",
        "--from",
        &from,
        "--manifest",
        &manifest,
        "--into",
        &into_path,
    ]);
    assert_eq!(code, 12, "stderr: {stderr}");
    let envelope: Value = serde_json::from_str(&stderr).expect("one JSON error envelope");
    assert_eq!(envelope["error"]["context"], "source_missing");
    assert_eq!(envelope["error"]["code"], "load_failed");
}

#[test]
fn provisioning_is_never_routed_to_a_device_and_needs_a_source() {
    let (code, _stdout, stderr) = run(&[
        "--device",
        "orin",
        "bundle",
        "provision",
        "smolvla-fixture",
        "--from",
        "/tmp",
    ]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(stderr.contains("run it on the device"), "{stderr}");

    let (code, _stdout, stderr) = run(&["bundle", "provision", "smolvla-fixture"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("requires `--from <dir>`"), "{stderr}");

    let (code, _stdout, stderr) = run(&["bundle", "fetch"]);
    assert_eq!(code, 2);
    assert!(
        stderr.contains("unknown bundle subcommand `fetch`"),
        "{stderr}"
    );
}

#[test]
fn a_usage_error_is_reported_as_the_bundle_command() {
    // Even when an argument is another command's name.
    for args in [
        ["--output", "json", "bundle", "provision", "smolvla-fixture"],
        ["--output", "json", "bundle", "provision", "deploy"],
    ] {
        let (code, _stdout, stderr) = run(&args);
        assert_eq!(code, 2, "stderr: {stderr}");
        let envelope: Value = serde_json::from_str(&stderr).expect("one JSON error envelope");
        assert_eq!(envelope["command"], "bundle", "{args:?}: {stderr}");
    }
}

/// Every directory and file under `root`, with its permission bits.
fn modes(root: &Path) -> Vec<(String, bool, u32)> {
    let mut out = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let meta = std::fs::symlink_metadata(&path).unwrap();
        if meta.is_dir() {
            for entry in std::fs::read_dir(&path).unwrap() {
                pending.push(entry.unwrap().path());
            }
        }
        out.push((
            path.display().to_string(),
            meta.is_dir(),
            meta.permissions().mode() & 0o7777,
        ));
    }
    out
}

#[test]
fn the_provisioned_bundle_is_readable_by_the_agent_and_writable_only_by_its_owner_whatever_the_umask(
) {
    // The agent reads a bundle through its other bits. A restrictive umask
    // must not hide it from the agent, and a permissive one must not open
    // it to writers.
    let source = repo("test/models/bundles/v0_1/smolvla_python_pytorch");
    let manifest = repo("protocol/rust/tests/fixtures/provisioning_manifest.json");
    for umask in ["077", "000"] {
        let into = tempfile::tempdir().unwrap();
        let config = common::write_default_cli_config(Path::new("/nonexistent/agent.sock"));
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("umask {umask}; exec \"$0\" \"$@\""))
            .arg(env!("CARGO_BIN_EXE_tensorplate"))
            .args(["bundle", "provision", "smolvla-fixture", "--from"])
            .arg(&source)
            .arg("--manifest")
            .arg(&manifest)
            .arg("--into")
            .arg(into.path())
            .env("TENSORPLATE_CLI_CONFIG", &config.config_path)
            .output()
            .expect("run cli");
        assert!(
            out.status.success(),
            "umask {umask}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let provisioned = modes(&into.path().join("smolvla-fixture"));
        assert_eq!(provisioned.len(), 6, "umask {umask}: {provisioned:?}");
        for (path, is_dir, mode) in provisioned {
            let expected = if is_dir { 0o755 } else { 0o644 };
            assert_eq!(mode, expected, "umask {umask}: {path} is {mode:o}");
        }
    }
}
