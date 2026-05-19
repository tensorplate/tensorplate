// SPDX-License-Identifier: Apache-2.0
//
// V01-E10-F01-T01: Observability service configuration schema.
//
// The observability service loads this struct at startup, validates it
// before binding to any local transport, and refuses to publish or write
// snapshots until validation succeeds. The serialized shape mirrors
// `config/schemas/observability.json`.
//
// Defaults are local-only: no hosted platform, no remote relay, no
// outbound DNS. The service can run with the ROS 2 publisher disabled
// and a no-op safe-state sink for headless test environments.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{ObservabilityError, ObservabilityResult};

/// Local event-listener transport. v0.1.0 defaults to an in-process
/// channel so the service can run in CI without any local sockets;
/// production deployments install a Unix domain socket pointing at the
/// supervisor / serving worker event source.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListenerTransport {
    /// Events are produced inside the same process via
    /// [`crate::listener::EventTx`]. Used by unit / integration tests
    /// and by the V01-E07 in-tree heartbeat hook when the worker runs
    /// in-process with the monitor.
    #[default]
    InProcess,
    /// Reserved for JSON lines on the configured Unix domain socket.
    /// Selecting this in v0.1.0 returns a typed config error so the
    /// service never starts with a configured-but-unbound transport.
    UnixSocket,
}

/// Safe-state event sink mode. The sink is bounded; a slow or absent
/// consumer never blocks heartbeat evaluation.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafeStateSinkKind {
    /// In-memory ring buffer drained by tests, by the V01-E11 CLI in a
    /// future commit, and by the optional ROS 2 publisher.
    #[default]
    InMemory,
    /// Append safe-state events as JSON lines to a file. Writes are
    /// best-effort; failures bump a bounded error counter.
    File,
}

/// Status snapshot sink mode.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusSnapshotKind {
    /// Hold the snapshot in memory; read through [`crate::snapshot::SnapshotWriter::current`].
    #[default]
    InMemory,
    /// Write the snapshot to disk via atomic-replace.
    File,
}

/// Heartbeat policy. Timings are monotonic; wall-clock changes never
/// affect freshness decisions.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatPolicy {
    /// Expected gap between consecutive heartbeats. Setting this below
    /// the producer's emission interval will cause false missed-heartbeat
    /// counts.
    #[serde(default = "default_expected_interval_ms")]
    pub expected_interval_ms: u64,
    /// Grace window added on top of `expected_interval_ms` before a
    /// missed heartbeat is recorded. Useful when the producer's
    /// scheduling can briefly stall.
    #[serde(default = "default_grace_ms")]
    pub grace_ms: u64,
    /// Number of consecutive missed heartbeats before the state
    /// transitions to `no-heartbeat`. Must be >= 1.
    #[serde(default = "default_missed_threshold")]
    pub missed_threshold: u32,
    /// Consecutive heartbeats required after a `no-heartbeat` state to
    /// transition back. v0.1.0 default is 1: a single fresh heartbeat
    /// recovers the source.
    #[serde(default = "default_recovery_heartbeats")]
    pub recovery_heartbeats: u32,
}

impl Default for HeartbeatPolicy {
    fn default() -> Self {
        Self {
            expected_interval_ms: default_expected_interval_ms(),
            grace_ms: default_grace_ms(),
            missed_threshold: default_missed_threshold(),
            recovery_heartbeats: default_recovery_heartbeats(),
        }
    }
}

const fn default_expected_interval_ms() -> u64 {
    1_000
}
const fn default_grace_ms() -> u64 {
    250
}
const fn default_missed_threshold() -> u32 {
    3
}
const fn default_recovery_heartbeats() -> u32 {
    1
}

/// Safe-state sink config.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SafeStateSinkConfig {
    #[serde(default)]
    pub kind: SafeStateSinkKind,
    /// Maximum number of safe-state events buffered for an in-memory
    /// sink. File sinks honour the same cap on the in-process queue
    /// that fronts the file writer.
    #[serde(default = "default_safe_state_queue_capacity")]
    pub queue_capacity: u32,
    /// File path for the file-backed sink. Required when
    /// `kind == File`; ignored otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// Optional periodic emission interval. When set, the aggregator
    /// emits a safe-state event on every transition AND on every
    /// `periodic_ms` tick that the current state is not `ready`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub periodic_ms: Option<u64>,
}

impl Default for SafeStateSinkConfig {
    fn default() -> Self {
        Self {
            kind: SafeStateSinkKind::default(),
            queue_capacity: default_safe_state_queue_capacity(),
            path: None,
            periodic_ms: None,
        }
    }
}

const fn default_safe_state_queue_capacity() -> u32 {
    128
}

