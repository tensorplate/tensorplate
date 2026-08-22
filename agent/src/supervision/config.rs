// SPDX-License-Identifier: Apache-2.0
//
// V01-E09-F01-T01: Worker supervisor configuration schema.
//
// The supervisor consumes one validated [`SupervisorConfig`] for the
// lifetime of the agent process. The schema covers:
//
//   - launch identity: binary path, args, environment allowlist, working
//     directory, serving-config reference, control endpoint
//   - bounded timeouts: startup readiness, graceful stop, kill escalation,
//     status poll interval
//   - restart policy: bounded exponential backoff with rolling-window
//     crash-loop detection (V01-E09-F03)
//
// Defaults are conservative and never expose serving endpoints beyond
// loopback. Unsupported policy values surface as typed
// [`AgentError::Config`] before durable state is touched.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{AgentError, AgentResult};

/// Restart policy discriminator. v0.1.0 ships exactly two: `disabled`
/// (operator restart only — useful for benchmarking and v0.1.0 host CI
/// where the worker is the test subject) and `bounded_backoff` (the
/// V01-E09-F03 production default). The set is union-stable; future
/// policies append rather than rename.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicyKind {
    Disabled,
    #[default]
    BoundedBackoff,
}

/// Bounded exponential-backoff parameters used by [`RestartPolicyKind::BoundedBackoff`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackoffConfig {
    #[serde(default = "default_initial_delay_ms")]
    pub initial_delay_ms: u64,
    #[serde(default = "default_multiplier_hundredths")]
    pub multiplier_hundredths: u32,
    #[serde(default = "default_max_delay_ms")]
    pub max_delay_ms: u64,
    /// Rolling window inside which restart attempts accumulate toward the
    /// crash-loop threshold. Counters reset after `stable_reset_ms` of
    /// uninterrupted ready uptime.
    #[serde(default = "default_window_ms")]
    pub window_ms: u64,
    #[serde(default = "default_threshold")]
    pub threshold: u32,
    #[serde(default = "default_stable_reset_ms")]
    pub stable_reset_ms: u64,
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self {
            initial_delay_ms: default_initial_delay_ms(),
            multiplier_hundredths: default_multiplier_hundredths(),
            max_delay_ms: default_max_delay_ms(),
            window_ms: default_window_ms(),
            threshold: default_threshold(),
            stable_reset_ms: default_stable_reset_ms(),
        }
    }
}

const fn default_initial_delay_ms() -> u64 {
    500
}
const fn default_multiplier_hundredths() -> u32 {
    200 // 2.0x
}
const fn default_max_delay_ms() -> u64 {
    30_000
}
const fn default_window_ms() -> u64 {
    300_000 // 5 minutes
}
const fn default_threshold() -> u32 {
    5
}
const fn default_stable_reset_ms() -> u64 {
    120_000 // 2 minutes
}

/// Combined restart policy as observed by the supervisor.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartPolicy {
    #[serde(default)]
    pub kind: RestartPolicyKind,
    #[serde(default)]
    pub backoff: BackoffConfig,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            kind: RestartPolicyKind::BoundedBackoff,
            backoff: BackoffConfig::default(),
        }
    }
}

/// Stdout / stderr capture mode. The agent default is `inherit` so an
/// operator running under systemd / journalctl sees structured worker
/// output without an extra pipe layer; `discard` is reserved for benches
/// and `capture_to_file` for the V01-E11 `tensorplate logs` consumer.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStdioMode {
    #[default]
    Inherit,
    Discard,
    CaptureToFile,
}

/// Bounded supervision-event sink config (V01-E09-F05).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventSinkConfig {
    /// Maximum number of pending events queued before the supervisor
    /// drops the oldest. The supervisor records the bounded drop count
    /// in status and emits a typed log line, but it never blocks
    /// supervision decisions waiting for a slow consumer.
    #[serde(default = "default_event_queue_capacity")]
    pub queue_capacity: u32,
    /// Optional Unix domain socket the observability service listens on.
    /// `None` (the v0.1.0 default) keeps the in-process channel only;
    /// V01-E10 lights this up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uds_path: Option<PathBuf>,
}

impl Default for EventSinkConfig {
    fn default() -> Self {
        Self {
            queue_capacity: default_event_queue_capacity(),
            uds_path: None,
        }
    }
}

const fn default_event_queue_capacity() -> u32 {
    256
}

