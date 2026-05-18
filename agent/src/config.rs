// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F01: Agent runtime configuration.
//
// The agent loads this struct at startup, validates it before opening the
// local control transport, and refuses to mutate durable state until
// validation succeeds. The serialized shape mirrors
// `config/schemas/agent.json`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use tensorplate_protocol::bundle_manifest::DeviceFamily;

use crate::error::{AgentError, AgentResult};
use crate::supervision::SupervisorConfig;

/// Local control API transport. v0.1.0 default: Unix domain socket. The
/// architecture decision is recorded in `docs/architecture/agent-control-api.md`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlTransport {
    UnixSocket,
    LoopbackTcp,
}

impl Default for ControlTransport {
    fn default() -> Self {
        Self::UnixSocket
    }
}

/// Serving-worker control implementation used by the agent.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerControlMode {
    /// Deterministic in-process worker used by host CI and unit tests.
    #[default]
    Mock,
    /// Spawn and supervise the V01-E07 `tensorplate-serving` binary.
    Process,
}

/// Capability flags published by an available backend. The verifier checks
/// these against the bundle's `capability_requirements`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct BackendCapability {
    #[serde(default, rename = "async")]
    pub async_: bool,
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub generation: bool,
    #[serde(default)]
    pub kv_cache: bool,
    #[serde(default)]
    pub fixed_shape: bool,
}

/// Worker-control bounded-timeout knobs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkerConfig {
    #[serde(default)]
    pub mode: WorkerControlMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serving_binary_path: Option<PathBuf>,
    #[serde(default = "default_serving_bind_host")]
    pub serving_bind_host: String,
    #[serde(default = "default_serving_bind_port")]
    pub serving_bind_port: u16,
    #[serde(default = "default_serving_candidate_bind_port")]
    pub serving_candidate_bind_port: u16,
    #[serde(default)]
    pub serving_use_mock_session: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serving_config_dir: Option<PathBuf>,
    #[serde(default = "default_warm_timeout_ms")]
    pub warm_timeout_ms: u64,
    #[serde(default = "default_prepare_timeout_ms")]
    pub prepare_timeout_ms: u64,
    #[serde(default = "default_status_poll_interval_ms")]
    pub status_poll_interval_ms: u64,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            mode: WorkerControlMode::Mock,
            serving_binary_path: None,
            serving_bind_host: default_serving_bind_host(),
            serving_bind_port: default_serving_bind_port(),
            serving_candidate_bind_port: default_serving_candidate_bind_port(),
            serving_use_mock_session: false,
            serving_config_dir: None,
            warm_timeout_ms: default_warm_timeout_ms(),
            prepare_timeout_ms: default_prepare_timeout_ms(),
            status_poll_interval_ms: default_status_poll_interval_ms(),
        }
    }
}

const fn default_warm_timeout_ms() -> u64 {
    30_000
}
const fn default_prepare_timeout_ms() -> u64 {
    60_000
}
const fn default_status_poll_interval_ms() -> u64 {
    250
}
fn default_serving_bind_host() -> String {
    "127.0.0.1".to_string()
}
const fn default_serving_bind_port() -> u16 {
    18080
}
const fn default_serving_candidate_bind_port() -> u16 {
    18081
}

/// V01-E08 agent configuration. Mirrors `config/schemas/agent.json`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default = "default_schema_version")]
    pub schema_version: String,
    #[serde(default)]
    pub transport: ControlTransport,
    pub socket_path: Option<PathBuf>,
    #[serde(default = "default_tcp_host")]
    pub tcp_bind_host: String,
    #[serde(default)]
    pub tcp_bind_port: u16,
    pub state_dir: PathBuf,
    pub staging_dir: PathBuf,
    #[serde(default = "default_backends")]
    pub available_backends: Vec<String>,
    #[serde(default)]
    pub backend_capabilities: BTreeMap<String, BackendCapability>,
    #[serde(default)]
    pub device_memory_bytes: Option<u64>,
    #[serde(default)]
    pub device_family: DeviceFamily,
    #[serde(default)]
    pub worker: WorkerConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervision: Option<SupervisorConfig>,
    /// Optional runtime version override. Falls back to the protocol crate
    /// version at validation time. Reserved for tests that need to drive
    /// runtime-compatibility rejection paths.
    #[serde(default)]
    pub runtime_version: Option<String>,
}

fn default_schema_version() -> String {
    tensorplate_protocol::SCHEMA_VERSION.to_string()
}