/// Snapshot writer config.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StatusSnapshotConfig {
    #[serde(default)]
    pub kind: StatusSnapshotKind,
    /// File path for the file-backed snapshot. Required when
    /// `kind == File`; ignored otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// Cap on bounded diagnostics retained alongside the snapshot.
    #[serde(default = "default_diagnostics_capacity")]
    pub diagnostics_capacity: u32,
}

impl Default for StatusSnapshotConfig {
    fn default() -> Self {
        Self {
            kind: StatusSnapshotKind::default(),
            path: None,
            diagnostics_capacity: default_diagnostics_capacity(),
        }
    }
}

const fn default_diagnostics_capacity() -> u32 {
    64
}

/// ROS 2 health publisher config. Disabled by default so the baseline
/// service runs in CI without a ROS 2 distribution installed. When
/// enabled the V01-E10 stub publishes the configured `DiagnosticArray`
/// to a mock publisher (test-mode) or to the real ROS 2 transport (when
/// the future native publisher is wired in).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Ros2HealthConfig {
    /// Master switch. When `false` no publisher is constructed.
    #[serde(default = "default_ros2_enabled")]
    pub enabled: bool,
    /// Topic name. v0.1.0 defaults to `/tensorplate/health`.
    #[serde(default = "default_ros2_topic")]
    pub topic: String,
    /// Health-publish interval. Independent from the heartbeat
    /// interval; the publisher emits on state changes AND on this
    /// interval.
    #[serde(default = "default_ros2_interval_ms")]
    pub interval_ms: u64,
    /// Behaviour when the ROS 2 runtime is unavailable. v0.1.0 ships
    /// `mock` so the stub works in headless CI; production deployments
    /// can select `required` to fail startup when the runtime is
    /// missing.
    #[serde(default)]
    pub runtime: Ros2Runtime,
}

impl Default for Ros2HealthConfig {
    fn default() -> Self {
        Self {
            enabled: default_ros2_enabled(),
            topic: default_ros2_topic(),
            interval_ms: default_ros2_interval_ms(),
            runtime: Ros2Runtime::default(),
        }
    }
}

const fn default_ros2_enabled() -> bool {
    false
}
fn default_ros2_topic() -> String {
    "/tensorplate/health".to_string()
}
const fn default_ros2_interval_ms() -> u64 {
    1_000
}

/// ROS 2 runtime selection.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ros2Runtime {
    /// In-process mock publisher (the v0.1.0 stub). Captures every
    /// publish for tests; never depends on `rclrs` or `rcl` libraries.
    #[default]
    Mock,
    /// Reserved for the post-v0.1.0 native publisher. Selecting this
    /// today returns a typed config error so deployments never
    /// accidentally rely on an unimplemented transport.
    Native,
}

/// Listener transport config.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ListenerConfig {
    #[serde(default)]
    pub transport: ListenerTransport,
    /// Bounded incoming-event queue capacity. The listener drops the
    /// oldest event when the queue is full and bumps a typed counter.
    #[serde(default = "default_listener_queue_capacity")]
    pub queue_capacity: u32,
    /// Unix-socket path for `transport=unix_socket`. v0.1.0 reserves
    /// this surface; the in-process transport is the active path until
    /// the C++ serving worker grows a heartbeat producer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uds_path: Option<PathBuf>,
}

impl Default for ListenerConfig {
    fn default() -> Self {
        Self {
            transport: ListenerTransport::default(),
            queue_capacity: default_listener_queue_capacity(),
            uds_path: None,
        }
    }
}

const fn default_listener_queue_capacity() -> u32 {
    1_024
}

/// V01-E10 observability service config. Mirrors
/// `config/schemas/observability.json`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ObservabilityConfig {
    #[serde(default = "default_schema_version")]
    pub schema_version: String,
    /// Stable identifier for the source the local heartbeat producer
    /// labels itself with. v0.1.0 default is `serving_worker` so the
    /// V01-E07 heartbeat producer needs no config bump.
    #[serde(default = "default_primary_source")]
    pub primary_source: String,
    #[serde(default)]
    pub listener: ListenerConfig,
    #[serde(default)]
    pub heartbeat: HeartbeatPolicy,
    #[serde(default)]
    pub safe_state: SafeStateSinkConfig,
    #[serde(default)]
    pub snapshot: StatusSnapshotConfig,
    #[serde(default)]
    pub ros2_health: Ros2HealthConfig,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            schema_version: default_schema_version(),
            primary_source: default_primary_source(),
            listener: ListenerConfig::default(),
            heartbeat: HeartbeatPolicy::default(),
            safe_state: SafeStateSinkConfig::default(),
            snapshot: StatusSnapshotConfig::default(),
            ros2_health: Ros2HealthConfig::default(),
        }
    }
}

fn default_schema_version() -> String {
    tensorplate_protocol::SCHEMA_VERSION.to_string()
}

fn default_primary_source() -> String {
    "serving_worker".to_string()
}

