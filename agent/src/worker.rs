// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F05: Agent -> serving-worker control client.
//
// The agent never mutates the serving worker's data path. It speaks the
// versioned local control contract documented in
// `protocol/schemas/worker_control.json` and `docs/architecture/agent-
// control-api.md`. The contract is modelled here as the
// [`WorkerControl`] trait so the deploy transaction coordinator depends
// on an interface, not on a concrete implementation.
//
// Three implementations / entry points:
//
//   - [`MockWorkerControl`]: deterministic in-process implementation
//     used by the agent integration tests and by the V01-E08 host CI
//     matrix where `tensorplate-serving` is not available.
//   - [`ProcessWorkerControl`]: process-backed implementation that
//     renders a V01-E07 serving config, starts `tensorplate-serving`,
//     polls `/health`, and promotes only warmed candidates.
//   - [`from_config`]: composition-root selector used by the binary.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tensorplate_protocol::worker_control::CandidateRef;

use crate::config::{AgentConfig, WorkerControlMode};
use crate::error::{AgentError, AgentResult};

/// Event surface for observability. `Coordinator` emits events at every
/// state transition; the agent's main loop subscribes a logging sink and
/// (in V01-E10) the observability service.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum WorkerEvent {
    PrepareStart {
        transaction_id: String,
        deployment_id: String,
    },
    PrepareEnd {
        transaction_id: String,
        deployment_id: String,
    },
    WarmStart {
        transaction_id: String,
        deployment_id: String,
    },
    WarmEnd {
        transaction_id: String,
        deployment_id: String,
    },
    Promote {
        transaction_id: String,
        deployment_id: String,
    },
    Unload {
        deployment_id: String,
    },
    Failure {
        transaction_id: String,
        deployment_id: String,
        reason: String,
    },
}

/// Worker readiness response.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct WorkerReadiness {
    pub deployment_id: String,
    pub ready: bool,
}

/// Narrow agent -> worker control surface. `prepare` stages and loads the
/// candidate; `warm` blocks until the candidate reports ready or the
/// timeout expires; `promote` flips the worker's active deployment to the
/// candidate; `unload` releases a previous active when policy requires.
pub trait WorkerControl: Send + Sync {
    /// Prepare/load the candidate. Returns Ok when the worker has
    /// accepted the candidate and started loading.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::WorkerControl`] for adapter/IPC failures and
    /// [`AgentError::WorkerTimeout`] when the prepare path exceeds its
    /// configured budget.
    fn prepare(
        &self,
        transaction_id: &str,
        candidate: &CandidateRef,
        timeout: Duration,
    ) -> AgentResult<()>;

    /// Block until the candidate reports ready or `timeout` expires.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::WorkerTimeout`] / [`AgentError::WorkerNotReady`] /
    /// [`AgentError::WorkerControl`].
    fn warm(
        &self,
        transaction_id: &str,
        candidate: &CandidateRef,
        timeout: Duration,
    ) -> AgentResult<WorkerReadiness>;

    /// Promote the previously-prepared candidate to active.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::WorkerControl`] for adapter/IPC failures.
    fn promote(&self, transaction_id: &str, candidate: &CandidateRef) -> AgentResult<()>;

    /// Unload `deployment_id` (the previous active) from the worker.
    /// Best-effort: a failure here is logged but never replaces the
    /// current active deployment.
    fn unload(&self, deployment_id: &str);

    /// Inspect the worker's active deployment id. Recovery uses this to
    /// reconcile desired state with actual worker state.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::WorkerControl`] for adapter/IPC failures.
    fn active_deployment_id(&self) -> AgentResult<Option<String>>;

    /// Data-plane endpoint for the active worker, when this control
    /// implementation owns a concrete serving process.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::WorkerControl`] for process inspection failures.
    fn active_serving_url(&self) -> AgentResult<Option<String>> {
        Ok(None)
    }
}

/// Build the configured worker-control implementation.
///
/// # Errors
///
/// Returns [`AgentError::Config`] if process mode is selected without the
/// required serving-worker process fields.
pub fn from_config(config: &AgentConfig) -> AgentResult<std::sync::Arc<dyn WorkerControl>> {
    match config.worker.mode {
        WorkerControlMode::Mock => Ok(std::sync::Arc::new(MockWorkerControl::new())),
        WorkerControlMode::Process => Ok(std::sync::Arc::new(ProcessWorkerControl::new(config)?)),
    }
}

