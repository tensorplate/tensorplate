// SPDX-License-Identifier: Apache-2.0
//
// V01-E14-F04: parse the shipped packaging config under
// `packaging/conf/cli.json`. Confirms the default profile uses the
// agent's Unix domain socket and that the CLI binary picks up the
// shipped config without any env override.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::PathBuf;

use tensorplate_cli::{CliConfig, ProfileMode};

fn packaging_cli_config_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("packaging")
        .join("conf")
        .join("cli.json")
}

#[test]
fn shipped_cli_config_parses_and_validates() {
    let raw = fs::read_to_string(packaging_cli_config_path()).expect("read cli.json");
    let cfg = CliConfig::parse_json(&raw).expect("packaging cli.json should validate");

    assert_eq!(cfg.default_profile, "local");
    let local = cfg
        .profiles
        .get("local")
        .expect("local profile must be present");
    assert!(matches!(local.mode, ProfileMode::Local));
    let socket = local
        .socket_path
        .as_ref()
        .expect("local profile must define socket_path");
    assert_eq!(socket, std::path::Path::new("/run/tensorplate/agent.sock"));
}