fn default_tcp_host() -> String {
    "127.0.0.1".to_string()
}

fn default_backends() -> Vec<String> {
    vec!["mock".to_string()]
}

impl AgentConfig {
    /// Validate the configuration. Returns a fully-resolved config or a
    /// typed [`AgentError::Config`].
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Config`] for missing or invalid fields. The
    /// caller must not mutate durable state before this returns Ok.
    pub fn validate(mut self) -> AgentResult<Self> {
        if self.schema_version != tensorplate_protocol::SCHEMA_VERSION {
            return Err(AgentError::Config(format!(
                "unsupported schema_version `{}` (expected `{}`)",
                self.schema_version,
                tensorplate_protocol::SCHEMA_VERSION
            )));
        }
        if self.state_dir.as_os_str().is_empty() {
            return Err(AgentError::Config(
                "state_dir must be an absolute path".into(),
            ));
        }
        if !self.state_dir.is_absolute() {
            return Err(AgentError::Config(format!(
                "state_dir `{}` must be absolute",
                self.state_dir.display()
            )));
        }
        if self.staging_dir.as_os_str().is_empty() {
            return Err(AgentError::Config(
                "staging_dir must be an absolute path".into(),
            ));
        }
        if !self.staging_dir.is_absolute() {
            return Err(AgentError::Config(format!(
                "staging_dir `{}` must be absolute",
                self.staging_dir.display()
            )));
        }
        if matches!(self.transport, ControlTransport::UnixSocket) {
            match self.socket_path.as_deref() {
                None => {
                    return Err(AgentError::Config(
                        "control.socket_path required for transport=unix_socket".into(),
                    ));
                }
                Some(p) if !p.is_absolute() => {
                    return Err(AgentError::Config(format!(
                        "control.socket_path `{}` must be absolute",
                        p.display()
                    )));
                }
                _ => {}
            }
        }
        if matches!(self.transport, ControlTransport::LoopbackTcp)
            && !matches!(
                self.tcp_bind_host.as_str(),
                "127.0.0.1" | "::1" | "localhost"
            )
        {
            return Err(AgentError::Config(format!(
                "control.tcp_bind_host `{}` must be a loopback literal",
                self.tcp_bind_host
            )));
        }
        if self.available_backends.is_empty() {
            return Err(AgentError::Config(
                "available_backends must contain at least one entry".into(),
            ));
        }
        if self.worker.warm_timeout_ms == 0
            || self.worker.prepare_timeout_ms == 0
            || self.worker.status_poll_interval_ms == 0
        {
            return Err(AgentError::Config(
                "worker timeouts and poll interval must be > 0".into(),
            ));
        }
        validate_process_worker_config(&mut self.worker, &self.state_dir)?;
        if let Some(supervision) = self.supervision.take() {
            self.supervision = Some(supervision.validate()?);
        }
        if self.runtime_version.is_none() {
            self.runtime_version = Some(tensorplate_protocol::version().to_string());
        }
        Ok(self)
    }

    /// Parse a JSON document into a validated config.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Serialization`] or [`AgentError::Config`].
    pub fn parse_json(text: &str) -> AgentResult<Self> {
        let cfg: Self = serde_json::from_str(text)?;
        cfg.validate()
    }

    /// True if `backend` is in the available list.
    #[must_use]
    pub fn backend_is_available(&self, backend: &str) -> bool {
        self.available_backends.iter().any(|b| b == backend)
    }

    /// Capability record for `backend`. Missing entries default to all-false.
    #[must_use]
    pub fn capability_for(&self, backend: &str) -> BackendCapability {
        self.backend_capabilities
            .get(backend)
            .copied()
            .unwrap_or_default()
    }
}

