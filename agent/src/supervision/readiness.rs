// SPDX-License-Identifier: Apache-2.0
//
// V01-E09-F02: Worker readiness watcher and actual-state polling.
//
// The supervisor distinguishes process liveness (owned by `WorkerProcess`)
// from serving readiness (this trait). It uses monotonic time everywhere
// and accepts a typed [`ReadinessSample`] suitable for V01-E08 startup
// reconciliation and V01-E11 status projection.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::error::{AgentError, AgentResult};

use super::config::SupervisorConfig;

/// One readiness check outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadinessSample {
    /// `true` when the worker reports `state == "ready"` and the active
    /// deployment matches the expected id.
    pub ready: bool,
    /// `true` when the worker reports a degraded but still-running state.
    pub degraded: bool,
    /// `true` when the worker reports a failed state (typically requires
    /// restart). The supervisor maps this to its degraded/failed policy.
    pub failed: bool,
    /// Deployment id the worker reports as active, if any.
    pub active_deployment: Option<String>,
    /// Backend name the worker advertises.
    pub backend: Option<String>,
    /// Worker queue depth at the moment of the sample.
    pub queue_depth: Option<u64>,
    /// Bounded last-error code surfaced by the worker.
    pub last_error: Option<String>,
}

impl ReadinessSample {
    /// Idle sample used when the supervisor has no worker to probe.
    #[must_use]
    pub fn unknown() -> Self {
        Self {
            ready: false,
            degraded: false,
            failed: false,
            active_deployment: None,
            backend: None,
            queue_depth: None,
            last_error: None,
        }
    }
}

/// Readiness-probe trait. Implementations must be `Send + Sync` so the
/// supervisor can hold them behind an `Arc`.
pub trait ReadinessProbe: Send + Sync {
    /// Sample worker readiness. The supervisor calls this repeatedly until
    /// the startup timeout fires.
    ///
    /// # Errors
    ///
    /// Returns transient I/O errors so the supervisor can decide whether
    /// the failure should count against the readiness budget.
    fn sample(&self, deployment_id: &str) -> AgentResult<ReadinessSample>;
}

/// HTTP-backed probe that targets the V01-E07 serving worker's
/// `/health` endpoint over loopback. Used in production.
pub struct HttpReadinessProbe {
    host: String,
    port: u16,
    timeout: Duration,
}

impl HttpReadinessProbe {
    #[must_use]
    pub fn from_config(cfg: &SupervisorConfig) -> Self {
        Self {
            host: cfg.control_host.clone(),
            port: cfg.control_port,
            timeout: Duration::from_millis(cfg.status_poll_interval_ms.max(50)),
        }
    }
}

impl ReadinessProbe for HttpReadinessProbe {
    fn sample(&self, deployment_id: &str) -> AgentResult<ReadinessSample> {
        let mut stream = TcpStream::connect((self.host.as_str(), self.port)).map_err(|err| {
            AgentError::WorkerControl(format!(
                "readiness connect {}:{}: {err}",
                self.host, self.port
            ))
        })?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        let request = format!(
            "GET /health HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            self.host
        );
        stream.write_all(request.as_bytes())?;
        let mut raw = String::new();
        stream.read_to_string(&mut raw)?;
        let Some((_head, body)) = raw.split_once("\r\n\r\n") else {
            return Err(AgentError::WorkerControl(
                "readiness response missing header terminator".into(),
            ));
        };
        let value: serde_json::Value = serde_json::from_str(body)?;
        let state = value
            .get("state")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let active = value
            .get("active_model_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let backend = value
            .get("backend")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let queue_depth = value.get("queue_depth").and_then(serde_json::Value::as_u64);
        let last_error = value
            .get("last_error_code")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let ready = state == "ready" && active.as_deref() == Some(deployment_id);
        let degraded = state == "degraded";
        let failed = state == "failed";
        Ok(ReadinessSample {
            ready,
            degraded,
            failed,
            active_deployment: active,
            backend,
            queue_depth,
            last_error,
        })
    }
}

/// In-process probe used by tests. Drives readiness deterministically and
/// records the call sequence for assertions.
#[derive(Debug, Default)]
pub struct MockReadinessProbe {
    inner: Mutex<MockState>,
}

#[derive(Debug, Default)]
struct MockState {
    samples: Vec<ReadinessSample>,
    cursor: usize,
    requested: Vec<String>,
    /// When set, every call returns this error until cleared.
    error: Option<String>,
}

impl MockReadinessProbe {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a script of samples; subsequent `sample` calls return them
    /// in order, repeating the last sample once the script is exhausted.
    #[allow(clippy::expect_used)]
    pub fn script(&self, samples: Vec<ReadinessSample>) {
        let mut state = self.inner.lock().expect("mock readiness poisoned");
        state.samples = samples;
        state.cursor = 0;
        state.error = None;
    }

    /// Cause subsequent `sample` calls to return an error.
    #[allow(clippy::expect_used)]
    pub fn fail_with(&self, reason: impl Into<String>) {
        let mut state = self.inner.lock().expect("mock readiness poisoned");
        state.error = Some(reason.into());
    }

