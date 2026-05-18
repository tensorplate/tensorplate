// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F08 integration harness shared between the deploy / rollback /
// recovery test suites. Provides:
//
//   - `Harness` builder that creates an isolated `state_dir` +
//     `staging_dir` per test and constructs a `Coordinator` wired to a
//     `MockWorkerControl`.
//   - bundle fixture helpers: `vision_bundle`, `bad_bundle` /
//     `bundle_unsupported_backend` / `bundle_oversized` / etc.
//   - small wrappers so each test file stays under 100 lines.

#![allow(
    dead_code,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::default_trait_access,
    clippy::needless_borrows_for_generic_args,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::Digest;

use tempfile::TempDir;
use tensorplate_agent::config::{AgentConfig, BackendCapability, ControlTransport};
use tensorplate_agent::coordinator::Coordinator;
use tensorplate_agent::state::StateStore;
use tensorplate_agent::worker::{MockBehavior, MockWorkerControl};
use tensorplate_protocol::bundle_manifest::DeviceFamily;
use tensorplate_protocol::SCHEMA_VERSION;

pub struct Harness {
    pub td: TempDir,
    pub store: Arc<StateStore>,
    pub worker: Arc<MockWorkerControl>,
    pub coord: Arc<Coordinator>,
    pub config: AgentConfig,
}

impl Harness {
    pub fn new() -> Self {
        Self::with_behavior(MockBehavior::default())
    }

    pub fn with_behavior(behavior: MockBehavior) -> Self {
        let td = TempDir::new().expect("td");
        let state_dir = td.path().join("state");
        let staging_dir = td.path().join("staging");
        let mut capabilities = BTreeMap::new();
        capabilities.insert(
            "mock".to_string(),
            BackendCapability {
                async_: true,
                streaming: true,
                generation: true,
                kv_cache: true,
                fixed_shape: true,
            },
        );
        let config = AgentConfig {
            schema_version: SCHEMA_VERSION.to_string(),
            transport: ControlTransport::UnixSocket,
            socket_path: Some(td.path().join("agent.sock")),
            tcp_bind_host: "127.0.0.1".into(),
            tcp_bind_port: 0,
            state_dir: state_dir.clone(),
            staging_dir,
            available_backends: vec!["mock".into()],
            backend_capabilities: capabilities,
            device_memory_bytes: Some(8 * 1024 * 1024 * 1024),
            device_family: DeviceFamily::Any,
            worker: Default::default(),
            supervision: None,
            runtime_version: Some("0.1.0".into()),
        }
        .validate()
        .expect("valid config");

        let store = Arc::new(StateStore::open(&state_dir).expect("open store"));
        let worker = Arc::new(MockWorkerControl::with_behavior(behavior));
        let coord = Arc::new(Coordinator::new(
            config.clone(),
            store.clone(),
            worker.clone(),
        ));
        Self {
            td,
            store,
            worker,
            coord,
            config,
        }
    }

    pub fn bundle_dir(&self, name: &str) -> PathBuf {
        self.td.path().join(format!("bundle-{name}"))
    }
}

#[derive(Clone, Copy, Default)]
pub struct BundleSpec {
    pub model_class: Option<&'static str>,
    pub backend_hint: Option<&'static str>,
    pub format_version: Option<&'static str>,
    pub memory_estimate_bytes: Option<u64>,
    pub min_runtime_version: Option<&'static str>,
    pub max_runtime_version: Option<&'static str>,
    pub corrupt_artifact_bytes_after: bool,
    pub require_capability_streaming: bool,
}

pub fn write_bundle(dir: &Path, deployment: &str, spec: BundleSpec) -> PathBuf {
    let bdir = dir.join(format!("bundle-{deployment}"));
    fs::create_dir_all(&bdir).expect("mkdir");
    let body: &[u8] = b"model-bytes";
    let model_path = bdir.join("model.engine");
    fs::write(&model_path, body).expect("write");
    let mut h = sha2::Sha256::new();
    h.update(body);
    let dg = format!("sha256:{}", hex::encode(h.finalize()));
    let model_class = spec.model_class.unwrap_or("vision");
    let backend_hint = spec.backend_hint.unwrap_or("mock");
    let format_version = spec.format_version.unwrap_or("0.1");
    let mut manifest = serde_json::json!({
        "schema_version": SCHEMA_VERSION,
        "name": format!("bundle-{deployment}"),
        "version": deployment,
        "format_version": format_version,
        "model_class": model_class,
        "backend_hint": backend_hint,
        "artifacts": [{
            "role": "model",
            "path": "model.engine",
            "digest": dg,
        }],
    });
    if let Some(min) = spec.min_runtime_version {
        manifest["runtime_compatibility"] = serde_json::json!({
            "min_runtime_version": min,
        });
    }
    if let Some(max) = spec.max_runtime_version {
        manifest["runtime_compatibility"]["max_runtime_version"] =
            serde_json::Value::String(max.to_string());
    }
    if let Some(mem) = spec.memory_estimate_bytes {
        manifest["target_hardware"] = serde_json::json!({
            "memory_estimate_bytes": mem,
        });
    }
    if spec.require_capability_streaming {
        manifest["capability_requirements"] = serde_json::json!({
            "streaming": true,
        });
    }
    let body_str = serde_json::to_string_pretty(&manifest).expect("ser");
    fs::write(bdir.join("manifest.json"), body_str).expect("manifest");
    if spec.corrupt_artifact_bytes_after {
        fs::write(model_path, b"corrupted").expect("corrupt");
    }
    bdir
}

pub fn vision_bundle(td: &Path, deployment: &str) -> PathBuf {
    write_bundle(td, deployment, BundleSpec::default())
}