fn validate_process_worker_config(
    worker: &mut WorkerConfig,
    state_dir: &std::path::Path,
) -> AgentResult<()> {
    if !matches!(worker.mode, WorkerControlMode::Process) {
        return Ok(());
    }
    let Some(path) = worker.serving_binary_path.as_deref() else {
        return Err(AgentError::Config(
            "worker.serving_binary_path required for worker.mode=process".into(),
        ));
    };
    if !path.is_absolute() {
        return Err(AgentError::Config(format!(
            "worker.serving_binary_path `{}` must be absolute",
            path.display()
        )));
    }
    if !matches!(
        worker.serving_bind_host.as_str(),
        "127.0.0.1" | "::1" | "localhost"
    ) {
        return Err(AgentError::Config(format!(
            "worker.serving_bind_host `{}` must be loopback",
            worker.serving_bind_host
        )));
    }
    if worker.serving_bind_port == 0 || worker.serving_candidate_bind_port == 0 {
        return Err(AgentError::Config(
            "worker serving ports must be fixed and > 0 in process mode".into(),
        ));
    }
    if worker.serving_bind_port == worker.serving_candidate_bind_port {
        return Err(AgentError::Config(
            "worker serving active and candidate ports must differ".into(),
        ));
    }
    if worker.serving_config_dir.is_none() {
        worker.serving_config_dir = Some(state_dir.join("worker-configs"));
    }
    if let Some(dir) = worker.serving_config_dir.as_deref() {
        if !dir.is_absolute() {
            return Err(AgentError::Config(format!(
                "worker.serving_config_dir `{}` must be absolute",
                dir.display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_used,
        clippy::default_trait_access
    )]
    use super::{AgentConfig, ControlTransport, WorkerControlMode};
    use std::path::PathBuf;

    fn minimal() -> AgentConfig {
        AgentConfig {
            schema_version: tensorplate_protocol::SCHEMA_VERSION.to_string(),
            transport: ControlTransport::UnixSocket,
            socket_path: Some(PathBuf::from("/tmp/tensorplate-agent.sock")),
            tcp_bind_host: "127.0.0.1".into(),
            tcp_bind_port: 0,
            state_dir: PathBuf::from("/var/lib/tensorplate"),
            staging_dir: PathBuf::from("/var/lib/tensorplate/staging"),
            available_backends: vec!["mock".into()],
            backend_capabilities: Default::default(),
            device_memory_bytes: Some(8 * 1024 * 1024 * 1024),
            device_family: Default::default(),
            worker: Default::default(),
            supervision: None,
            runtime_version: None,
        }
    }

    #[test]
    fn minimal_validates() {
        let cfg = minimal().validate().expect("valid");
        assert!(cfg.runtime_version.is_some());
        assert!(cfg.backend_is_available("mock"));
        assert!(!cfg.backend_is_available("tensorrt"));
    }

    #[test]
    fn requires_socket_path_for_uds_transport() {
        let mut c = minimal();
        c.socket_path = None;
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_non_loopback_tcp() {
        let mut c = minimal();
        c.transport = ControlTransport::LoopbackTcp;
        c.tcp_bind_host = "0.0.0.0".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_unknown_schema_version() {
        let mut c = minimal();
        c.schema_version = "99.99".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_relative_state_dir() {
        let mut c = minimal();
        c.state_dir = PathBuf::from("relative/path");
        assert!(c.validate().is_err());
    }

    #[test]
    fn process_worker_requires_absolute_binary_path() {
        let mut c = minimal();
        c.worker.mode = WorkerControlMode::Process;
        c.worker.serving_binary_path = Some(PathBuf::from("tensorplate-serving"));
        assert!(c.validate().is_err());
    }

    #[test]
    fn process_worker_defaults_config_dir() {
        let mut c = minimal();
        c.worker.mode = WorkerControlMode::Process;
        c.worker.serving_binary_path = Some(PathBuf::from("/usr/local/bin/tensorplate-serving"));
        let cfg = c.validate().expect("valid");
        assert_eq!(
            cfg.worker.serving_config_dir.as_deref(),
            Some(PathBuf::from("/var/lib/tensorplate/worker-configs").as_path())
        );
    }

    #[test]
    fn json_round_trip_works() {
        let cfg = minimal().validate().expect("valid");
        let raw = serde_json::to_string(&cfg).expect("serialize");
        let back = AgentConfig::parse_json(&raw).expect("parse");
        assert_eq!(cfg, back);
    }

    #[test]
    fn parses_and_validates_supervision_block() {
        let raw = format!(
            r#"{{
                "schema_version":"{}",
                "transport":"unix_socket",
                "socket_path":"/tmp/tensorplate-agent.sock",
                "state_dir":"/var/lib/tensorplate",
                "staging_dir":"/var/lib/tensorplate/staging",
                "supervision":{{
                    "binary_path":"/usr/local/bin/tensorplate-serving",
                    "working_dir":"/var/lib/tensorplate",
                    "serving_config_path":"/var/lib/tensorplate/serving.json",
                    "control_port":18080
                }}
            }}"#,
            tensorplate_protocol::SCHEMA_VERSION
        );
        let cfg = AgentConfig::parse_json(&raw).expect("parse");
        assert!(cfg.supervision.is_some());
    }
}