#[derive(Clone, Debug)]
struct ProcessWorkerConfig {
    binary_path: PathBuf,
    bind_host: String,
    active_port: u16,
    candidate_port: u16,
    config_dir: PathBuf,
    use_mock_session: bool,
    status_poll_interval: Duration,
}

/// Process-backed worker controller for the V01-E07 `tensorplate-serving`
/// binary. It renders a serving config for each prepared candidate, starts
/// the worker on a loopback port, polls `/health` for warmup, and promotes
/// by making the warmed candidate the active child.
pub struct ProcessWorkerControl {
    config: ProcessWorkerConfig,
    inner: Mutex<ProcessWorkerState>,
}

#[derive(Default)]
struct ProcessWorkerState {
    active: Option<RunningWorker>,
    candidate: Option<RunningWorker>,
}

struct RunningWorker {
    deployment_id: String,
    port: u16,
    config_path: PathBuf,
    child: Child,
}

impl ProcessWorkerControl {
    /// Build a process-backed worker controller from validated agent config.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Config`] when required process-mode fields are
    /// missing.
    pub fn new(agent: &AgentConfig) -> AgentResult<Self> {
        let binary_path = agent
            .worker
            .serving_binary_path
            .clone()
            .ok_or_else(|| AgentError::Config("worker.serving_binary_path missing".into()))?;
        let config_dir = agent
            .worker
            .serving_config_dir
            .clone()
            .unwrap_or_else(|| agent.state_dir.join("worker-configs"));
        Ok(Self {
            config: ProcessWorkerConfig {
                binary_path,
                bind_host: agent.worker.serving_bind_host.clone(),
                active_port: agent.worker.serving_bind_port,
                candidate_port: agent.worker.serving_candidate_bind_port,
                config_dir,
                use_mock_session: agent.worker.serving_use_mock_session,
                status_poll_interval: Duration::from_millis(agent.worker.status_poll_interval_ms),
            },
            inner: Mutex::new(ProcessWorkerState::default()),
        })
    }

    fn candidate_port_for(&self, state: &ProcessWorkerState) -> u16 {
        match state.active.as_ref().map(|w| w.port) {
            Some(port) if port == self.config.candidate_port => self.config.active_port,
            _ => self.config.candidate_port,
        }
    }

    fn render_config(
        &self,
        candidate: &CandidateRef,
        port: u16,
    ) -> AgentResult<(PathBuf, serde_json::Value)> {
        fs::create_dir_all(&self.config.config_dir)?;
        let safe_tx_name = candidate
            .deployment_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>();
        let path = self
            .config
            .config_dir
            .join(format!("serving-{safe_tx_name}-{port}.json"));
        let artifact_path = candidate.artifact_relative_path.as_ref().map_or_else(
            || candidate.staged_path.clone(),
            |rel| {
                Path::new(&candidate.staged_path)
                    .join(rel)
                    .display()
                    .to_string()
            },
        );
        let config = serde_json::json!({
            "schema_version": tensorplate_protocol::SCHEMA_VERSION,
            "bind": {
                "host": self.config.bind_host,
                "port": port,
                "allow_non_loopback": false
            },
            "health_mode": "local_json",
            "metrics_mode": "prometheus_text",
            "enable_stderr_logs": true,
            "deployment": {
                "use_mock_session": self.config.use_mock_session,
                "endpoint": candidate.deployment_id,
                "backend": candidate.backend_hint,
                "model": {
                    "model_id": candidate.deployment_id,
                    "model_class": candidate.model_class,
                    "artifact_path": artifact_path,
                    "backend_hint": candidate.backend_hint,
                    "precision_hint": "auto"
                }
            }
        });
        fs::write(&path, serde_json::to_vec_pretty(&config)?)?;
        Ok((path, config))
    }

