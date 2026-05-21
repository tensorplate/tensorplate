// SPDX-License-Identifier: Apache-2.0
//
// V01-E14-F04: parse the shipped packaging config under
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
