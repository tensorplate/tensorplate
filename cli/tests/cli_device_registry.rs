// SPDX-License-Identifier: Apache-2.0
//
// CLI integration: the `device` registry command group.
//
// Exercises the full binary end-to-end against a temp registry redirected via
// $TENSORPLATE_DEVICE_REGISTRY. Device commands never contact the agent, so no
// stub agent is needed — the socket path only feeds the default CLI config the
// harness writes and is never connected.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::default_trait_access
)]

mod common;

use common::run_cli_with_extra_env;
use tempfile::TempDir;

fn run_device(registry: &std::path::Path, args: &[&str]) -> (i32, String, String) {
    let socket = registry.parent().unwrap().join("agent.sock");
    let reg = registry.to_string_lossy().into_owned();
    run_cli_with_extra_env(
        &socket,
        args,
        &[("TENSORPLATE_DEVICE_REGISTRY", reg.as_str())],
    )
}

fn read_registry(registry: &std::path::Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(registry).expect("registry file");
    serde_json::from_str(&raw).expect("registry json")
}

#[test]
fn add_first_device_becomes_default_and_list_reports_it() {
    let td = TempDir::new().unwrap();
    let registry = td.path().join("devices.json");

    let (code, out, _err) = run_device(
        &registry,
        &[
            "device",
            "add",
            "orin",
            "--ssh",
            "reid@orin.local",
            "--no-verify",
        ],
    );
    assert_eq!(code, 0, "add failed: {out}");
    let parsed = read_registry(&registry);
    assert_eq!(parsed["default_device"], "orin");
    assert_eq!(parsed["devices"]["orin"]["ssh_target"], "reid@orin.local");
    // Enrollment records the import dir the remote deploy path needs.
    assert_eq!(
        parsed["devices"]["orin"]["remote_import_dir"],
        "/var/lib/tensorplate/bundles/import"
    );

    let (code, out, _err) = run_device(&registry, &["device", "list", "--output", "json"]);
    assert_eq!(code, 0);
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("list json");
    assert_eq!(parsed["command"], "device");
    assert_eq!(parsed["payload"]["default_device"], "orin");
    assert_eq!(parsed["payload"]["devices"][0]["name"], "orin");
    assert_eq!(parsed["payload"]["devices"][0]["default"], true);
    assert_eq!(
        parsed["payload"]["devices"][0]["remote_import_dir"],
        "/var/lib/tensorplate/bundles/import"
    );
}

#[test]
fn second_add_keeps_default_until_use_switches_it() {
    let td = TempDir::new().unwrap();
    let registry = td.path().join("devices.json");

    run_device(
        &registry,
        &[
            "device",
            "add",
            "orin",
            "--ssh",
            "reid@orin.local",
            "--no-verify",
        ],
    );
    let (code, _out, err) = run_device(
        &registry,
        &[
            "device",
            "add",
            "nano",
            "--ssh",
            "reid@nano.local",
            "--no-verify",
        ],
    );
    assert_eq!(code, 0);
    assert!(err.contains("device use nano"), "missing hint: {err}");
    assert_eq!(read_registry(&registry)["default_device"], "orin");

    let (code, _out, _err) = run_device(&registry, &["device", "use", "nano"]);
    assert_eq!(code, 0);
    assert_eq!(read_registry(&registry)["default_device"], "nano");
}

#[test]
fn remove_default_clears_default() {
    let td = TempDir::new().unwrap();
    let registry = td.path().join("devices.json");

    run_device(
        &registry,
        &[
            "device",
            "add",
            "orin",
            "--ssh",
            "reid@orin.local",
            "--no-verify",
        ],
    );
    let (code, _out, _err) = run_device(&registry, &["device", "remove", "orin"]);
    assert_eq!(code, 0);
    let parsed = read_registry(&registry);
    assert!(parsed["devices"].as_object().unwrap().is_empty());
    assert!(parsed["default_device"].is_null());
}

#[test]
fn rename_moves_entry_and_follows_default() {
    let td = TempDir::new().unwrap();
    let registry = td.path().join("devices.json");

    run_device(
        &registry,
        &[
            "device",
            "add",
            "orin",
            "--ssh",
            "reid@orin.local",
            "--no-verify",
        ],
    );
    let (code, _out, _err) = run_device(&registry, &["device", "rename", "orin", "orin-lab"]);
    assert_eq!(code, 0);
    let parsed = read_registry(&registry);
    assert_eq!(parsed["default_device"], "orin-lab");
    assert!(parsed["devices"]["orin-lab"].is_object());
    assert!(parsed["devices"]["orin"].is_null());
}

#[test]
fn add_rejects_unsafe_ssh_target_and_leaves_no_registry() {
    let td = TempDir::new().unwrap();
    let registry = td.path().join("devices.json");

    let (code, _out, _err) = run_device(
        &registry,
        &["device", "add", "bad", "--ssh", "reid@orin.local; rm -rf /"],
    );
    // Usage/config rejection maps to exit code 2, and nothing is persisted.
    assert_eq!(code, 2);
    assert!(!registry.exists());
}

#[test]
fn device_without_subcommand_is_usage_error() {
    let td = TempDir::new().unwrap();
    let registry = td.path().join("devices.json");
    let (code, _out, _err) = run_device(&registry, &["device"]);
    assert_eq!(code, 2);
}

#[test]
fn operational_command_with_unenrolled_device_is_usage_error() {
    // `--device <name> status` fails closed before any SSH when the device is
    // not enrolled.
    let td = TempDir::new().unwrap();
    let registry = td.path().join("devices.json");
    let (code, _out, err) = run_device(&registry, &["--device", "ghost", "status"]);
    assert_eq!(code, 2, "err={err}");
    assert!(err.contains("not enrolled"), "err={err}");
}

#[test]
fn device_commands_work_with_unsupported_default_profile() {
    // Device management is local-only: a reserved/unsupported default profile
    // in the CLI config must not block it (no transport is resolved).
    let td = TempDir::new().unwrap();
    let registry = td.path().join("devices.json");
    run_device(
        &registry,
        &[
            "device",
            "add",
            "orin",
            "--ssh",
            "reid@orin.local",
            "--no-verify",
        ],
    );

    let cli_cfg = td.path().join("cli.json");
    std::fs::write(
        &cli_cfg,
        r#"{"schema_version":"0.1","default_profile":"relay","profiles":{"relay":{"mode":"relay"}}}"#,
    )
    .expect("write relay cli config");
    let socket = td.path().join("agent.sock");
    let reg = registry.to_string_lossy().into_owned();
    let cfg = cli_cfg.to_string_lossy().into_owned();
    let (code, out, _err) = run_cli_with_extra_env(
        &socket,
        &["device", "list", "--output", "json"],
        &[
            ("TENSORPLATE_DEVICE_REGISTRY", reg.as_str()),
            ("TENSORPLATE_CLI_CONFIG", cfg.as_str()),
        ],
    );
    assert_eq!(
        code, 0,
        "device list must ignore the unsupported profile: {out}"
    );
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("list json");
    assert_eq!(parsed["payload"]["devices"][0]["name"], "orin");
}
