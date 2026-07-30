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

fn packaging_conf_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("packaging")
        .join("conf")
        .join(name)
}

fn packaging_agent_config_path() -> PathBuf {
    packaging_conf_path("agent.json")
}

/// The x86_64 variant the `tensorplate-agent` package installs to the same
/// `/etc/tensorplate/agent.json` path on amd64 hosts.
fn packaging_agent_config_path_amd64() -> PathBuf {
    packaging_conf_path("agent.amd64.json")
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
fn shipped_amd64_agent_config_parses_and_declares_only_built_backends() {
    let p = packaging_agent_config_path_amd64();
    let raw = fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
    let cfg = AgentConfig::parse_json(&raw).expect("packaging agent.amd64.json should validate");

    // The x86_64 serving worker is built without the TensorRT adapter, so the
    // agent must not advertise a backend it cannot resolve. Advertising it
    // would admit a TensorRT bundle at the compatibility check and fail later
    // inside the worker, which is the opposite of failing closed.
    assert!(
        !cfg.available_backends.contains(&"tensorrt".to_string()),
        "the x86_64 install must not advertise TensorRT; that build has no TensorRT adapter"
    );
    assert!(
        !cfg.backend_capabilities.contains_key("tensorrt"),
        "the x86_64 install must not publish TensorRT capabilities"
    );
    assert!(
        cfg.available_backends
            .contains(&"python_pytorch".to_string()),
        "the x86_64 deploy-smoke path is the Python/PyTorch sidecar"
    );
    assert!(
        !cfg.available_backends.contains(&"mock".to_string()),
        "no install may publish the test-only mock backend"
    );

    // Every advertised backend needs a capability record, or the agent
    // admits a bundle it cannot describe.
    for backend in &cfg.available_backends {
        assert!(
            cfg.backend_capabilities.contains_key(backend),
            "backend {backend} is advertised without a capability record"
        );
    }
}

#[test]
fn agent_configs_differ_only_where_the_architecture_forces_it() {
    let arm = fs::read_to_string(packaging_agent_config_path()).expect("read agent.json");
    let amd = fs::read_to_string(packaging_agent_config_path_amd64()).expect("read amd64 config");
    let arm = AgentConfig::parse_json(&arm).expect("agent.json validates");
    let amd = AgentConfig::parse_json(&amd).expect("agent.amd64.json validates");

    // Both variants install to the same conffile path, so anything that is
    // not architecture-determined must stay identical — otherwise the two
    // hosts quietly run different control planes.
    assert_eq!(arm.transport, amd.transport);
    assert_eq!(arm.socket_path, amd.socket_path);
    assert_eq!(arm.state_dir, amd.state_dir);
    assert_eq!(arm.staging_dir, amd.staging_dir);
    assert_eq!(arm.worker.mode, amd.worker.mode);
    assert_eq!(
        arm.worker.serving_binary_path,
        amd.worker.serving_binary_path
    );
    assert_eq!(arm.worker.serving_bind_host, amd.worker.serving_bind_host);
    assert_eq!(arm.worker.serving_bind_port, amd.worker.serving_bind_port);

    // And the architecture-determined fields must actually differ, or the
    // per-architecture split is not doing anything.
    assert_ne!(
        arm.device_family, amd.device_family,
        "the two variants exist to declare different device families"
    );
}