fn validate_schema_version(version: &str) -> ObservabilityResult<()> {
    if version != tensorplate_protocol::SCHEMA_VERSION {
        return Err(ObservabilityError::Config(format!(
            "unsupported schema_version `{}` (expected `{}`)",
            version,
            tensorplate_protocol::SCHEMA_VERSION
        )));
    }
    Ok(())
}

fn validate_listener(cfg: &ListenerConfig) -> ObservabilityResult<()> {
    if let Some(path) = cfg.uds_path.as_deref() {
        if !path.is_absolute() {
            return Err(ObservabilityError::Config(format!(
                "listener.uds_path `{}` must be absolute",
                path.display()
            )));
        }
    }
    if matches!(cfg.transport, ListenerTransport::UnixSocket) {
        if cfg.uds_path.is_none() {
            return Err(ObservabilityError::Config(
                "listener.uds_path required for transport=unix_socket".into(),
            ));
        }
        return Err(ObservabilityError::Config(
            "listener.transport=unix_socket is reserved until the external socket listener lands; \
             use `in_process` in v0.1.0"
                .into(),
        ));
    }
    if cfg.queue_capacity == 0 {
        return Err(ObservabilityError::Config(
            "listener.queue_capacity must be > 0".into(),
        ));
    }
    Ok(())
}

fn validate_heartbeat(cfg: &HeartbeatPolicy) -> ObservabilityResult<()> {
    if cfg.expected_interval_ms == 0 {
        return Err(ObservabilityError::Config(
            "heartbeat.expected_interval_ms must be > 0".into(),
        ));
    }
    if cfg.missed_threshold == 0 {
        return Err(ObservabilityError::Config(
            "heartbeat.missed_threshold must be >= 1".into(),
        ));
    }
    if cfg.recovery_heartbeats == 0 {
        return Err(ObservabilityError::Config(
            "heartbeat.recovery_heartbeats must be >= 1".into(),
        ));
    }
    Ok(())
}

fn validate_safe_state(cfg: &SafeStateSinkConfig) -> ObservabilityResult<()> {
    if cfg.queue_capacity == 0 {
        return Err(ObservabilityError::Config(
            "safe_state.queue_capacity must be > 0".into(),
        ));
    }
    if matches!(cfg.kind, SafeStateSinkKind::File) {
        let Some(path) = cfg.path.as_deref() else {
            return Err(ObservabilityError::Config(
                "safe_state.path required for kind=file".into(),
            ));
        };
        if !path.is_absolute() {
            return Err(ObservabilityError::Config(format!(
                "safe_state.path `{}` must be absolute",
                path.display()
            )));
        }
    }
    if let Some(periodic) = cfg.periodic_ms {
        if periodic == 0 {
            return Err(ObservabilityError::Config(
                "safe_state.periodic_ms must be > 0 when set".into(),
            ));
        }
    }
    Ok(())
}

fn validate_snapshot(cfg: &StatusSnapshotConfig) -> ObservabilityResult<()> {
    if cfg.diagnostics_capacity == 0 {
        return Err(ObservabilityError::Config(
            "snapshot.diagnostics_capacity must be > 0".into(),
        ));
    }
    if matches!(cfg.kind, StatusSnapshotKind::File) {
        let Some(path) = cfg.path.as_deref() else {
            return Err(ObservabilityError::Config(
                "snapshot.path required for kind=file".into(),
            ));
        };
        if !path.is_absolute() {
            return Err(ObservabilityError::Config(format!(
                "snapshot.path `{}` must be absolute",
                path.display()
            )));
        }
    }
    Ok(())
}

impl ObservabilityConfig {
    /// Validate the config. Returns the canonicalised value on success.
    ///
    /// # Errors
    ///
    /// Returns [`ObservabilityError::Config`] for missing required
    /// fields, unsupported schema versions, or runtime modes the v0.1.0
    /// baseline cannot satisfy.
    pub fn validate(self) -> ObservabilityResult<Self> {
        validate_schema_version(&self.schema_version)?;
        if self.primary_source.trim().is_empty() {
            return Err(ObservabilityError::Config(
                "primary_source must be non-empty".into(),
            ));
        }
        if !matches!(
            self.primary_source.as_str(),
            "serving_worker" | "agent_supervisor" | "internal"
        ) {
            return Err(ObservabilityError::Config(format!(
                "primary_source `{}` must be one of serving_worker, agent_supervisor, internal",
                self.primary_source
            )));
        }
        validate_listener(&self.listener)?;
        validate_heartbeat(&self.heartbeat)?;
        validate_safe_state(&self.safe_state)?;
        validate_snapshot(&self.snapshot)?;
        if self.ros2_health.enabled {
            if !self.ros2_health.topic.starts_with('/') {
                return Err(ObservabilityError::Config(format!(
                    "ros2_health.topic `{}` must start with `/`",
                    self.ros2_health.topic
                )));
            }
            if self.ros2_health.interval_ms == 0 {
                return Err(ObservabilityError::Config(
                    "ros2_health.interval_ms must be > 0 when enabled".into(),
                ));
            }
            if matches!(self.ros2_health.runtime, Ros2Runtime::Native) {
                return Err(ObservabilityError::Config(
                    "ros2_health.runtime=native is reserved for a future release; \
                     select `mock` for the v0.1.0 stub"
                        .into(),
                ));
            }
        }
        Ok(self)
    }

