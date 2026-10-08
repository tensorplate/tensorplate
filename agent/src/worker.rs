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
//   - [`crate::registry::MemberRegistry`]: the process-backed
//     implementation. This module keeps what it shares with tests: the
//     control-socket spawn, stderr forwarding with the startup failure
//     record, and the `/health` read.
//   - [`from_config`]: composition-root selector used by the binary.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tensorplate_protocol::worker_control::CandidateRef;
use tensorplate_protocol::ErrorCode;

use crate::config::{AgentConfig, WorkerControlMode};
use crate::error::{AgentError, AgentResult};

/// Pass the close-on-exec child socket as fd 0 without a pre-exec hook.
/// Consuming the command closes its parent-side stdin copy before returning.
#[cfg(unix)]
pub fn spawn_with_control(
    mut command: Command,
) -> AgentResult<(Child, std::os::unix::net::UnixStream)> {
    let (agent, worker) = std::os::unix::net::UnixStream::pair()?;
    let worker: std::os::fd::OwnedFd = worker.into();
    command.stdin(Stdio::from(worker));
    let child = command.spawn()?;
    drop(command);
    Ok((child, agent))
}

/// Send SIGTERM to a child this process started and has not reaped, so the
/// pid still names it.
#[cfg(unix)]
pub(crate) fn send_sigterm(pid: u32) -> std::io::Result<()> {
    let pid = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .ok_or(std::io::ErrorKind::InvalidInput)?;
    Ok(rustix::process::kill_process(
        pid,
        rustix::process::Signal::Term,
    )?)
}

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

/// Build the configured worker-control implementation. Process mode is the
/// member registry, drawing its generations from `store`.
///
/// # Errors
///
/// Returns [`AgentError::Config`] if process mode is selected without the
/// required serving-worker process fields.
pub fn from_config(
    config: &AgentConfig,
    store: &Arc<crate::state::StateStore>,
) -> AgentResult<Arc<dyn WorkerControl>> {
    match config.worker.mode {
        WorkerControlMode::Mock => Ok(Arc::new(MockWorkerControl::new())),
        #[cfg(unix)]
        WorkerControlMode::Process => Ok(Arc::new(crate::registry::MemberRegistry::from_config(
            config,
            crate::registry::durable_generations(store.clone()),
            agent_stderr_sink(),
        )?)),
        #[cfg(not(unix))]
        WorkerControlMode::Process => {
            let _ = store;
            Err(AgentError::Config(
                "worker.mode `process` needs a Unix host".into(),
            ))
        }
    }
}

/// Receives every stderr line of the serving workers this agent starts,
/// newline included, in the order each worker wrote them.
pub type WorkerStderrSink = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// A longer stderr line is forwarded in pieces and never read as a record.
const MAX_STDERR_LINE_BYTES: u64 = 16 * 1024;

/// How long an exited candidate's stderr is given to finish arriving.
const STDERR_DRAIN_WAIT: Duration = Duration::from_secs(1);

/// The `message` of the record a serving worker ends its stderr with when
/// it cannot start; `serving_worker/src/main.cpp` writes it.
const STARTUP_FAILURE_MESSAGE: &str = "worker startup failed";

#[derive(Clone, Debug, Eq, PartialEq)]
struct StartupFailure {
    code: ErrorCode,
    message: String,
}

#[derive(Default)]
pub(crate) struct WorkerStderr {
    startup_failure: Option<StartupFailure>,
    closed: bool,
}

fn parse_startup_failure(line: &[u8]) -> Option<StartupFailure> {
    let record: serde_json::Value = serde_json::from_slice(line).ok()?;
    if record.get("component")?.as_str()? != "serving"
        || record.get("message")?.as_str()? != STARTUP_FAILURE_MESSAGE
    {
        return None;
    }
    let fields = record.get("fields")?;
    Some(StartupFailure {
        code: serde_json::from_value(fields.get("code")?.clone()).ok()?,
        message: fields.get("message")?.as_str()?.to_string(),
    })
}