/// V01-E09-F01 worker supervisor configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorConfig {
    /// Absolute path to the V01-E07 `tensorplate-serving` binary the
    /// supervisor will launch. Validated before any process start.
    pub binary_path: PathBuf,
    /// Extra arguments appended after the supervisor-supplied `--config`
    /// flag. Empty by default.
    #[serde(default)]
    pub args: Vec<String>,
    /// Names of environment variables the supervisor will forward from
    /// the agent's own environment. Empty disables environment
    /// forwarding entirely.
    #[serde(default)]
    pub env_allowlist: BTreeSet<String>,
    /// Absolute working directory the launched process inherits.
    pub working_dir: PathBuf,
    /// Absolute serving config file the supervisor passes via
    /// `--config <serving_config_path>`.
    pub serving_config_path: PathBuf,
    /// Loopback host the worker binds its data plane to. Validated as a
    /// loopback literal.
    #[serde(default = "default_control_host")]
    pub control_host: String,
    /// Loopback port the worker binds its `/health` and `/infer` endpoints
    /// to.
    pub control_port: u16,
    /// Stdout/stderr capture mode.
    #[serde(default)]
    pub stdio_mode: WorkerStdioMode,
    /// Time the supervisor waits for the worker to report `ready` after
    /// launch. Hitting this timeout counts as a not-ready failure
    /// against the restart policy (V01-E09-F02 / F03).
    #[serde(default = "default_startup_timeout_ms")]
    pub startup_timeout_ms: u64,
    /// Time the supervisor waits for a graceful shutdown before
    /// escalating to forced termination.
    #[serde(default = "default_graceful_stop_timeout_ms")]
    pub graceful_stop_timeout_ms: u64,
    /// Hard upper bound after forced-termination escalation. The
    /// supervisor reports a typed `WorkerControl` error if the process
    /// still has not exited.
    #[serde(default = "default_kill_timeout_ms")]
    pub kill_timeout_ms: u64,
    /// How often the supervisor polls the worker for actual-state during
    /// steady-state operation.
    #[serde(default = "default_status_poll_interval_ms")]
    pub status_poll_interval_ms: u64,
    /// Restart policy.
    #[serde(default)]
    pub restart_policy: RestartPolicy,
    /// Supervision-event sink.
    #[serde(default)]
    pub event_sink: EventSinkConfig,
}

const fn default_startup_timeout_ms() -> u64 {
    30_000
}
const fn default_graceful_stop_timeout_ms() -> u64 {
    5_000
}
const fn default_kill_timeout_ms() -> u64 {
    2_000
}
const fn default_status_poll_interval_ms() -> u64 {
    1_000
}
fn default_control_host() -> String {
    "127.0.0.1".to_string()
}

