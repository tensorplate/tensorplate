// SPDX-License-Identifier: Apache-2.0

//! Generation-keyed registry of the serving workers this agent runs.
//!
//! Every worker process the agent starts is a member generation here: the
//! registry allocates its generation, renders its serving config, starts it
//! with its control socket, judges its readiness, and stops and reaps it.
//! The deploy coordinator and startup recovery reach it through
//! [`WorkerControl`].

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tensorplate_protocol::worker_control::{CandidateRef, MemberRef};

use crate::error::{AgentError, AgentResult};
use crate::worker::{WorkerControl, WorkerReadiness};

/// How long a member asked to stop is given to drain before it is killed.
pub const STOP_DRAIN: Duration = Duration::from_secs(5);

/// What a member's control channel has shown so far.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlContact {
    /// No ledger has arrived yet, or polls are currently unanswered.
    Pending,
    /// The member answers its polls and has reported its ledger.
    Reporting,
    /// Contact stayed lost past the failure deadline; the channel is over.
    Failed,
}

/// One member generation to start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchSpec {
    pub member: MemberRef,
    pub config_path: PathBuf,
}

/// A started worker process together with its control channel.
pub trait MemberProcess: Send {
    /// Whether the process has exited; reaps it when it has.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Io`] when the process cannot be inspected.
    fn exited(&mut self) -> AgentResult<bool>;

    /// Why a process that exited while loading did so.
    fn startup_failure(&mut self) -> AgentError;

    fn control(&self) -> ControlContact;

    /// Ask the process to drain and exit.
    fn terminate(&mut self);

    /// Kill the process and reap it.
    fn kill(&mut self);
}

/// Starts member processes; the seam the registry's tests replace.
pub trait ProcessLauncher: Send + Sync {
    /// Start the worker for `spec` and begin polling its control channel.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::WorkerControl`] when the process cannot be
    /// started or its control channel cannot be opened.
    fn launch(&self, spec: &LaunchSpec) -> AgentResult<Box<dyn MemberProcess>>;
}

/// Reads a worker's `/health`.
pub trait HealthProbe: Send + Sync {
    /// The model the worker listening on `port` reports ready, if any.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::WorkerControl`] when the worker cannot be
    /// reached or answers something else.
    fn ready_model(&self, port: u16, timeout: Duration) -> AgentResult<Option<String>>;
}

/// Hands out the next deployment generation, durably and never twice.
pub type GenerationSource = Arc<dyn Fn() -> AgentResult<u64> + Send + Sync>;

/// Where members listen and how the registry waits on them.
#[derive(Clone, Debug)]
pub struct RegistrySettings {
    pub bind_host: String,
    pub active_port: u16,
    pub candidate_port: u16,
    pub config_dir: PathBuf,
    pub use_mock_session: bool,
    pub status_poll_interval: Duration,
    pub stop_drain: Duration,
}

struct Member {
    deployment_id: String,
    port: u16,
    config_path: PathBuf,
    process: Box<dyn MemberProcess>,
}

#[derive(Default)]
struct Members {
    by_generation: BTreeMap<u64, Member>,
    serving: Option<u64>,
    candidate: Option<u64>,
}

/// The one owner of every serving worker process the agent starts.
pub struct MemberRegistry {
    settings: RegistrySettings,
    launcher: Arc<dyn ProcessLauncher>,
    probe: Arc<dyn HealthProbe>,
    generations: GenerationSource,
    inner: Mutex<Members>,
}

impl MemberRegistry {
    #[must_use]
    pub fn new(
        settings: RegistrySettings,
        launcher: Arc<dyn ProcessLauncher>,
        probe: Arc<dyn HealthProbe>,
        generations: GenerationSource,
    ) -> Self {
        Self {
            settings,
            launcher,
            probe,
            generations,
            inner: Mutex::new(Members::default()),
        }
    }
}

// BEGIN-BEHAVIOUR
impl WorkerControl for MemberRegistry {
    fn prepare(&self, _tx: &str, _candidate: &CandidateRef, _timeout: Duration) -> AgentResult<()> {
        Err(AgentError::Internal("member registry: not implemented".into()))
    }

    fn warm(
        &self,
        _tx: &str,
        _candidate: &CandidateRef,
        _timeout: Duration,
    ) -> AgentResult<WorkerReadiness> {
        Err(AgentError::Internal("member registry: not implemented".into()))
    }

    fn promote(&self, _tx: &str, _candidate: &CandidateRef) -> AgentResult<()> {
        Err(AgentError::Internal("member registry: not implemented".into()))
    }

    fn unload(&self, _deployment_id: &str) {}

    fn active_deployment_id(&self) -> AgentResult<Option<String>> {
        Ok(None)
    }
}
// END-BEHAVIOUR

#[cfg(test)]
mod tests;