    fn spawn_candidate(&self, candidate: &CandidateRef, port: u16) -> AgentResult<RunningWorker> {
        let (config_path, _) = self.render_config(candidate, port)?;
        let child = Command::new(&self.config.binary_path)
            .arg("--config")
            .arg(&config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                AgentError::WorkerControl(format!(
                    "spawn {}: {e}",
                    self.config.binary_path.display()
                ))
            })?;
        Ok(RunningWorker {
            deployment_id: candidate.deployment_id.clone(),
            port,
            config_path,
            child,
        })
    }

    fn health(&self, port: u16, timeout: Duration) -> AgentResult<serde_json::Value> {
        let started = Instant::now();
        let deadline = started.checked_add(timeout).unwrap_or(started);
        loop {
            match get_health_json(&self.config.bind_host, port, timeout) {
                Ok(v) => return Ok(v),
                Err(err) if Instant::now() < deadline => {
                    let sleep = self
                        .config
                        .status_poll_interval
                        .min(deadline.saturating_duration_since(Instant::now()));
                    if !sleep.is_zero() {
                        std::thread::sleep(sleep);
                    }
                    let _ = err;
                }
                Err(err) => return Err(err),
            }
        }
    }
}

impl WorkerControl for ProcessWorkerControl {
    fn prepare(
        &self,
        _transaction_id: &str,
        candidate: &CandidateRef,
        _timeout: Duration,
    ) -> AgentResult<()> {
        let mut state = self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("process worker mutex poisoned: {e}")))?;
        if let Some(mut old_candidate) = state.candidate.take() {
            old_candidate.stop();
        }
        let port = if state.active.is_none() {
            self.config.active_port
        } else {
            self.candidate_port_for(&state)
        };
        state.candidate = Some(self.spawn_candidate(candidate, port)?);
        Ok(())
    }

    fn warm(
        &self,
        _transaction_id: &str,
        candidate: &CandidateRef,
        timeout: Duration,
    ) -> AgentResult<WorkerReadiness> {
        let port = {
            let state = self
                .inner
                .lock()
                .map_err(|e| AgentError::Internal(format!("process worker mutex poisoned: {e}")))?;
            let Some(prepared) = state.candidate.as_ref() else {
                return Err(AgentError::WorkerNotReady);
            };
            if prepared.deployment_id != candidate.deployment_id {
                return Err(AgentError::WorkerControl(format!(
                    "prepared candidate `{}` does not match warm request `{}`",
                    prepared.deployment_id, candidate.deployment_id
                )));
            }
            prepared.port
        };
        let started = Instant::now();
        let deadline = started.checked_add(timeout).unwrap_or(started);
        loop {
            if Instant::now() >= deadline {
                return Ok(WorkerReadiness {
                    deployment_id: candidate.deployment_id.clone(),
                    ready: false,
                });
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let health = self.health(port, remaining)?;
            let state = health
                .get("state")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let model = health
                .get("active_model_id")
                .and_then(serde_json::Value::as_str);
            if state == "ready" && model == Some(candidate.deployment_id.as_str()) {
                return Ok(WorkerReadiness {
                    deployment_id: candidate.deployment_id.clone(),
                    ready: true,
                });
            }
            if Instant::now() >= deadline {
                return Ok(WorkerReadiness {
                    deployment_id: candidate.deployment_id.clone(),
                    ready: false,
                });
            }
            let sleep = self
                .config
                .status_poll_interval
                .min(deadline.saturating_duration_since(Instant::now()));
            if !sleep.is_zero() {
                std::thread::sleep(sleep);
            }
        }
    }

    fn promote(&self, _transaction_id: &str, candidate: &CandidateRef) -> AgentResult<()> {
        let mut state = self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("process worker mutex poisoned: {e}")))?;
        let Some(mut prepared) = state.candidate.take() else {
            return Err(AgentError::WorkerNotReady);
        };
        if prepared.deployment_id != candidate.deployment_id {
            prepared.stop();
            return Err(AgentError::WorkerControl(format!(
                "prepared candidate `{}` does not match promote request `{}`",
                prepared.deployment_id, candidate.deployment_id
            )));
        }
        if let Some(mut active) = state.active.take() {
            active.stop();
        }
        state.active = Some(prepared);
        Ok(())
    }

    fn unload(&self, deployment_id: &str) {
        if let Ok(mut state) = self.inner.lock() {
            if state
                .active
                .as_ref()
                .is_some_and(|active| active.deployment_id == deployment_id)
            {
                if let Some(mut active) = state.active.take() {
                    active.stop();
                }
            }
            if state
                .candidate
                .as_ref()
                .is_some_and(|candidate| candidate.deployment_id == deployment_id)
            {
                if let Some(mut candidate) = state.candidate.take() {
                    candidate.stop();
                }
            }
        }
    }

    fn active_deployment_id(&self) -> AgentResult<Option<String>> {
        let mut state = self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("process worker mutex poisoned: {e}")))?;
        if let Some(active) = state.active.as_mut() {
            if active.child.try_wait()?.is_some() {
                state.active = None;
            }
        }
        Ok(state.active.as_ref().map(|w| w.deployment_id.clone()))
    }

    fn active_serving_url(&self) -> AgentResult<Option<String>> {
        let mut state = self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("process worker mutex poisoned: {e}")))?;
        if let Some(active) = state.active.as_mut() {
            if active.child.try_wait()?.is_some() {
                state.active = None;
            }
        }
        Ok(state
            .active
            .as_ref()
            .map(|w| format!("http://{}:{}/infer", self.config.bind_host, w.port)))
    }
}