/// Copy a worker's stderr to `sink` line by line until it closes, keeping
/// the startup failure record if one arrives.
pub(crate) fn forward_stderr(
    stderr: ChildStderr,
    sink: WorkerStderrSink,
    shared: Arc<Mutex<WorkerStderr>>,
) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = Vec::new();
        let mut mid_line = false;
        loop {
            line.clear();
            let read = (&mut reader)
                .take(MAX_STDERR_LINE_BYTES)
                .read_until(b'\n', &mut line);
            if !matches!(read, Ok(n) if n > 0) {
                break;
            }
            // Forwarded before it is read, so a deploy answered from the
            // record never precedes the record's own line in the journal.
            sink(&line);
            let whole_line = !mid_line && line.ends_with(b"\n");
            mid_line = !line.ends_with(b"\n");
            if whole_line {
                if let Some(failure) = parse_startup_failure(&line) {
                    if let Ok(mut shared) = shared.lock() {
                        shared.startup_failure = Some(failure);
                    }
                }
            }
        }
        if let Ok(mut shared) = shared.lock() {
            shared.closed = true;
        }
    });
}

/// The sink that copies worker stderr to the agent's own.
#[must_use]
pub fn agent_stderr_sink() -> WorkerStderrSink {
    Arc::new(|line| {
        let _ = std::io::stderr().lock().write_all(line);
    })
}

/// What an exited candidate reported, waiting briefly for its last stderr
/// lines; without a record the exit status is all there is.
pub(crate) fn startup_error(status: ExitStatus, stderr: &Mutex<WorkerStderr>) -> AgentError {
    let deadline = Instant::now() + STDERR_DRAIN_WAIT;
    loop {
        if let Ok(mut shared) = stderr.lock() {
            if let Some(failure) = shared.startup_failure.take() {
                return AgentError::WorkerStartupFailed {
                    code: failure.code,
                    message: failure.message,
                };
            }
            if shared.closed {
                break;
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    AgentError::WorkerExited(status.to_string())
}

pub(crate) fn get_health_json(
    host: &str,
    port: u16,
    timeout: Duration,
) -> AgentResult<serde_json::Value> {
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
    use super::{AgentErrorKind, CandidateRef, MockBehavior, MockWorkerControl, WorkerControl};
    use std::time::Duration;

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
    fn startup_failure_is_read_only_from_the_workers_startup_record() {
        let record = br#"{"component":"serving","fields":{"code":"oom_error","message":"m"},"level":"error","message":"worker startup failed","ts_ns":1}"#;
        assert_eq!(
            super::parse_startup_failure(record),
            Some(super::StartupFailure {
                code: tensorplate_protocol::ErrorCode::OomError,
                message: "m".into(),
            })
        );
        for other in [
            &br#"{"component":"serving","fields":{"code":"oom_error","message":"m"},"level":"error","message":"session load failed"}"#[..],
            br#"{"component":"sidecar","fields":{"code":"oom_error","message":"m"},"message":"worker startup failed"}"#,
            br#"{"component":"serving","fields":{"code":"not_a_code","message":"m"},"message":"worker startup failed"}"#,
            br#"{"component":"serving","fields":{"code":"oom_error"},"message":"worker startup failed"}"#,
            br#"{"component":"serving","message":"worker startup failed"}"#,
            b"worker startup failed",
        ] {
            assert_eq!(super::parse_startup_failure(other), None);
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_overlong_stderr_line_is_forwarded_in_full_and_never_read_as_a_record() {
        use std::process::{Command, Stdio};
        use std::sync::{Arc, Mutex};

        let record = r#"{"component":"serving","fields":{"code":"oom_error","message":"m"},"message":"worker startup failed"}"#;
        let padding = usize::try_from(super::MAX_STDERR_LINE_BYTES).expect("usize");
        let script = format!(
            "head -c {padding} /dev/zero | tr '\\0' 'x' >&2; printf '%s\\n' '{record}' >&2"
        );
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(script)
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");
        let forwarded = Arc::new(Mutex::new(Vec::new()));
        let sink = forwarded.clone();
        let shared = Arc::new(Mutex::new(super::WorkerStderr::default()));
        super::forward_stderr(
            child.stderr.take().expect("stderr"),
            Arc::new(move |line: &[u8]| sink.lock().expect("lock").extend_from_slice(line)),
            shared.clone(),
        );
        let status = child.wait().expect("wait");

        let err = super::startup_error(status, &shared);
        assert!(matches!(err, super::AgentError::WorkerExited(_)), "{err}");
        let forwarded = forwarded.lock().expect("lock");
        assert_eq!(forwarded.len(), padding + record.len() + 1);
        assert!(forwarded.ends_with(format!("{record}\n").as_bytes()));
    }
}
