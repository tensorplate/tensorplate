// SPDX-License-Identifier: Apache-2.0
//
// V01-E14-F04: parse the shipped packaging config under
// `packaging/conf/agent.json`. Catches drift between the agent's
// runtime config validator and the on-disk default config the
// `tensorplate-agent` Debian package installs to /etc/tensorplate/.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::path::PathBuf;

use tensorplate_agent::config::{AgentConfig, ControlTransport};

fn packaging_agent_config_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("packaging")
        .join("conf")
        .join("agent.json")
}

#[test]
fn shipped_agent_config_parses_and_validates() {
    let p = packaging_agent_config_path();
    let raw = fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
    let cfg = AgentConfig::parse_json(&raw).expect("packaging agent.json should validate");

    assert_eq!(
        cfg.transport,
        ControlTransport::UnixSocket,
        "default install must use the Unix domain socket transport"
    );
    let socket = cfg.socket_path.expect("socket_path required for UDS");
    assert!(
        socket.starts_with(tensorplate_protocol::install_paths::RUN_DIR),
        "agent control socket must live under {} (got {})",
        tensorplate_protocol::install_paths::RUN_DIR,
        socket.display()
    );

    // staging_dir must live under the durable state root so reinstall /
    // upgrade preserves verified bundles.
    assert!(cfg
        .staging_dir
        .starts_with(tensorplate_protocol::install_paths::STATE_DIR));

    // first-run state contract: no active deployment lives in the agent
    // *state* file, not config. The default config must therefore not
    // pin a deployment.
    assert!(
        cfg.available_backends.contains(&"mock".to_string()),
        "default available_backends must include mock so first-run doctor passes"
    );
}

#[test]
fn shipped_agent_config_is_loopback_only() {
    let raw = fs::read_to_string(packaging_agent_config_path()).expect("read agent.json");
    let cfg = AgentConfig::parse_json(&raw).expect("agent.json validates");

    // Even though the default uses UDS, the worker section is loopback
    // for the alternate process mode.
    assert!(
        matches!(
            cfg.worker.serving_bind_host.as_str(),
            "127.0.0.1" | "::1" | "localhost"
        ),
        "worker.serving_bind_host must be loopback in the default install"
    );
}