impl RunningWorker {
    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.config_path);
    }
}

impl Drop for RunningWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn get_health_json(host: &str, port: u16, timeout: Duration) -> AgentResult<serde_json::Value> {
    let mut stream = TcpStream::connect((host, port)).map_err(|e| {
        AgentError::WorkerControl(format!("connect serving worker {host}:{port}: {e}"))
    })?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let request = format!("GET /health HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes())?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw)?;
    let Some((head, body)) = raw.split_once("\r\n\r\n") else {
        return Err(AgentError::WorkerControl(
            "serving worker health response missing header terminator".into(),
        ));
    };
    let status_ok = head
        .lines()
        .next()
        .is_some_and(|line| line.contains(" 200 ") || line.contains(" 503 "));
    if !status_ok {
        return Err(AgentError::WorkerControl(format!(
            "serving worker health returned unexpected status line: {}",
            head.lines().next().unwrap_or("")
        )));
    }
    serde_json::from_str(body).map_err(AgentError::from)
}

/// Behavior knobs for the mock worker. Tests flip these to drive failure
/// paths.
#[derive(Clone, Debug, Default)]
pub struct MockBehavior {
    pub fail_prepare: Option<AgentErrorKind>,
    pub fail_warm: Option<AgentErrorKind>,
    pub fail_promote: Option<AgentErrorKind>,
    pub warm_not_ready: bool,
    pub prepare_sleep: Option<Duration>,
    pub warm_sleep: Option<Duration>,
    /// Deployment id the worker should report as `active` for
    /// [`WorkerControl::active_deployment_id`]. Tests use this to drive
    /// recovery reconciliation.
    pub active_deployment_id: Option<String>,
}

/// Failure mode discriminator. Used by tests to ask the mock to fail in a
/// specific way.
#[derive(Clone, Debug)]
pub enum AgentErrorKind {
    LoadFailed(String),
    Unsupported(String),
    Timeout,
    NotReady,
    Internal(String),
}

impl AgentErrorKind {
    fn into_error(self) -> AgentError {
        match self {
            Self::LoadFailed(m) => AgentError::WorkerControl(m),
            Self::Unsupported(m) => AgentError::UnsupportedBackend(m),
            Self::Timeout => AgentError::WorkerTimeout(0),
            Self::NotReady => AgentError::WorkerNotReady,
            Self::Internal(m) => AgentError::Internal(m),
        }
    }
}

/// Recorded call (for assertions).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MockCall {
    pub op: &'static str,
    pub transaction_id: String,
    pub deployment_id: Option<String>,
}

/// Deterministic in-process worker. Records every call so tests can
/// assert the prepare/warm/promote sequence.
pub struct MockWorkerControl {
    inner: Mutex<MockState>,
}

struct MockState {
    behavior: MockBehavior,
    calls: Vec<MockCall>,
    prepared: Option<String>,
    active: Option<String>,
}

impl MockWorkerControl {
    /// Build a mock with default (success-everywhere) behavior.
    #[must_use]
    pub fn new() -> Self {
        Self::with_behavior(MockBehavior::default())
    }

    /// Build a mock with a specific behavior.
    #[must_use]
    pub fn with_behavior(behavior: MockBehavior) -> Self {
        let active = behavior.active_deployment_id.clone();
        Self {
            inner: Mutex::new(MockState {
                behavior,
                calls: Vec::new(),
                prepared: None,
                active,
            }),
        }
    }

