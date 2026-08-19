// SPDX-License-Identifier: Apache-2.0
//
// packaging: parse the shipped packaging config under
// `packaging/conf/agent.json`. Catches drift between the agent's
// runtime config validator and the on-disk default config the
// `tensorplate-agent` Debian package installs to /etc/tensorplate/.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::path::PathBuf;

use tensorplate_agent::config::{AgentConfig, ControlTransport, WorkerControlMode};

fn packaging_agent_config_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("packaging")
        .join("conf")
        .join("agent.json")
}

fn homebrew_agent_config_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("packaging")
        .join("homebrew")
        .join("conf")
        .join("agent.json.in")
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
        !cfg.available_backends.contains(&"mock".to_string()),
        "default install must not publish the test-only mock backend"
    );
    assert!(
        cfg.available_backends.contains(&"tensorrt".to_string()),
        "default available_backends must include TensorRT for the Jetson vision validation path"
    );
    assert!(
        cfg.available_backends
            .contains(&"python_pytorch".to_string()),
        "default available_backends must probe the separately-installed Python backend"
    );
    let tensorrt = cfg
        .backend_capabilities
        .get("tensorrt")
        .expect("TensorRT capability must be published");
    assert!(tensorrt.fixed_shape);
    assert!(tensorrt.deterministic_latency);
    assert!(tensorrt
        .supported_artifact_kinds
        .contains(&"tensorrt_engine".to_string()));
    assert!(tensorrt.supported_precision.contains(&"fp16".to_string()));

    let python = cfg
        .backend_capabilities
        .get("python_pytorch")
        .expect("Python/PyTorch capability must be published");
    assert!(python.async_);
    assert!(python.control_loop_integration);
    assert!(python
        .supported_artifact_kinds
        .contains(&"python_pytorch_entry".to_string()));
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
    assert_eq!(
        cfg.worker.mode,
        WorkerControlMode::Process,
        "default install must exercise the agent-supervised serving worker"
    );
    assert_eq!(
        cfg.worker.serving_binary_path.as_deref(),
        Some(std::path::Path::new(
            tensorplate_protocol::install_paths::SERVING_BINARY_PATH
        )),
        "process worker must launch the package-installed serving binary"
    );
}

#[test]
fn homebrew_agent_config_uses_prefix_local_paths() {
    let p = homebrew_agent_config_path();
    let raw = fs::read_to_string(p)
        .expect("read Homebrew agent config")
        .replace("@HOMEBREW_PREFIX@", "/opt/homebrew");
    let cfg = AgentConfig::parse_json(&raw).expect("Homebrew agent config should validate");

    assert_eq!(cfg.transport, ControlTransport::UnixSocket);
    assert_eq!(
        cfg.socket_path.as_deref(),
        Some(std::path::Path::new(
            "/opt/homebrew/var/run/tensorplate/agent.sock"
        ))
    );
    assert!(cfg.state_dir.starts_with("/opt/homebrew/var/tensorplate"));
    assert!(cfg.staging_dir.starts_with("/opt/homebrew/var/tensorplate"));
    assert_eq!(cfg.worker.mode, WorkerControlMode::Process);
    assert_eq!(
        cfg.worker.serving_binary_path.as_deref(),
        Some(std::path::Path::new(
            "/opt/homebrew/opt/tensorplate-serving/libexec/tensorplate-serving"
        ))
    );
    assert_eq!(cfg.available_backends, ["python_pytorch"]);
    assert!(cfg.supervision.is_none());
}
