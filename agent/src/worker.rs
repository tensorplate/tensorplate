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
// Two implementations:
//
//   - [`MockWorkerControl`]: deterministic in-process implementation
//     used by the agent integration tests and by the V01-E08 host CI
//     matrix where `tensorplate-serving` is not available.
//   - The real V01-E07 worker client lands in a follow-on Feature; the
//     trait is intentionally narrow so the implementation can be added
//     without revising the coordinator code.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tensorplate_protocol::worker_control::CandidateRef;

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
}
