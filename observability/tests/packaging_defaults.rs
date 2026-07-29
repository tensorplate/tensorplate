// SPDX-License-Identifier: Apache-2.0
//
// packaging: parse the shipped packaging config under
// `packaging/conf/observability.json`. Asserts the default install
// stays local-only and matches the documented filesystem layout.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::PathBuf;

use tensorplate_observability::config::{ListenerTransport, ObservabilityConfig};

fn packaging_config_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("packaging")
        .join("conf")
        .join("observability.json")
}

fn homebrew_config_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("packaging")
        .join("homebrew")
        .join("conf")
        .join("observability.json.in")
}

#[test]
fn shipped_observability_config_parses_and_validates() {
    let raw = fs::read_to_string(packaging_config_path()).expect("read observability.json");
    let cfg = ObservabilityConfig::parse_json(&raw)
        .expect("packaging observability.json should validate");

    // listener stays in_process by default in v0.1.0 (the unix_socket
    // path is reserved).
    assert!(matches!(
        cfg.listener.transport,
        ListenerTransport::InProcess
    ));

    // ROS 2 health stub is OFF by default; sites opt-in via drop-in.
    assert!(
        !cfg.ros2_health.enabled,
        "default install must not enable the ROS 2 health stub"
    );

    // snapshot file path lives under /var/lib/tensorplate/state so the
    // CLI status command can read it without escaping the install
    // layout.
    let snap_path = cfg.snapshot.path.as_ref().expect("snapshot.path set");
    assert!(snap_path.starts_with(tensorplate_protocol::install_paths::STATE_INNER_DIR));
}

#[test]
fn homebrew_observability_config_uses_prefix_local_state_and_logs() {
    let p = homebrew_config_path();
    let raw = fs::read_to_string(p)
        .expect("read Homebrew observability config")
        .replace("@HOMEBREW_PREFIX@", "/opt/homebrew");
    let cfg = ObservabilityConfig::parse_json(&raw)
        .expect("Homebrew observability config should validate");

    assert_eq!(
        cfg.snapshot.path.as_deref(),
        Some(std::path::Path::new(
            "/opt/homebrew/var/tensorplate/state/observability-snapshot.json"
        ))
    );
    assert_eq!(
        cfg.diagnostics_retention.file_path.as_deref(),
        Some(std::path::Path::new(
            "/opt/homebrew/var/log/tensorplate/events.ndjson"
        ))
    );
}