    /// Parse a JSON document into a validated config.
    ///
    /// # Errors
    ///
    /// Returns [`ObservabilityError::Serialization`] or
    /// [`ObservabilityError::Config`].
    pub fn parse_json(text: &str) -> ObservabilityResult<Self> {
        let cfg: Self = serde_json::from_str(text)?;
        cfg.validate()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{
        ListenerTransport, ObservabilityConfig, Ros2Runtime, SafeStateSinkKind, StatusSnapshotKind,
    };
    use std::path::PathBuf;

    fn minimal() -> ObservabilityConfig {
        ObservabilityConfig::default()
    }

    #[test]
    fn default_config_validates() {
        let cfg = minimal().validate().expect("valid");
        assert_eq!(cfg.primary_source, "serving_worker");
        assert!(!cfg.ros2_health.enabled);
        assert_eq!(cfg.heartbeat.missed_threshold, 3);
        assert_eq!(cfg.heartbeat.recovery_heartbeats, 1);
    }

    #[test]
    fn rejects_unknown_schema_version() {
        let mut c = minimal();
        c.schema_version = "99.99".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_empty_primary_source() {
        let mut c = minimal();
        c.primary_source = String::new();
        assert!(c.validate().is_err());
        let mut c = minimal();
        c.primary_source = "made_up_source".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn uds_transport_is_reserved_and_requires_absolute_path() {
        let mut c = minimal();
        c.listener.transport = ListenerTransport::UnixSocket;
        c.listener.uds_path = Some(PathBuf::from("relative/path"));
        assert!(c.clone().validate().is_err());
        c.listener.uds_path = None;
        assert!(c.clone().validate().is_err());
        c.listener.uds_path = Some(PathBuf::from("/run/tensorplate/observability.sock"));
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_zero_thresholds() {
        let mut c = minimal();
        c.heartbeat.missed_threshold = 0;
        assert!(c.validate().is_err());
        let mut c = minimal();
        c.heartbeat.expected_interval_ms = 0;
        assert!(c.validate().is_err());
        let mut c = minimal();
        c.heartbeat.recovery_heartbeats = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn file_safe_state_requires_absolute_path() {
        let mut c = minimal();
        c.safe_state.kind = SafeStateSinkKind::File;
        assert!(c.clone().validate().is_err());
        c.safe_state.path = Some(PathBuf::from("relative"));
        assert!(c.clone().validate().is_err());
        c.safe_state.path = Some(PathBuf::from("/var/log/tensorplate/safestate.jsonl"));
        assert!(c.validate().is_ok());
    }

    #[test]
    fn file_snapshot_requires_absolute_path() {
        let mut c = minimal();
        c.snapshot.kind = StatusSnapshotKind::File;
        assert!(c.clone().validate().is_err());
        c.snapshot.path = Some(PathBuf::from(
            "/var/lib/tensorplate/observability/status.json",
        ));
        assert!(c.validate().is_ok());
    }

    #[test]
    fn ros2_enabled_validates_topic_and_interval() {
        let mut c = minimal();
        c.ros2_health.enabled = true;
        c.ros2_health.topic = "tensorplate/health".into();
        assert!(c.validate().is_err()); // missing leading `/`
        let mut c = minimal();
        c.ros2_health.enabled = true;
        c.ros2_health.interval_ms = 0;
        assert!(c.validate().is_err());
        let mut c = minimal();
        c.ros2_health.enabled = true;
        c.ros2_health.runtime = Ros2Runtime::Native;
        assert!(c.validate().is_err());
        let mut c = minimal();
        c.ros2_health.enabled = true;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn json_round_trip_works() {
        let cfg = minimal().validate().expect("valid");
        let raw = serde_json::to_string(&cfg).expect("ser");
        let back = ObservabilityConfig::parse_json(&raw).expect("parse");
        assert_eq!(cfg, back);
    }

    #[test]
    fn periodic_zero_is_rejected() {
        let mut c = minimal();
        c.safe_state.periodic_ms = Some(0);
        assert!(c.validate().is_err());
    }

    #[test]
    fn listener_queue_capacity_must_be_positive() {
        let mut c = minimal();
        c.listener.queue_capacity = 0;
        assert!(c.validate().is_err());
    }
}
