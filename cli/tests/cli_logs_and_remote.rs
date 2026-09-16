// SPDX-License-Identifier: Apache-2.0
//
// V01-E11-F08 CLI integration: `logs` command + explicit remote URL
// profile semantics.
//
// Covers:
//   - bounded log reads against a local NDJSON file.
//   - `--component` / `--level` filters across mixed entries.
//   - remote profile via --agent-url which fails with a typed transport
//     error when nothing is reachable.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::default_trait_access
)]

mod common;

use std::process::Command;
use tempfile::TempDir;

fn write_log_file(td: &TempDir) -> std::path::PathBuf {
    let path = td.path().join("agent.ndjson");
    std::fs::write(
        &path,
        r#"{"timestamp":"t1","level":"info","component":"agent","event":"start","correlation_id":"c-1"}
{"timestamp":"t2","level":"warn","component":"agent","event":"slow","correlation_id":"c-1"}
{"timestamp":"t3","level":"error","component":"serving","event":"fail","correlation_id":"c-2","message":"oops"}
"#,
    )
    .unwrap();
    path
}

fn write_cli_config_with_log_source(
    td: &TempDir,
    socket: &std::path::Path,
    log_path: &std::path::Path,
) -> std::path::PathBuf {
    let path = td.path().join("cli.json");
    let body = format!(
        r#"{{
            "schema_version":"0.1",
            "default_profile":"local",
            "log_source":{{"kind":"file","path":"{}","tail_default":50}},
            "profiles":{{"local":{{"mode":"local","socket_path":"{}"}}}}
        }}"#,
        log_path.display(),
        socket.display()
    );
    std::fs::write(&path, body).unwrap();
    path
}

#[test]
fn logs_reads_bounded_entries_with_filters() {
    let td = TempDir::new().unwrap();
    let log_path = write_log_file(&td);
    let dummy_socket = td.path().join("agent.sock");
    let cli_cfg = write_cli_config_with_log_source(&td, &dummy_socket, &log_path);
    let out = Command::new(env!("CARGO_BIN_EXE_tensorplate"))
        .env("TENSORPLATE_CLI_CONFIG", &cli_cfg)
        .args(["--output", "json", "logs", "--level", "warn"])
        .output()
        .expect("run");
    assert_eq!(out.status.code().unwrap_or(127), 0);
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let entries = parsed["payload"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "expected warn+error retained");
}

#[test]
fn logs_correlation_filter() {
    let td = TempDir::new().unwrap();
    let log_path = write_log_file(&td);
    let dummy_socket = td.path().join("agent.sock");
    let cli_cfg = write_cli_config_with_log_source(&td, &dummy_socket, &log_path);
    let out = Command::new(env!("CARGO_BIN_EXE_tensorplate"))
        .env("TENSORPLATE_CLI_CONFIG", &cli_cfg)
        .args(["--output", "json", "logs", "--correlation-id", "c-2"])
        .output()
        .expect("run");
    assert_eq!(out.status.code().unwrap_or(127), 0);
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let entries = parsed["payload"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["correlation_id"], "c-2");
}

#[test]
fn remote_profile_via_agent_url_fails_with_transport_when_unreachable() {
    // Pick a definitely-unbound port. The CLI should report a transport
    // failure, not silently fall back to anything.
    let td = TempDir::new().unwrap();
    let dummy_socket = td.path().join("agent.sock");
    let cli_cfg = td.path().join("cli.json");
    let body = format!(
        r#"{{
            "schema_version":"0.1",
            "default_profile":"local",
            "profiles":{{"local":{{"mode":"local","socket_path":"{}"}}}}
        }}"#,
        dummy_socket.display()
    );
    std::fs::write(&cli_cfg, body).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tensorplate"))
        .env("TENSORPLATE_CLI_CONFIG", &cli_cfg)
        .args(["--agent-url", "127.0.0.1:1", "status"])
        .output()
        .expect("run");
    let code = out.status.code().unwrap_or(127);
    // Transport exit code.
    assert_eq!(
        code,
        4,
        "stderr was {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn logs_remote_profile_is_unavailable() {
    let td = TempDir::new().unwrap();
    let log_path = write_log_file(&td);
    let cli_cfg = td.path().join("cli.json");
    let body = format!(
        r#"{{
            "schema_version":"0.1",
            "default_profile":"remote",
            "log_source":{{"kind":"file","path":"{}","tail_default":50}},
            "profiles":{{"remote":{{"mode":"url","agent_url":"127.0.0.1:1"}}}}
        }}"#,
        log_path.display(),
    );
    std::fs::write(&cli_cfg, body).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tensorplate"))
        .env("TENSORPLATE_CLI_CONFIG", &cli_cfg)
        .args(["logs"])
        .output()
        .expect("run");
    assert_eq!(out.status.code().unwrap_or(127), 6);
}

/// On a native install nothing writes an NDJSON log, so the packaged
/// config names no `log_source.path`. The command must say where the logs
/// are instead of printing nothing or telling the operator to configure a
/// path no component writes.
#[test]
fn logs_without_a_source_is_unavailable_and_says_what_to_use() {
    let td = TempDir::new().unwrap();
    let dummy_socket = td.path().join("agent.sock");
    let cli_cfg = td.path().join("cli.json");
    let body = format!(
        r#"{{
            "schema_version":"0.1",
            "default_profile":"local",
            "log_source":{{"kind":"file","tail_default":100}},
            "profiles":{{"local":{{"mode":"local","socket_path":"{}"}}}}
        }}"#,
        dummy_socket.display()
    );
    std::fs::write(&cli_cfg, body).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tensorplate"))
        .env("TENSORPLATE_CLI_CONFIG", &cli_cfg)
        .args(["--output", "json", "logs", "--component", "agent"])
        .output()
        .expect("run");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code().unwrap_or(127), 6, "stderr was {stderr}");
    // The whole failure envelope goes to stderr; stdout stays empty so a
    // caller parsing `--output json` from stdout is not handed a document
    // that says a successful read returned nothing.
    assert!(out.stdout.is_empty(), "stdout was {:?}", out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stderr)
        .unwrap_or_else(|e| panic!("stderr must be a JSON envelope ({e}): {stderr}"));
    assert_eq!(parsed["status"], "unavailable");
    let hint = parsed["error"]["hint"]
        .as_str()
        .unwrap_or_else(|| panic!("the failure must carry a hint: {stderr}"));
    assert!(hint.contains("--source"), "{hint}");
    #[cfg(target_os = "linux")]
    assert!(hint.contains("journalctl -u tensorplate-agent"), "{hint}");
}

#[test]
fn unsupported_profile_mode_returns_unavailable() {
    let td = TempDir::new().unwrap();
    let cli_cfg = td.path().join("cli.json");
    let body = r#"{
        "schema_version":"0.1",
        "default_profile":"relay",
        "profiles":{"relay":{"mode":"relay"}}
    }"#;
    std::fs::write(&cli_cfg, body).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tensorplate"))
        .env("TENSORPLATE_CLI_CONFIG", &cli_cfg)
        .args(["status"])
        .output()
        .expect("run");
    assert_eq!(out.status.code().unwrap_or(127), 6);
}