impl SupervisorConfig {
    /// Validate the config. Returns the same value on success so the
    /// caller can chain `?` from the deserialize path.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Config`] for missing / non-absolute /
    /// non-loopback / zero-timeout / unsupported policy values.
    pub fn validate(self) -> AgentResult<Self> {
        if !self.binary_path.is_absolute() {
            return Err(AgentError::Config(format!(
                "supervision.binary_path `{}` must be absolute",
                self.binary_path.display()
            )));
        }
        if !self.working_dir.is_absolute() {
            return Err(AgentError::Config(format!(
                "supervision.working_dir `{}` must be absolute",
                self.working_dir.display()
            )));
        }
        if !self.serving_config_path.is_absolute() {
            return Err(AgentError::Config(format!(
                "supervision.serving_config_path `{}` must be absolute",
                self.serving_config_path.display()
            )));
        }
        if !matches!(
            self.control_host.as_str(),
            "127.0.0.1" | "::1" | "localhost"
        ) {
            return Err(AgentError::Config(format!(
                "supervision.control_host `{}` must be a loopback literal",
                self.control_host
            )));
        }
        if self.control_port == 0 {
            return Err(AgentError::Config(
                "supervision.control_port must be > 0".into(),
            ));
        }
        if self.startup_timeout_ms == 0
            || self.graceful_stop_timeout_ms == 0
            || self.kill_timeout_ms == 0
            || self.status_poll_interval_ms == 0
        {
            return Err(AgentError::Config(
                "supervision timeouts must be > 0".into(),
            ));
        }
        match self.restart_policy.kind {
            RestartPolicyKind::Disabled => {}
            RestartPolicyKind::BoundedBackoff => {
                let b = &self.restart_policy.backoff;
                if b.initial_delay_ms == 0 {
                    return Err(AgentError::Config(
                        "supervision.restart_policy.backoff.initial_delay_ms must be > 0".into(),
                    ));
                }
                if b.multiplier_hundredths < 100 {
                    return Err(AgentError::Config(
                        "supervision.restart_policy.backoff.multiplier_hundredths must be >= 100 (1.0x)".into(),
                    ));
                }
                if b.max_delay_ms < b.initial_delay_ms {
                    return Err(AgentError::Config(
                        "supervision.restart_policy.backoff.max_delay_ms must be >= initial_delay_ms".into(),
                    ));
                }
                if b.window_ms == 0 {
                    return Err(AgentError::Config(
                        "supervision.restart_policy.backoff.window_ms must be > 0".into(),
                    ));
                }
                if b.threshold == 0 {
                    return Err(AgentError::Config(
                        "supervision.restart_policy.backoff.threshold must be > 0".into(),
                    ));
                }
                if b.stable_reset_ms == 0 {
                    return Err(AgentError::Config(
                        "supervision.restart_policy.backoff.stable_reset_ms must be > 0".into(),
                    ));
                }
            }
        }
        if self.event_sink.queue_capacity == 0 {
            return Err(AgentError::Config(
                "supervision.event_sink.queue_capacity must be > 0".into(),
            ));
        }
        if let Some(path) = self.event_sink.uds_path.as_deref() {
            if !path.is_absolute() {
                return Err(AgentError::Config(format!(
                    "supervision.event_sink.uds_path `{}` must be absolute",
                    path.display()
                )));
            }
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{
        BackoffConfig, EventSinkConfig, RestartPolicy, RestartPolicyKind, SupervisorConfig,
        WorkerStdioMode,
    };
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn minimal() -> SupervisorConfig {
        SupervisorConfig {
            binary_path: PathBuf::from("/usr/local/bin/tensorplate-serving"),
            args: vec![],
            env_allowlist: BTreeSet::new(),
            working_dir: PathBuf::from("/var/lib/tensorplate"),
            serving_config_path: PathBuf::from("/var/lib/tensorplate/serving.json"),
            control_host: "127.0.0.1".into(),
            control_port: 18080,
            stdio_mode: WorkerStdioMode::Inherit,
            startup_timeout_ms: 30_000,
            graceful_stop_timeout_ms: 5_000,
            kill_timeout_ms: 2_000,
            status_poll_interval_ms: 1_000,
            restart_policy: RestartPolicy::default(),
            event_sink: EventSinkConfig::default(),
        }
    }

    #[test]
    fn minimal_config_validates() {
        let c = minimal().validate().expect("valid");
        assert_eq!(c.restart_policy.kind, RestartPolicyKind::BoundedBackoff);
        assert!(matches!(c.stdio_mode, WorkerStdioMode::Inherit));
    }

    #[test]
    fn rejects_relative_binary_path() {
        let mut c = minimal();
        c.binary_path = PathBuf::from("tensorplate-serving");
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_non_loopback_control_host() {
        let mut c = minimal();
        c.control_host = "0.0.0.0".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_zero_timeouts() {
        let mut c = minimal();
        c.startup_timeout_ms = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_bad_backoff_multiplier() {
        let mut c = minimal();
        c.restart_policy.backoff = BackoffConfig {
            multiplier_hundredths: 50,
            ..BackoffConfig::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_max_below_initial_delay() {
        let mut c = minimal();
        c.restart_policy.backoff = BackoffConfig {
            initial_delay_ms: 1_000,
            max_delay_ms: 500,
            ..BackoffConfig::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_zero_event_queue_capacity() {
        let mut c = minimal();
        c.event_sink.queue_capacity = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn json_round_trip_works() {
        let c = minimal().validate().expect("valid");
        let raw = serde_json::to_string(&c).expect("ser");
        let back: SupervisorConfig = serde_json::from_str(&raw).expect("de");
        let back = back.validate().expect("valid back");
        assert_eq!(c, back);
    }

    #[test]
    fn disabled_policy_validates_without_backoff_check() {
        let mut c = minimal();
        c.restart_policy.kind = RestartPolicyKind::Disabled;
        c.restart_policy.backoff = BackoffConfig {
            multiplier_hundredths: 50, // would normally fail
            ..BackoffConfig::default()
        };
        assert!(c.validate().is_ok());
    }
}
