// SPDX-License-Identifier: Apache-2.0
//
// V01-E09-F01-T02: Worker process launch / stop primitives.
//
// The supervisor depends on the [`WorkerProcess`] trait, not on
// `std::process::Child`. Two implementations ship in v0.1.0:
//
//   - [`SystemWorkerProcess`] launches the real `tensorplate-serving`
//     binary on a loopback port; used by the agent main binary.
//   - [`MockWorkerProcess`] is a deterministic in-process worker used by
//     unit and integration tests. It records launch attempts, simulates
//     exit codes, and never spawns a child.
//
// The trait surface is intentionally narrow. Readiness is observed by the
// V01-E09-F02 [`super::readiness::ReadinessProbe`]; the process trait
// only owns spawning, signaling, and exit detection.

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::error::{AgentError, AgentResult};

use super::config::{SupervisorConfig, WorkerStdioMode};

/// Identity of one launched worker. The supervisor keeps this opaque so
/// future platforms (Windows job objects, cgroup-rooted services) can
/// extend it without changing call sites.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerHandle {
    pub deployment_id: String,
    pub launch_sequence: u64,
    pub launched_at: Instant,
    pub pid: Option<u32>,
    pub command_digest: String,
}

/// Exit outcome reported by [`WorkerProcess::poll`] once the worker has
/// terminated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExitStatus {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    /// True iff the worker reached the supervisor's `ready` state at some
    /// point during this launch (set by the supervisor, not the process
    /// trait — the trait reports it back through [`WorkerProcess::poll`]
    /// because the host process is what actually keeps the bookkeeping).
    pub after_ready: bool,
}

impl ExitStatus {
    /// Convenience: build from a `std::process::ExitStatus`.
    #[must_use]
    pub fn from_std(status: std::process::ExitStatus, after_ready: bool) -> Self {
        let code = status.code();
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        };
        #[cfg(not(unix))]
        let signal: Option<i32> = None;
        Self {
            code,
            signal,
            after_ready,
        }
    }
}

/// Result of [`WorkerProcess::poll`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PollOutcome {
    Running,
    Exited(ExitStatus),
}