    /// Drop-in helper that returns an Arc-wrapped instance ready to be
    /// passed to the coordinator.
    #[must_use]
    pub fn shared() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::new())
    }

    /// Read the recorded call sequence.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Internal`] if the mutex is poisoned.
    pub fn calls(&self) -> AgentResult<Vec<MockCall>> {
        Ok(self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("mock mutex poisoned: {e}")))?
            .calls
            .clone())
    }

    /// Inspect the deployment id currently `prepared` on the mock.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Internal`] if the mutex is poisoned.
    pub fn prepared_deployment_id(&self) -> AgentResult<Option<String>> {
        Ok(self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("mock mutex poisoned: {e}")))?
            .prepared
            .clone())
    }
}

impl Default for MockWorkerControl {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkerControl for MockWorkerControl {
    fn prepare(
        &self,
        transaction_id: &str,
        candidate: &CandidateRef,
        _timeout: Duration,
    ) -> AgentResult<()> {
        let mut s = self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("mock mutex poisoned: {e}")))?;
        s.calls.push(MockCall {
            op: "prepare",
            transaction_id: transaction_id.to_string(),
            deployment_id: Some(candidate.deployment_id.clone()),
        });
        if let Some(sleep) = s.behavior.prepare_sleep.take() {
            // Best-effort; tests use short sleeps to drive timeouts. We
            // drop the guard so the sleep doesn't hold the mutex.
            drop(s);
            std::thread::sleep(sleep);
            let mut s = self
                .inner
                .lock()
                .map_err(|e| AgentError::Internal(format!("mock mutex poisoned: {e}")))?;
            if let Some(kind) = s.behavior.fail_prepare.take() {
                return Err(kind.into_error());
            }
            s.prepared = Some(candidate.deployment_id.clone());
            return Ok(());
        }
        if let Some(kind) = s.behavior.fail_prepare.take() {
            return Err(kind.into_error());
        }
        s.prepared = Some(candidate.deployment_id.clone());
        Ok(())
    }

    fn warm(
        &self,
        transaction_id: &str,
        candidate: &CandidateRef,
        timeout: Duration,
    ) -> AgentResult<WorkerReadiness> {
        let started = Instant::now();
        let mut s = self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("mock mutex poisoned: {e}")))?;
        s.calls.push(MockCall {
            op: "warm",
            transaction_id: transaction_id.to_string(),
            deployment_id: Some(candidate.deployment_id.clone()),
        });
        if let Some(sleep) = s.behavior.warm_sleep.take() {
            drop(s);
            if sleep > timeout {
                std::thread::sleep(timeout);
                return Err(AgentError::WorkerTimeout(
                    u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                ));
            }
            std::thread::sleep(sleep);
            let mut s = self
                .inner
                .lock()
                .map_err(|e| AgentError::Internal(format!("mock mutex poisoned: {e}")))?;
            if let Some(kind) = s.behavior.fail_warm.take() {
                return Err(kind.into_error());
            }
            if s.behavior.warm_not_ready {
                return Ok(WorkerReadiness {
                    deployment_id: candidate.deployment_id.clone(),
                    ready: false,
                });
            }
            return Ok(WorkerReadiness {
                deployment_id: candidate.deployment_id.clone(),
                ready: true,
            });
        }
        if let Some(kind) = s.behavior.fail_warm.take() {
            return Err(kind.into_error());
        }
        if s.behavior.warm_not_ready {
            // Honor caller's timeout deterministically: claim not ready.
            let _ = started;
            return Ok(WorkerReadiness {
                deployment_id: candidate.deployment_id.clone(),
                ready: false,
            });
        }
        Ok(WorkerReadiness {
            deployment_id: candidate.deployment_id.clone(),
            ready: true,
        })
    }

    fn promote(&self, transaction_id: &str, candidate: &CandidateRef) -> AgentResult<()> {
        let mut s = self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("mock mutex poisoned: {e}")))?;
        s.calls.push(MockCall {
            op: "promote",
            transaction_id: transaction_id.to_string(),
            deployment_id: Some(candidate.deployment_id.clone()),
        });
        if let Some(kind) = s.behavior.fail_promote.take() {
            return Err(kind.into_error());
        }
        s.active = Some(candidate.deployment_id.clone());
        Ok(())
    }

    fn unload(&self, deployment_id: &str) {
        if let Ok(mut s) = self.inner.lock() {
            s.calls.push(MockCall {
                op: "unload",
                transaction_id: String::new(),
                deployment_id: Some(deployment_id.to_string()),
            });
        }
    }

    fn active_deployment_id(&self) -> AgentResult<Option<String>> {
        Ok(self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("mock mutex poisoned: {e}")))?
            .active
            .clone())
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_used,
        clippy::default_trait_access
    )]
    use super::{
        AgentErrorKind, CandidateRef, MockBehavior, MockWorkerControl, ProcessWorkerControl,
        WorkerControl,
    };
    use crate::config::{AgentConfig, ControlTransport, WorkerControlMode};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Duration;
    use tempfile::TempDir;
    use tensorplate_protocol::bundle_manifest::DeviceFamily;

    fn candidate(id: &str) -> CandidateRef {
        CandidateRef {
            deployment_id: id.into(),
            staged_path: format!("/staging/{id}"),
            bundle_digest: "sha256:cafe".into(),
            backend_hint: "mock".into(),
            model_class: "vision".into(),
            bundle_name: Some("m".into()),
            bundle_version: Some("1".into()),
            artifact_relative_path: Some("model.bin".into()),
        }
    }

    fn process_config(td: &std::path::Path) -> AgentConfig {
        let mut cfg = AgentConfig {
            schema_version: tensorplate_protocol::SCHEMA_VERSION.to_string(),
            transport: ControlTransport::UnixSocket,
            socket_path: Some(td.join("agent.sock")),
            tcp_bind_host: "127.0.0.1".into(),
            tcp_bind_port: 0,
            state_dir: td.join("state"),
            staging_dir: td.join("staging"),
            available_backends: vec!["mock".into()],
            backend_capabilities: BTreeMap::new(),
            device_memory_bytes: Some(8 * 1024 * 1024 * 1024),
            device_family: DeviceFamily::Any,
            admission_posture: None,
            worker: Default::default(),
            supervision: None,
            runtime_version: Some("0.1.0".into()),
        };
        cfg.worker.mode = WorkerControlMode::Process;
        cfg.worker.serving_binary_path = Some(PathBuf::from("/usr/local/bin/tensorplate-serving"));
        cfg.worker.serving_config_dir = Some(td.join("worker-configs"));
        cfg.validate().expect("valid")
    }

    #[test]
    fn happy_path_records_calls() {
        let m = MockWorkerControl::new();
        let c = candidate("d1");
        m.prepare("tx", &c, Duration::from_millis(100)).expect("p");
        let r = m.warm("tx", &c, Duration::from_millis(100)).expect("w");
        assert!(r.ready);
        m.promote("tx", &c).expect("promote");
        let calls = m.calls().expect("calls");
        let ops: Vec<&'static str> = calls.iter().map(|c| c.op).collect();
        assert_eq!(ops, vec!["prepare", "warm", "promote"]);
        assert_eq!(m.active_deployment_id().expect("active"), Some("d1".into()));
    }

    #[test]
    fn prepare_can_be_injected_to_fail() {
        let m = MockWorkerControl::with_behavior(MockBehavior {
            fail_prepare: Some(AgentErrorKind::LoadFailed("artifact unreadable".into())),
            ..Default::default()
        });
        let c = candidate("d1");
        let err = m
            .prepare("tx", &c, Duration::from_millis(10))
            .expect_err("fail");
        assert!(matches!(err, super::AgentError::WorkerControl(_)));
    }

    #[test]
    fn warm_can_be_injected_to_not_ready() {
        let m = MockWorkerControl::with_behavior(MockBehavior {
            warm_not_ready: true,
            ..Default::default()
        });
        let c = candidate("d1");
        m.prepare("tx", &c, Duration::from_millis(10)).expect("p");
        let r = m.warm("tx", &c, Duration::from_millis(10)).expect("w");
        assert!(!r.ready);
    }

    #[test]
    fn process_worker_renders_serving_config_for_candidate() {
        let td = TempDir::new().expect("td");
        let cfg = process_config(td.path());
        let worker = ProcessWorkerControl::new(&cfg).expect("worker");
        let c = candidate("deploy-1");
        let (path, rendered) = worker.render_config(&c, 18080).expect("render");
        assert!(path.is_file());
        assert_eq!(rendered["deployment"]["model"]["model_id"], "deploy-1");
        assert_eq!(
            rendered["deployment"]["model"]["artifact_path"],
            "/staging/deploy-1/model.bin"
        );
        assert_eq!(rendered["bind"]["port"], 18080);
    }
}