    /// Read recorded request ids.
    #[allow(clippy::expect_used)]
    #[must_use]
    pub fn requested(&self) -> Vec<String> {
        self.inner
            .lock()
            .expect("mock readiness poisoned")
            .requested
            .clone()
    }
}

impl ReadinessProbe for MockReadinessProbe {
    fn sample(&self, deployment_id: &str) -> AgentResult<ReadinessSample> {
        let mut state = self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("mock readiness poisoned: {e}")))?;
        state.requested.push(deployment_id.to_string());
        if let Some(reason) = state.error.clone() {
            return Err(AgentError::WorkerControl(reason));
        }
        if state.samples.is_empty() {
            return Ok(ReadinessSample::unknown());
        }
        let idx = state.cursor.min(state.samples.len() - 1);
        let sample = state.samples[idx].clone();
        if state.cursor < state.samples.len() - 1 {
            state.cursor += 1;
        }
        Ok(sample)
    }
}

/// Drive [`ReadinessProbe`] until the worker reports ready or the
/// configured timeout fires. Used by the supervisor during the startup
/// path; tests inject a fake clock through the supervisor `tick` loop
/// instead of calling this directly.
///
/// # Errors
///
/// Returns [`AgentError::WorkerTimeout`] when the deadline fires before
/// the probe reports ready. Transient probe errors are surfaced once the
/// deadline expires; before then, they decrement a bounded error budget
/// to avoid flapping.
pub fn wait_until_ready(
    probe: &dyn ReadinessProbe,
    deployment_id: &str,
    timeout: Duration,
    interval: Duration,
    error_budget: u32,
) -> AgentResult<ReadinessSample> {
    let started = Instant::now();
    let deadline = started.checked_add(timeout).unwrap_or(started);
    let mut remaining_errors = error_budget;
    loop {
        match probe.sample(deployment_id) {
            Ok(s) if s.ready => return Ok(s),
            Ok(_) => {}
            Err(err) if remaining_errors == 0 => return Err(err),
            Err(_) => {
                remaining_errors = remaining_errors.saturating_sub(1);
            }
        }
        if Instant::now() >= deadline {
            return Err(AgentError::WorkerTimeout(
                u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            ));
        }
        std::thread::sleep(interval.min(deadline.saturating_duration_since(Instant::now())));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::default_trait_access)]

    use super::{wait_until_ready, MockReadinessProbe, ReadinessProbe, ReadinessSample};
    use std::time::Duration;

    fn ready(id: &str) -> ReadinessSample {
        ReadinessSample {
            ready: true,
            active_deployment: Some(id.to_string()),
            backend: Some("mock".into()),
            queue_depth: Some(0),
            ..ReadinessSample::unknown()
        }
    }

    fn not_ready() -> ReadinessSample {
        ReadinessSample::unknown()
    }

    fn degraded(id: &str) -> ReadinessSample {
        ReadinessSample {
            degraded: true,
            active_deployment: Some(id.to_string()),
            ..ReadinessSample::unknown()
        }
    }

    #[test]
    fn mock_returns_unknown_by_default() {
        let probe = MockReadinessProbe::new();
        let s = probe.sample("d-1").expect("sample");
        assert!(!s.ready);
        assert_eq!(probe.requested(), vec!["d-1".to_string()]);
    }

    #[test]
    fn mock_walks_script_then_holds_last_sample() {
        let probe = MockReadinessProbe::new();
        probe.script(vec![not_ready(), not_ready(), ready("d-1")]);
        assert!(!probe.sample("d-1").expect("s1").ready);
        assert!(!probe.sample("d-1").expect("s2").ready);
        assert!(probe.sample("d-1").expect("s3").ready);
        // Holds the last sample.
        assert!(probe.sample("d-1").expect("s4").ready);
    }

    #[test]
    fn mock_can_fail_until_recovered() {
        let probe = MockReadinessProbe::new();
        probe.fail_with("connection refused");
        let err = probe.sample("d-1").expect_err("fail");
        assert!(matches!(err, crate::error::AgentError::WorkerControl(_)));
    }

    #[test]
    fn wait_until_ready_returns_immediately_on_ready_sample() {
        let probe = MockReadinessProbe::new();
        probe.script(vec![ready("d-1")]);
        let s = wait_until_ready(
            &probe,
            "d-1",
            Duration::from_millis(50),
            Duration::from_millis(1),
            2,
        )
        .expect("ready");
        assert!(s.ready);
    }

    #[test]
    fn wait_until_ready_times_out_on_persistent_not_ready() {
        let probe = MockReadinessProbe::new();
        probe.script(vec![degraded("d-1")]);
        let err = wait_until_ready(
            &probe,
            "d-1",
            Duration::from_millis(20),
            Duration::from_millis(2),
            2,
        )
        .expect_err("timeout");
        assert!(matches!(err, crate::error::AgentError::WorkerTimeout(_)));
    }
}