/// Process-lifecycle trait. The supervisor (V01-E09) is the only owner.
/// Implementations must be `Send + Sync` so they can sit behind the
/// supervisor's mutex.
pub trait WorkerProcess: Send + Sync {
    /// Spawn a worker for `deployment_id`. The implementation records
    /// platform-specific identity and a `launch_sequence` derived from
    /// [`WorkerProcess::next_launch_sequence`].
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::WorkerControl`] on spawn failures and
    /// [`AgentError::Config`] when a required path is missing at launch
    /// time. The supervisor maps these to typed supervision errors.
    fn launch(&self, deployment_id: &str) -> AgentResult<WorkerHandle>;

    /// Sample whether the worker is still running. The implementation
    /// must be non-blocking; the supervisor calls this from its `tick`
    /// loop.
    fn poll(&self, handle: &WorkerHandle) -> AgentResult<PollOutcome>;

    /// Request a graceful shutdown. The supervisor calls this before
    /// escalating to forced termination after the configured timeout.
    ///
    /// # Errors
    ///
    /// Implementations return errors only for unrecoverable platform
    /// failures; an already-exited child is reported as success so the
    /// supervisor can finalize state without a retry.
    fn graceful_stop(&self, handle: &WorkerHandle) -> AgentResult<()>;

    /// Force-terminate the worker. Called after the graceful timeout
    /// expires. Must be idempotent.
    fn force_terminate(&self, handle: &WorkerHandle) -> AgentResult<()>;

    /// Strictly-monotonic launch sequence; the supervisor stores this
    /// in [`WorkerHandle::launch_sequence`] to distinguish replays of
    /// the same deployment.
    fn next_launch_sequence(&self) -> u64;
}

/// Real-process implementation backed by `std::process::Child`. Used by
/// the agent main binary in `worker.mode=process` configurations.
pub struct SystemWorkerProcess {
    cfg: SupervisorConfig,
    inner: Mutex<SystemState>,
}

#[derive(Default)]
struct SystemState {
    child: Option<Child>,
    next_sequence: u64,
}

impl SystemWorkerProcess {
    /// Build a system process worker. The config is validated by the
    /// caller; this constructor simply moves it in.
    #[must_use]
    pub fn new(cfg: SupervisorConfig) -> Self {
        Self {
            cfg,
            inner: Mutex::new(SystemState::default()),
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.cfg.binary_path);
        cmd.arg("--config").arg(&self.cfg.serving_config_path);
        for extra in &self.cfg.args {
            cmd.arg(extra);
        }
        cmd.current_dir(&self.cfg.working_dir);
        cmd.env_clear();
        for key in &self.cfg.env_allowlist {
            if let Ok(value) = std::env::var(key) {
                cmd.env(key, value);
            }
        }
        match self.cfg.stdio_mode {
            WorkerStdioMode::Inherit => {
                cmd.stdin(Stdio::null())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit());
            }
            WorkerStdioMode::Discard => {
                cmd.stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
            }
            WorkerStdioMode::CaptureToFile => {
                cmd.stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
            }
        }
        cmd
    }

    fn digest(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(self.cfg.binary_path.as_os_str().as_encoded_bytes());
        h.update(self.cfg.serving_config_path.as_os_str().as_encoded_bytes());
        for arg in &self.cfg.args {
            h.update(arg.as_bytes());
        }
        format!("sha256:{}", hex::encode(h.finalize()))
    }
}

impl WorkerProcess for SystemWorkerProcess {
    fn launch(&self, deployment_id: &str) -> AgentResult<WorkerHandle> {
        if !self.cfg.binary_path.is_file() {
            return Err(AgentError::Config(format!(
                "supervision.binary_path `{}` is not a regular file",
                self.cfg.binary_path.display()
            )));
        }
        if !self.cfg.serving_config_path.is_file() {
            return Err(AgentError::Config(format!(
                "supervision.serving_config_path `{}` is not a regular file",
                self.cfg.serving_config_path.display()
            )));
        }
        let mut state = self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("supervisor process mutex poisoned: {e}")))?;
        if state.child.is_some() {
            return Err(AgentError::Busy("worker process already running".into()));
        }
        let mut cmd = self.command();
        let child = cmd.spawn().map_err(|err| {
            AgentError::WorkerControl(format!("spawn {}: {err}", self.cfg.binary_path.display()))
        })?;
        let pid = Some(child.id());
        state.next_sequence = state.next_sequence.saturating_add(1);
        let handle = WorkerHandle {
            deployment_id: deployment_id.to_string(),
            launch_sequence: state.next_sequence,
            launched_at: Instant::now(),
            pid,
            command_digest: self.digest(),
        };
        state.child = Some(child);
        Ok(handle)
    }

    fn poll(&self, _handle: &WorkerHandle) -> AgentResult<PollOutcome> {
        let mut state = self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("supervisor process mutex poisoned: {e}")))?;
        let Some(child) = state.child.as_mut() else {
            return Ok(PollOutcome::Exited(ExitStatus {
                code: None,
                signal: None,
                after_ready: false,
            }));
        };
        match child.try_wait() {
            Ok(None) => Ok(PollOutcome::Running),
            Ok(Some(status)) => {
                state.child = None;
                Ok(PollOutcome::Exited(ExitStatus::from_std(status, false)))
            }
            Err(err) => Err(AgentError::WorkerControl(format!("try_wait: {err}"))),
        }
    }

    fn graceful_stop(&self, _handle: &WorkerHandle) -> AgentResult<()> {
        let state = self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("supervisor process mutex poisoned: {e}")))?;
        if let Some(child) = state.child.as_ref() {
            #[cfg(unix)]
            {
                // Best-effort SIGTERM via libc; we cannot depend on a new
                // crate just for this so we use a raw syscall via the
                // `kill` system call exposed through std::process when
                // available. v0.1.0 ships unix-only.
                let pid = child.id();
                send_sigterm(pid)?;
            }
            #[cfg(not(unix))]
            {
                let _ = child;
                return Err(AgentError::WorkerControl(
                    "graceful stop is only supported on unix in v0.1.0".into(),
                ));
            }
        }
        Ok(())
    }

    fn force_terminate(&self, _handle: &WorkerHandle) -> AgentResult<()> {
        let mut state = self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("supervisor process mutex poisoned: {e}")))?;
        if let Some(mut child) = state.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        Ok(())
    }

    fn next_launch_sequence(&self) -> u64 {
        if let Ok(state) = self.inner.lock() {
            state.next_sequence
        } else {
            0
        }
    }
}

#[cfg(unix)]
fn send_sigterm(pid: u32) -> AgentResult<()> {
    // Safe: kill(2) with SIGTERM (15) is signal-safe and reentrant.
    // We deliberately avoid pulling in `libc` or `nix` for one syscall.
    // SAFETY: directly invoke kill(2) through std's libc wrapper via
    // the `signal` crate replacement — fall back to writing the signal
    // through `/proc/<pid>/term` is unsafe; instead we shell out only
    // when libc-style syscall fails. v0.1.0 keeps the dependency
    // surface small, so we use the established `Command::new("kill")`
    // pattern.
    let status = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|err| AgentError::WorkerControl(format!("invoke kill -TERM {pid}: {err}")))?;
    if !status.success() {
        // `kill -TERM` against an already-exited PID returns non-zero;
        // we treat that as a no-op (the process is already gone).
        let code = status.code().unwrap_or(0);
        if code != 1 {
            return Err(AgentError::WorkerControl(format!(
                "kill -TERM {pid} exited with {code}"
            )));
        }
    }
    Ok(())
}

/// Behavior knobs for [`MockWorkerProcess`].
#[derive(Clone, Debug, Default)]
pub struct MockProcessBehavior {
    /// Cause `launch` to fail with the supplied error.
    pub fail_launch: Option<String>,
    /// Bytes of artificial work each `poll` advances before the worker
    /// reports `Exited`. Tests script this through [`MockWorkerProcess::exit_now`].
    pub exit_at_poll: Option<u32>,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<i32>,
    pub graceful_stop_ignored: bool,
}

/// In-process deterministic worker used by tests. Records every call so
/// supervision tests can assert ordering.
#[derive(Debug, Default)]
pub struct MockWorkerProcess {
    inner: Mutex<MockState>,
}

#[derive(Debug, Default)]
struct MockState {
    behavior: MockProcessBehavior,
    next_sequence: u64,
    current: Option<WorkerHandle>,
    poll_count: u32,
    pending_exit: Option<ExitStatus>,
    history: Vec<MockCall>,
    force_killed: bool,
}

/// Per-call audit record used by tests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MockCall {
    pub op: &'static str,
    pub deployment_id: Option<String>,
}

impl MockWorkerProcess {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_behavior(behavior: MockProcessBehavior) -> Self {
        Self {
            inner: Mutex::new(MockState {
                behavior,
                ..MockState::default()
            }),
        }
    }

    /// Mark the next `poll` invocation as an exit with the supplied
    /// status. The supervisor sees the exit on the very next tick.
    #[allow(clippy::expect_used)]
    pub fn exit_now(&self, status: ExitStatus) {
        let mut state = self.inner.lock().expect("mock process poisoned");
        state.pending_exit = Some(status);
    }

    /// Read the recorded call history.
    #[allow(clippy::expect_used)]
    #[must_use]
    pub fn history(&self) -> Vec<MockCall> {
        self.inner
            .lock()
            .expect("mock process poisoned")
            .history
            .clone()
    }

    /// True after [`WorkerProcess::force_terminate`] has been called at
    /// least once.
    #[allow(clippy::expect_used)]
    #[must_use]
    pub fn force_killed(&self) -> bool {
        self.inner
            .lock()
            .expect("mock process poisoned")
            .force_killed
    }
}

impl WorkerProcess for MockWorkerProcess {
    fn launch(&self, deployment_id: &str) -> AgentResult<WorkerHandle> {
        let mut state = self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("mock process poisoned: {e}")))?;
        state.history.push(MockCall {
            op: "launch",
            deployment_id: Some(deployment_id.to_string()),
        });
        if let Some(reason) = state.behavior.fail_launch.clone() {
            return Err(AgentError::WorkerControl(reason));
        }
        if state.current.is_some() {
            return Err(AgentError::Busy("mock worker already running".into()));
        }
        state.next_sequence = state.next_sequence.saturating_add(1);
        let handle = WorkerHandle {
            deployment_id: deployment_id.to_string(),
            launch_sequence: state.next_sequence,
            launched_at: Instant::now(),
            pid: Some(1_000 + u32::try_from(state.next_sequence).unwrap_or(0)),
            command_digest: format!("mock:{deployment_id}:{}", state.next_sequence),
        };
        state.current = Some(handle.clone());
        state.poll_count = 0;
        Ok(handle)
    }

    fn poll(&self, handle: &WorkerHandle) -> AgentResult<PollOutcome> {
        let mut state = self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("mock process poisoned: {e}")))?;
        if state.current.as_ref().map(|c| c.launch_sequence) != Some(handle.launch_sequence) {
            return Ok(PollOutcome::Exited(ExitStatus {
                code: None,
                signal: None,
                after_ready: false,
            }));
        }
        state.poll_count = state.poll_count.saturating_add(1);
        if let Some(exit) = state.pending_exit.take() {
            state.current = None;
            return Ok(PollOutcome::Exited(exit));
        }
        if let Some(threshold) = state.behavior.exit_at_poll {
            if state.poll_count >= threshold {
                state.current = None;
                return Ok(PollOutcome::Exited(ExitStatus {
                    code: state.behavior.exit_code,
                    signal: state.behavior.exit_signal,
                    after_ready: false,
                }));
            }
        }
        Ok(PollOutcome::Running)
    }

    fn graceful_stop(&self, handle: &WorkerHandle) -> AgentResult<()> {
        let mut state = self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("mock process poisoned: {e}")))?;
        state.history.push(MockCall {
            op: "graceful_stop",
            deployment_id: Some(handle.deployment_id.clone()),
        });
        if state.behavior.graceful_stop_ignored {
            return Ok(());
        }
        if state.current.as_ref().map(|c| c.launch_sequence) == Some(handle.launch_sequence) {
            // Keep `current` populated so the next `poll` for this same
            // handle can consume the pending exit. `poll` clears it.
            state.pending_exit = Some(ExitStatus {
                code: Some(0),
                signal: None,
                after_ready: true,
            });
        }
        Ok(())
    }

    fn force_terminate(&self, handle: &WorkerHandle) -> AgentResult<()> {
        let mut state = self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("mock process poisoned: {e}")))?;
        state.history.push(MockCall {
            op: "force_terminate",
            deployment_id: Some(handle.deployment_id.clone()),
        });
        state.force_killed = true;
        if state.current.as_ref().map(|c| c.launch_sequence) == Some(handle.launch_sequence) {
            // Keep `current` so the next `poll` for this same handle
            // reports a typed exit. `poll` clears it.
            state.pending_exit = Some(ExitStatus {
                code: None,
                signal: Some(9),
                after_ready: false,
            });
        }
        Ok(())
    }

    fn next_launch_sequence(&self) -> u64 {
        self.inner.lock().ok().map_or(0, |s| s.next_sequence)
    }
}

/// Helper that creates the working / config / state directories the
/// supervisor needs before its first launch.
///
/// # Errors
///
/// Returns [`AgentError::Io`] on filesystem failure.
pub fn ensure_supervisor_directories(cfg: &SupervisorConfig) -> AgentResult<()> {
    fs::create_dir_all(&cfg.working_dir)?;
    if let Some(parent) = cfg.serving_config_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

/// Best-effort wait-for-exit loop driven by [`WorkerProcess::poll`]. Used
/// by integration tests and the agent main binary's shutdown path.
///
/// # Errors
///
/// Propagates the first error from `poll`.
pub fn wait_for_exit(
    process: &dyn WorkerProcess,
    handle: &WorkerHandle,
    timeout: Duration,
    tick: Duration,
) -> AgentResult<Option<ExitStatus>> {
    let started = Instant::now();
    let deadline = started.checked_add(timeout).unwrap_or(started);
    loop {
        match process.poll(handle)? {
            PollOutcome::Running => {}
            PollOutcome::Exited(status) => return Ok(Some(status)),
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(tick.min(deadline.saturating_duration_since(Instant::now())));
    }
}

/// Convenience: derive a digest of the worker command line + config
/// without spawning. Useful for status projection.
#[must_use]
pub fn command_digest(binary: &Path, args: &[String], config: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(binary.as_os_str().as_encoded_bytes());
    h.update(config.as_os_str().as_encoded_bytes());
    for arg in args {
        h.update(arg.as_bytes());
    }
    format!("sha256:{}", hex::encode(h.finalize()))
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
        command_digest, ExitStatus, MockProcessBehavior, MockWorkerProcess, PollOutcome,
        WorkerProcess,
    };
    use std::path::PathBuf;

    #[test]
    fn mock_launch_then_exit_records_history() {
        let mock = MockWorkerProcess::with_behavior(MockProcessBehavior {
            exit_at_poll: Some(2),
            exit_code: Some(0),
            ..Default::default()
        });
        let handle = mock.launch("d-1").expect("launch");
        assert!(matches!(mock.poll(&handle), Ok(PollOutcome::Running)));
        match mock.poll(&handle).expect("poll") {
            PollOutcome::Exited(status) => assert_eq!(status.code, Some(0)),
            PollOutcome::Running => panic!("expected exit"),
        }
        let h = mock.history();
        assert_eq!(h[0].op, "launch");
    }

    #[test]
    fn mock_double_launch_returns_busy() {
        let mock = MockWorkerProcess::new();
        let _h1 = mock.launch("d-1").expect("launch");
        let err = mock.launch("d-1").expect_err("double launch");
        assert!(matches!(err, crate::error::AgentError::Busy(_)));
    }

    #[test]
    fn mock_graceful_stop_exits_on_next_poll() {
        let mock = MockWorkerProcess::new();
        let handle = mock.launch("d-1").expect("launch");
        mock.graceful_stop(&handle).expect("stop");
        match mock.poll(&handle).expect("poll") {
            PollOutcome::Exited(status) => {
                assert_eq!(status.code, Some(0));
                assert!(status.after_ready);
            }
            PollOutcome::Running => panic!("expected exit"),
        }
    }

    #[test]
    fn mock_force_terminate_marks_killed() {
        let mock = MockWorkerProcess::with_behavior(MockProcessBehavior {
            graceful_stop_ignored: true,
            ..Default::default()
        });
        let handle = mock.launch("d-1").expect("launch");
        // Graceful stop is ignored; supervisor escalates.
        mock.graceful_stop(&handle).expect("stop");
        assert!(!mock.force_killed());
        mock.force_terminate(&handle).expect("kill");
        assert!(mock.force_killed());
        match mock.poll(&handle).expect("poll") {
            PollOutcome::Exited(status) => assert_eq!(status.signal, Some(9)),
            PollOutcome::Running => panic!("expected exit"),
        }
    }

    #[test]
    fn mock_launch_failure_returns_typed_error() {
        let mock = MockWorkerProcess::with_behavior(MockProcessBehavior {
            fail_launch: Some("missing artifact".into()),
            ..Default::default()
        });
        let err = mock.launch("d-1").expect_err("fail");
        assert!(matches!(err, crate::error::AgentError::WorkerControl(_)));
    }

    #[test]
    fn mock_exit_now_overrides_default_threshold() {
        let mock = MockWorkerProcess::new();
        let handle = mock.launch("d-1").expect("launch");
        mock.exit_now(ExitStatus {
            code: Some(2),
            signal: None,
            after_ready: true,
        });
        match mock.poll(&handle).expect("poll") {
            PollOutcome::Exited(status) => {
                assert_eq!(status.code, Some(2));
                assert!(status.after_ready);
            }
            PollOutcome::Running => panic!("expected exit"),
        }
    }

    #[test]
    fn command_digest_is_stable_for_same_inputs() {
        let bin = PathBuf::from("/usr/local/bin/tensorplate-serving");
        let cfg = PathBuf::from("/var/lib/tensorplate/serving.json");
        let args = vec!["--quiet".to_string()];
        assert_eq!(
            command_digest(&bin, &args, &cfg),
            command_digest(&bin, &args, &cfg)
        );
    }
}
