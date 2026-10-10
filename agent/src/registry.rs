// SPDX-License-Identifier: Apache-2.0

//! Generation-keyed registry of the serving workers this agent runs.
//!
//! Every worker process the agent starts is a member generation here: the
//! registry allocates its generation, renders its serving config, starts it
//! with its control socket, judges its readiness, and stops and reaps it.
//! The deploy coordinator and startup recovery reach it through
//! [`WorkerControl`].

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tensorplate_protocol::agent_control::is_valid_deployment_id;
use tensorplate_protocol::agent_state::ErrorRecord;
use tensorplate_protocol::worker_control::{CandidateRef, MemberRef};

use crate::bundle::StagedManifest;
use crate::config::AgentConfig;
use crate::control_channel::{ContactState, ControlChannel};
use crate::error::{AgentError, AgentResult};
use crate::state::StateStore;
use crate::supervision::{MonotonicClock, SystemMonotonicClock};
use crate::worker::{
    forward_stderr, get_health_json, send_sigterm, spawn_with_control, startup_error, MemberEvent,
    WorkerControl, WorkerReadiness, WorkerStderr, WorkerStderrSink, STDERR_DRAIN_WAIT,
};

/// The drain a worker is told to give its sessions once it is asked to stop.
pub const WORKER_DRAIN: Duration = Duration::from_secs(5);

/// How long a member asked to stop is given before it is killed: its own
/// drain, then a margin in which it stops its sidecar and exits.
pub const STOP_DEADLINE: Duration = Duration::from_secs(10);

const _: () = assert!(WORKER_DRAIN.as_millis() < STOP_DEADLINE.as_millis());

/// How long past the stop deadline a stop waits for a launch that had
/// begun on another thread to end and be stopped by that thread.
pub const LAUNCH_GRACE: Duration = Duration::from_secs(2);

/// The longest the agent takes to stop its workers once it is told to stop:
/// a supervision step that was stopping a successor and reading its last
/// stderr, then the registry's stop.
pub const AGENT_STOP_BOUND: Duration = Duration::from_secs(
    2 * STOP_DEADLINE.as_secs() + STDERR_DRAIN_WAIT.as_secs() + LAUNCH_GRACE.as_secs(),
);

const DRAIN_POLL: Duration = Duration::from_millis(10);

/// The wait before the first, second and third restart of a member inside
/// [`RESTART_WINDOW`]; a member that stops with all three used is failed.
pub const RESTART_DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

/// How long a restart counts against its member.
pub const RESTART_WINDOW: Duration = Duration::from_secs(300);

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

    /// Why a process that exited did so.
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
    /// The model the worker listening on `port` reports ready; `None` when
    /// it is not ready or did not answer within `timeout`.
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
    pub worker_drain: Duration,
    pub stop_deadline: Duration,
    /// See [`LAUNCH_GRACE`].
    pub launch_grace: Duration,
    /// How long a restarted generation is given to become ready.
    pub restart_warm_timeout: Duration,
}

struct Member {
    deployment_id: String,
    port: u16,
    config_path: PathBuf,
    process: Box<dyn MemberProcess>,
    /// What this generation was started from, to start its successor.
    recipe: CandidateRef,
    /// Set while a thread holds the member outside the registry's map.
    /// Declared last, so it is released after the process was reaped.
    outside: Option<Outside>,
}

/// Counts a member process, or a launch, that a thread holds outside the
/// registry's map. Taken under the registry's lock; `shutdown` returns once
/// none is left.
struct Outside(Arc<AtomicUsize>);

impl Outside {
    fn new(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count.clone())
    }
}

impl Drop for Outside {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct Members {
    by_generation: BTreeMap<u64, Member>,
    serving: Option<u64>,
    candidate: Option<u64>,
    /// The candidate's deploy gave up on it; it is not promoted any more.
    candidate_abandoned: bool,
    /// Calls to `prepare` that have not returned.
    preparing: usize,
    restart: Restart,
    /// When each restart inside [`RESTART_WINDOW`] was made, oldest first.
    restarts: VecDeque<Instant>,
    /// Set by `shutdown` before it stops the first member; no launch begins
    /// and no failure is recorded from then on.
    stopping: bool,
    outside: Arc<AtomicUsize>,
}

/// What the registry owes the deployment that was serving.
#[derive(Default)]
enum Restart {
    /// Nothing: the serving member, if any, was promoted or came back.
    #[default]
    Idle,
    /// The serving generation stopped; its successor starts once `due` passes.
    Due {
        recipe: CandidateRef,
        port: u16,
        due: Instant,
    },
    /// The supervision step is working on the member outside the lock.
    InHand { deployment_id: String, port: u16 },
    /// The successor holds the serving slot and has until `deadline` to be
    /// ready.
    Loading {
        deadline: Instant,
        probe_at: Instant,
    },
    /// Restarts are used up; nothing is started until a promotion.
    Failed {
        deployment_id: String,
        error: ErrorRecord,
    },
}

/// Why the supervision step took the serving generation out of service.
enum Cause {
    Exited,
    ContactLost,
    NotReady,
}

/// Counts one running `prepare` until it is dropped.
struct Preparing<'a>(&'a MemberRegistry);

impl Drop for Preparing<'_> {
    fn drop(&mut self) {
        if let Ok(mut members) = self.0.inner.lock() {
            members.preparing = members.preparing.saturating_sub(1);
        }
    }
}

/// The one owner of every serving worker process the agent starts.
pub struct MemberRegistry {
    settings: RegistrySettings,
    launcher: Arc<dyn ProcessLauncher>,
    probe: Arc<dyn HealthProbe>,
    generations: GenerationSource,
    clock: Arc<dyn MonotonicClock>,
    inner: Mutex<Members>,
    /// Runs once, just before a restart reads the staged manifest.
    #[cfg(test)]
    before_staged_read: Mutex<Option<Box<dyn FnOnce() + Send>>>,
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
            clock: Arc::new(SystemMonotonicClock),
            inner: Mutex::new(Members::default()),
            #[cfg(test)]
            before_staged_read: Mutex::new(None),
        }
    }

    /// Read restart delays, windows and deadlines from `clock`.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn MonotonicClock>) -> Self {
        self.clock = clock;
        self
    }
}

impl Members {
    fn take_candidate(&mut self) -> Option<(u64, Member)> {
        let generation = self.candidate.take()?;
        self.candidate_abandoned = false;
        Some((generation, self.take(generation)?))
    }

    fn take(&mut self, generation: u64) -> Option<Member> {
        let mut member = self.by_generation.remove(&generation)?;
        member.outside = Some(Outside::new(&self.outside));
        Some(member)
    }

    fn admit(&mut self, generation: u64, mut member: Member) {
        member.outside = None;
        self.by_generation.insert(generation, member);
    }

    /// Whether a deploy holds the member: it is preparing a candidate, or
    /// has one it may still promote. No restart starts meanwhile, so a
    /// promoted generation is never older than the one it replaces.
    fn held_by_a_deploy(&self) -> bool {
        self.preparing > 0 || (self.candidate.is_some() && !self.candidate_abandoned)
    }

    /// The port of the serving member, or of the restart owed to it.
    fn serving_port(&self) -> Option<u16> {
        match &self.restart {
            Restart::Due { port, .. } | Restart::InHand { port, .. } => Some(*port),
            _ => self
                .serving
                .and_then(|generation| self.by_generation.get(&generation))
                .map(|member| member.port),
        }
    }

    fn restart_names(&self, deployment_id: &str) -> bool {
        match &self.restart {
            Restart::Due { recipe, .. } => recipe.deployment_id == deployment_id,
            Restart::InHand {
                deployment_id: owed,
                ..
            }
            | Restart::Failed {
                deployment_id: owed,
                ..
            } => owed == deployment_id,
            Restart::Idle | Restart::Loading { .. } => false,
        }
    }

    /// Whether the supervision step still holds the restart of
    /// `deployment_id` it took, and no stop has begun.
    fn restart_in_hand(&self, deployment_id: &str) -> bool {
        !self.stopping
            && matches!(
                &self.restart,
                Restart::InHand { deployment_id: owed, .. } if owed == deployment_id
            )
    }

    fn forget_restart(&mut self) {
        self.restart = Restart::Idle;
        self.restarts.clear();
    }

    fn take_serving(&mut self) -> Option<Member> {
        let generation = self.serving.take()?;
        self.take(generation)
    }

    fn names(&self, slot: Option<u64>, deployment_id: &str) -> bool {
        slot.and_then(|generation| self.by_generation.get(&generation))
            .is_some_and(|member| member.deployment_id == deployment_id)
    }
}

impl MemberRegistry {
    /// The registry the agent runs: real worker processes, `/health` over
    /// loopback HTTP. Serving configs left by an earlier agent process are
    /// removed, since no worker of that process is this registry's member.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Config`] when the serving binary is not
    /// configured.
    pub fn from_config(
        agent: &AgentConfig,
        generations: GenerationSource,
        stderr_sink: WorkerStderrSink,
    ) -> AgentResult<Self> {
        let binary_path = agent
            .worker
            .serving_binary_path
            .clone()
            .ok_or_else(|| AgentError::Config("worker.serving_binary_path missing".into()))?;
        let settings = RegistrySettings {
            bind_host: agent.worker.serving_bind_host.clone(),
            active_port: agent.worker.serving_bind_port,
            candidate_port: agent.worker.serving_candidate_bind_port,
            config_dir: agent
                .worker
                .serving_config_dir
                .clone()
                .unwrap_or_else(|| agent.state_dir.join("worker-configs")),
            use_mock_session: agent.worker.serving_use_mock_session,
            status_poll_interval: Duration::from_millis(agent.worker.status_poll_interval_ms),
            worker_drain: WORKER_DRAIN,
            stop_deadline: STOP_DEADLINE,
            launch_grace: LAUNCH_GRACE,
            restart_warm_timeout: Duration::from_millis(agent.worker.warm_timeout_ms),
        };
        remove_stale_configs(&settings.config_dir);
        let probe = Arc::new(HttpHealth {
            host: settings.bind_host.clone(),
        });
        let launcher = Arc::new(SystemLauncher {
            binary_path,
            stderr_sink,
        });
        Ok(Self::new(settings, launcher, probe, generations))
    }

    fn members(&self) -> AgentResult<MutexGuard<'_, Members>> {
        self.inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("member registry mutex poisoned: {e}")))
    }

    /// The port a new candidate binds: the one the serving member, or the
    /// restart owed to it, is not on.
    fn candidate_port(&self, members: &Members) -> u16 {
        match members.serving_port() {
            Some(port) if port != self.settings.candidate_port => self.settings.candidate_port,
            _ => self.settings.active_port,
        }
    }

    /// Start the next generation of `recipe` on `port`. Refused once the
    /// registry is stopping; a launch that began before is waited for.
    fn launch(
        &self,
        recipe: &CandidateRef,
        port: u16,
        runner_profile: Option<&str>,
    ) -> AgentResult<(u64, Member)> {
        let outside = {
            let members = self.members()?;
            if members.stopping {
                return Err(AgentError::Stopping);
            }
            Outside::new(&members.outside)
        };
        let generation = (self.generations)()?;
        // A stop gives up waiting on a write that hangs; once the write
        // returns, nothing is started for it.
        if self.members()?.stopping {
            return Err(AgentError::Stopping);
        }
        let spec = LaunchSpec {
            member: MemberRef::new(recipe.deployment_id.clone(), generation),
            config_path: self.render_config(recipe, generation, port, runner_profile)?,
        };
        match self.launcher.launch(&spec) {
            Ok(process) => Ok((
                generation,
                Member {
                    deployment_id: spec.member.deployment_id,
                    port,
                    config_path: spec.config_path,
                    process,
                    recipe: recipe.clone(),
                    outside: Some(outside),
                },
            )),
            Err(err) => {
                let _ = fs::remove_file(&spec.config_path);
                Err(err)
            }
        }
    }

    fn render_config(
        &self,
        candidate: &CandidateRef,
        generation: u64,
        port: u16,
        runner_profile: Option<&str>,
    ) -> AgentResult<PathBuf> {
        fs::create_dir_all(&self.settings.config_dir)?;
        let path = self.settings.config_dir.join(format!(
            "{CONFIG_PREFIX}{}-{generation}{CONFIG_SUFFIX}",
            candidate.deployment_id
        ));
        let artifact_path = candidate.artifact_relative_path.as_ref().map_or_else(
            || candidate.staged_path.clone(),
            |rel| {
                Path::new(&candidate.staged_path)
                    .join(rel)
                    .display()
                    .to_string()
            },
        );
        let mut config = serde_json::json!({
            "schema_version": tensorplate_protocol::SCHEMA_VERSION,
            "bind": {
                "host": self.settings.bind_host,
                "port": port,
                "allow_non_loopback": false
            },
            "health_mode": "local_json",
            "metrics_mode": "prometheus_text",
            "enable_stderr_logs": true,
            "shutdown": {
                "drain_deadline_ms": u64::try_from(self.settings.worker_drain.as_millis())
                    .unwrap_or(u64::MAX)
            },
            "deployment": {
                "use_mock_session": self.settings.use_mock_session,
                "endpoint": candidate.deployment_id,
                "generation": generation,
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
        if let Some(profile) = runner_profile {
            config["deployment"]["model"]["runner_profile"] = profile.into();
        }
        fs::write(&path, serde_json::to_vec_pretty(&config)?)?;
        Ok(path)
    }

    /// Stop `members`: ask each to drain, then kill whatever is still
    /// running when the one stop deadline they share passes.
    fn retire(&self, mut members: Vec<Member>) {
        for member in &mut members {
            member.process.terminate();
        }
        let deadline = Instant::now() + self.settings.stop_deadline;
        for member in &mut members {
            while !matches!(member.process.exited(), Ok(true)) {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    member.process.kill();
                    break;
                }
                std::thread::sleep(DRAIN_POLL.min(remaining));
            }
            let _ = fs::remove_file(&member.config_path);
        }
    }

    /// The prepared candidate's port and control contact. A candidate that
    /// exited or lost its control channel is removed and answered as the
    /// failure it is.
    fn candidate_progress(&self, candidate: &CandidateRef) -> AgentResult<(u16, ControlContact)> {
        let (generation, mut member, exited) = {
            let mut members = self.members()?;
            if members.stopping {
                return Err(AgentError::Stopping);
            }
            let Some(member) = members
                .candidate
                .and_then(|generation| members.by_generation.get_mut(&generation))
            else {
                return Err(AgentError::WorkerNotReady);
            };
            if member.deployment_id != candidate.deployment_id {
                return Err(AgentError::WorkerControl(format!(
                    "prepared candidate `{}` does not match warm request `{}`",
                    member.deployment_id, candidate.deployment_id
                )));
            }
            let exited = member.process.exited()?;
            let contact = member.process.control();
            if !exited && contact != ControlContact::Failed {
                return Ok((member.port, contact));
            }
            let Some((generation, member)) = members.take_candidate() else {
                return Err(AgentError::WorkerNotReady);
            };
            (generation, member, exited)
        };
        let _ = fs::remove_file(&member.config_path);
        if exited {
            return Err(member.process.startup_failure());
        }
        // A worker that stopped answering its control channel is wedged; a
        // request to drain would not reach it either.
        member.process.kill();
        Err(AgentError::WorkerControl(format!(
            "serving worker `{}` generation {generation} lost its control channel while loading",
            member.deployment_id
        )))
    }

    /// Read the serving member while it runs and is ready. One that stopped
    /// is left for the supervision step.
    fn serving<T>(&self, read: impl FnOnce(&Member) -> T) -> AgentResult<Option<T>> {
        let mut members = self.members()?;
        if !matches!(members.restart, Restart::Idle) {
            return Ok(None);
        }
        let Some(generation) = members.serving else {
            return Ok(None);
        };
        let Some(member) = members.by_generation.get_mut(&generation) else {
            return Ok(None);
        };
        if member.process.exited()? {
            return Ok(None);
        }
        Ok(Some(read(member)))
    }

    /// Take a serving generation that stopped out of service and decide
    /// what follows it; report a restarted one that became ready.
    fn watch_serving(&self, now: Instant, events: &mut Vec<MemberEvent>) {
        let (generation, cause) = {
            let Ok(mut members) = self.members() else {
                return;
            };
            let loading = match members.restart {
                Restart::Idle => None,
                Restart::Loading { deadline, probe_at } => Some((deadline, probe_at)),
                Restart::Due { .. } | Restart::InHand { .. } | Restart::Failed { .. } => return,
            };
            let Some(generation) = members.serving else {
                return;
            };
            let Some(member) = members.by_generation.get_mut(&generation) else {
                return;
            };
            let contact = member.process.control();
            let cause = if matches!(member.process.exited(), Ok(true)) {
                Some(Cause::Exited)
            } else if contact == ControlContact::Failed {
                Some(Cause::ContactLost)
            } else if loading.is_some_and(|(deadline, _)| now >= deadline) {
                Some(Cause::NotReady)
            } else {
                None
            };
            match (cause, loading) {
                (Some(cause), _) => (generation, Ok(cause)),
                (None, Some((deadline, probe_at))) if now >= probe_at => {
                    let probe = (member.port, member.deployment_id.clone(), contact);
                    members.restart = Restart::Loading {
                        deadline,
                        probe_at: later(now, self.settings.status_poll_interval),
                    };
                    (generation, Err(probe))
                }
                _ => return,
            }
        };
        match cause {
            Ok(cause) => self.take_out_of_service(generation, &cause, now, events),
            Err((port, deployment_id, contact)) => {
                let ready = contact == ControlContact::Reporting
                    && matches!(
                        self.probe.ready_model(port, self.settings.status_poll_interval),
                        Ok(Some(model)) if model == deployment_id
                    );
                let Ok(mut members) = self.members() else {
                    return;
                };
                if ready
                    && members.serving == Some(generation)
                    && matches!(members.restart, Restart::Loading { .. })
                {
                    members.restart = Restart::Idle;
                    events.push(MemberEvent::Serving {
                        member: MemberRef::new(deployment_id, generation),
                    });
                }
            }
        }
    }

    fn take_out_of_service(
        &self,
        generation: u64,
        cause: &Cause,
        now: Instant,
        events: &mut Vec<MemberEvent>,
    ) {
        let mut member = {
            let Ok(mut members) = self.members() else {
                return;
            };
            if members.serving != Some(generation) {
                return;
            }
            let Some(member) = members.take_serving() else {
                return;
            };
            members.restart = Restart::InHand {
                deployment_id: member.deployment_id.clone(),
                port: member.port,
            };
            member
        };
        let name = format!(
            "serving worker `{}` generation {generation}",
            member.deployment_id
        );
        let error = match cause {
            Cause::Exited => member.process.startup_failure(),
            // A worker that answers no poll would not hear a request to drain.
            Cause::ContactLost => {
                member.process.kill();
                AgentError::WorkerControl(format!("{name} lost its control channel"))
            }
            Cause::NotReady => {
                member.process.kill();
                AgentError::WorkerControl(format!(
                    "{name} was restarted and was not ready within {} ms",
                    self.settings.restart_warm_timeout.as_millis()
                ))
            }
        }
        .to_record();
        let _ = fs::remove_file(&member.config_path);
        events.push(MemberEvent::Stopped {
            member: MemberRef::new(member.deployment_id.clone(), generation),
            error: error.clone(),
        });
        if let Ok(mut members) = self.members() {
            events.extend(owe_restart(
                &mut members,
                member.recipe,
                member.port,
                error,
                now,
            ));
        }
    }

    /// Read the staged manifest the restart of `recipe`, which is in hand,
    /// is rendered from. `None` when the bundle staged there is no longer
    /// the member's: the member is failed and nothing is started.
    fn staged_for_restart(
        &self,
        recipe: &CandidateRef,
        events: &mut Vec<MemberEvent>,
    ) -> Option<AgentResult<StagedManifest>> {
        #[cfg(test)]
        if let Some(hook) = self
            .before_staged_read
            .lock()
            .ok()
            .and_then(|mut h| h.take())
        {
            hook();
        }
        // Read outside the lock: the restart in hand keeps the member. A
        // manifest that cannot be read counts as a launch that failed.
        let staged = match crate::bundle::staged_manifest(Path::new(&recipe.staged_path)) {
            Ok(staged) => staged,
            Err(err) => return Some(Err(err)),
        };
        if crate::bundle::bundle_digests_equal(&staged.bundle_digest, &recipe.bundle_digest) {
            return Some(Ok(staged));
        }
        // A deploy of the same id that failed after staging left its own
        // bundle in the staged root. Another attempt would read the same
        // root, so the member is failed without one.
        let error = AgentError::BundleIntegrity {
            path: "manifest.json".into(),
            reason: format!(
                "`{}` is not restarted: the bundle staged at `{}` is `{}`, not the `{}` it was started from",
                recipe.deployment_id, recipe.staged_path, staged.bundle_digest, recipe.bundle_digest
            ),
        }
        .to_record();
        if let Ok(mut members) = self.members() {
            if members.restart_in_hand(&recipe.deployment_id) {
                members.restart = Restart::Failed {
                    deployment_id: recipe.deployment_id.clone(),
                    error: error.clone(),
                };
                events.push(MemberEvent::Failed {
                    deployment_id: recipe.deployment_id.clone(),
                    error,
                });
            }
        }
        None
    }

    /// Start the successor a stopped generation is owed, once its wait is
    /// over and no deploy holds the member.
    fn start_owed_restart(&self, now: Instant, events: &mut Vec<MemberEvent>) {
        let (recipe, port) = {
            let Ok(mut members) = self.members() else {
                return;
            };
            if members.held_by_a_deploy()
                || !matches!(members.restart, Restart::Due { due, .. } if now >= due)
            {
                return;
            }
            let Restart::Due { recipe, port, .. } = std::mem::take(&mut members.restart) else {
                return;
            };
            members.restart = Restart::InHand {
                deployment_id: recipe.deployment_id.clone(),
                port,
            };
            (recipe, port)
        };
        let Some(staged) = self.staged_for_restart(&recipe, events) else {
            return;
        };
        let launched =
            staged.and_then(|staged| self.launch(&recipe, port, staged.runner_profile.as_deref()));
        let mut yielded = None;
        if let Ok(mut members) = self.members() {
            let in_hand = members.restart_in_hand(&recipe.deployment_id);
            match launched {
                Ok((generation, member)) if in_hand && !members.held_by_a_deploy() => {
                    members.restarts.push_back(now);
                    members.admit(generation, member);
                    members.serving = Some(generation);
                    members.restart = Restart::Loading {
                        deadline: later(now, self.settings.restart_warm_timeout),
                        probe_at: now,
                    };
                    events.push(MemberEvent::Restarted {
                        member: MemberRef::new(recipe.deployment_id, generation),
                    });
                }
                // A deploy began, or the member was taken, while this
                // generation was starting: it yields and is not counted.
                Ok((_, member)) => {
                    if in_hand {
                        members.restart = Restart::Due {
                            recipe,
                            port,
                            due: now,
                        };
                    }
                    yielded = Some(member);
                }
                Err(err) if in_hand => {
                    members.restarts.push_back(now);
                    let error = err.to_record();
                    events.push(MemberEvent::LaunchFailed {
                        deployment_id: recipe.deployment_id.clone(),
                        error: error.clone(),
                    });
                    events.extend(owe_restart(&mut members, recipe, port, error, now));
                }
                Err(_) => {}
            }
        }
        self.retire(yielded.into_iter().collect());
    }
}

fn later(now: Instant, wait: Duration) -> Instant {
    now.checked_add(wait).unwrap_or(now)
}

/// Schedule the successor of a generation that stopped, or fail the member
/// when its restarts inside the window are used up. Nothing is owed once
/// the member was promoted over, unloaded or stopped meanwhile.
fn owe_restart(
    members: &mut Members,
    recipe: CandidateRef,
    port: u16,
    error: ErrorRecord,
    now: Instant,
) -> Option<MemberEvent> {
    let in_hand = matches!(
        &members.restart,
        Restart::InHand { deployment_id, .. } if *deployment_id == recipe.deployment_id
    );
    if !in_hand {
        return None;
    }
    while members
        .restarts
        .front()
        .is_some_and(|made| now.saturating_duration_since(*made) >= RESTART_WINDOW)
    {
        members.restarts.pop_front();
    }
    let deployment_id = recipe.deployment_id.clone();
    if let Some(&after) = RESTART_DELAYS.get(members.restarts.len()) {
        members.restart = Restart::Due {
            recipe,
            port,
            due: later(now, after),
        };
        return Some(MemberEvent::RestartDue {
            deployment_id,
            after,
        });
    }
    let error = ErrorRecord {
        message: format!(
            "`{deployment_id}` is not restarted: its {} restarts inside {} s are used up; it last stopped with: {}",
            RESTART_DELAYS.len(),
            RESTART_WINDOW.as_secs(),
            error.message
        ),
        ..error
    };
    members.restart = Restart::Failed {
        deployment_id: deployment_id.clone(),
        error: error.clone(),
    };
    Some(MemberEvent::Failed {
        deployment_id,
        error,
    })
}

impl WorkerControl for MemberRegistry {
    fn prepare(
        &self,
        _transaction_id: &str,
        candidate: &CandidateRef,
        _timeout: Duration,
    ) -> AgentResult<()> {
        if !is_valid_deployment_id(&candidate.deployment_id) {
            return Err(AgentError::Config(format!(
                "deployment id `{}` cannot name a member: a member name is 1 to 128 of ASCII letters, digits, `-`, `_` or `.`",
                candidate.deployment_id
            )));
        }
        let displaced = {
            let mut members = self.members()?;
            members.preparing += 1;
            members.take_candidate()
        };
        let _preparing = Preparing(self);
        self.retire(displaced.map(|(_, member)| member).into_iter().collect());

        let staged = crate::bundle::staged_manifest(Path::new(&candidate.staged_path))?;
        let port = self.candidate_port(&*self.members()?);
        let (generation, member) =
            self.launch(candidate, port, staged.runner_profile.as_deref())?;
        // The lock was released since the slot was emptied; whatever took it
        // in between is displaced as well, not forgotten.
        let (displaced, stopped) = {
            let mut members = self.members()?;
            if members.stopping {
                (Some(member), true)
            } else {
                let displaced = members.take_candidate();
                members.admit(generation, member);
                members.candidate = Some(generation);
                (displaced.map(|(_, member)| member), false)
            }
        };
        self.retire(displaced.into_iter().collect());
        if stopped {
            return Err(AgentError::Stopping);
        }
        Ok(())
    }

    fn warm(
        &self,
        _transaction_id: &str,
        candidate: &CandidateRef,
        timeout: Duration,
    ) -> AgentResult<WorkerReadiness> {
        let started = Instant::now();
        let deadline = started.checked_add(timeout).unwrap_or(started);
        let readiness = |ready| WorkerReadiness {
            deployment_id: candidate.deployment_id.clone(),
            ready,
        };
        let mut last_error = None;
        loop {
            let (port, contact) = self.candidate_progress(candidate)?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.abandon(candidate);
                return last_error.map_or_else(|| Ok(readiness(false)), Err);
            }
            match self.probe.ready_model(port, remaining) {
                Ok(model) => {
                    // Ready on `/health` is not enough: a member the agent
                    // cannot reach on its control channel is not under control.
                    if contact == ControlContact::Reporting
                        && model.as_deref() == Some(candidate.deployment_id.as_str())
                    {
                        return Ok(readiness(true));
                    }
                    last_error = None;
                }
                Err(err) => last_error = Some(err),
            }
            let sleep = self
                .settings
                .status_poll_interval
                .min(deadline.saturating_duration_since(Instant::now()));
            if !sleep.is_zero() {
                std::thread::sleep(sleep);
            }
        }
    }

    fn promote(&self, _transaction_id: &str, candidate: &CandidateRef) -> AgentResult<()> {
        let retired = {
            let mut members = self.members()?;
            if members.stopping {
                return Err(AgentError::Stopping);
            }
            if members.candidate_abandoned {
                return Err(AgentError::WorkerNotReady);
            }
            let Some((generation, mut prepared)) = members.take_candidate() else {
                return Err(AgentError::WorkerNotReady);
            };
            if prepared.deployment_id != candidate.deployment_id {
                drop(members);
                let err = AgentError::WorkerControl(format!(
                    "prepared candidate `{}` does not match promote request `{}`",
                    prepared.deployment_id, candidate.deployment_id
                ));
                self.retire(vec![prepared]);
                return Err(err);
            }
            // A candidate that exited after it was warmed replaces nothing:
            // the serving member stays.
            let exited = prepared.process.exited();
            if !matches!(exited, Ok(false)) {
                drop(members);
                let _ = fs::remove_file(&prepared.config_path);
                return Err(exited
                    .err()
                    .unwrap_or_else(|| prepared.process.startup_failure()));
            }
            let old = members.take_serving();
            members.admit(generation, prepared);
            members.serving = Some(generation);
            members.forget_restart();
            old
        };
        self.retire(retired.into_iter().collect());
        Ok(())
    }

    fn abandon(&self, candidate: &CandidateRef) {
        if let Ok(mut members) = self.members() {
            if members.names(members.candidate, &candidate.deployment_id) {
                members.candidate_abandoned = true;
            }
        }
    }

    fn unload(&self, deployment_id: &str) {
        let mut stopped = Vec::new();
        if let Ok(mut members) = self.members() {
            if members.names(members.serving, deployment_id) {
                stopped.extend(members.take_serving());
            }
            if members.names(members.candidate, deployment_id) {
                stopped.extend(members.take_candidate().map(|(_, member)| member));
            }
            if members.restart_names(deployment_id) {
                members.forget_restart();
            }
        }
        self.retire(stopped);
    }

    fn active_deployment_id(&self) -> AgentResult<Option<String>> {
        self.serving(|member| member.deployment_id.clone())
    }

    fn active_serving_url(&self) -> AgentResult<Option<String>> {
        self.serving(|member| format!("http://{}:{}/infer", self.settings.bind_host, member.port))
    }

    fn supervise(&self) -> Vec<MemberEvent> {
        let mut events = Vec::new();
        let now = self.clock.now();
        self.watch_serving(now, &mut events);
        self.start_owed_restart(now, &mut events);
        events
    }

    fn member_failure(&self) -> Option<ErrorRecord> {
        match &self.members().ok()?.restart {
            Restart::Failed { error, .. } => Some(error.clone()),
            _ => None,
        }
    }

    fn shutdown(&self) {
        let give_up = Instant::now() + self.settings.stop_deadline + self.settings.launch_grace;
        let (members, outside) = {
            // A poisoned lock still guards the map of what has to be stopped.
            let mut members = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            members.stopping = true;
            members.serving = None;
            members.candidate = None;
            (
                std::mem::take(&mut members.by_generation),
                members.outside.clone(),
            )
        };
        self.retire(members.into_values().collect());
        // What another thread was starting or stopping when the flag was
        // set: it ends stopped, by that thread, within the stop deadline.
        // A launch held in its generation write outlasts the wait, and
        // starts nothing when the write returns.
        loop {
            let left = outside.load(Ordering::SeqCst);
            if left == 0 {
                break;
            }
            if Instant::now() >= give_up {
                eprintln!(
                    "member registry stop: {left} worker launch or stop on another thread had not ended after {} ms; not waited for",
                    (self.settings.stop_deadline + self.settings.launch_grace).as_millis()
                );
                break;
            }
            std::thread::sleep(DRAIN_POLL);
        }
    }

    fn stopping(&self) -> bool {
        // A poisoned lock is not a stop; it is reported by what locks next.
        self.members().is_ok_and(|members| members.stopping)
    }
}

impl Drop for MemberRegistry {
    fn drop(&mut self) {
        let members = std::mem::take(
            self.inner
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        self.retire(members.by_generation.into_values().collect());
    }
}

const CONFIG_PREFIX: &str = "serving-";
const CONFIG_SUFFIX: &str = ".json";

/// Config names carry a generation and so are never written twice; the
/// ones an agent process did not live to remove are cleared by the next.
fn remove_stale_configs(config_dir: &Path) {
    let Ok(entries) = fs::read_dir(config_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(CONFIG_PREFIX) && name.ends_with(CONFIG_SUFFIX) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// A [`GenerationSource`] over the durable state's counter: the write that
/// advances it has committed before the generation is returned.
pub fn durable_generations(store: Arc<StateStore>) -> GenerationSource {
    Arc::new(move || {
        store.update(|state| {
            state
                .allocate_generation(1)
                .map_err(|e| AgentError::Internal(format!("allocate deployment generation: {e}")))
        })
    })
}

struct HttpHealth {
    host: String,
}

impl HealthProbe for HttpHealth {
    fn ready_model(&self, port: u16, timeout: Duration) -> AgentResult<Option<String>> {
        let health = match get_health_json(&self.host, port, timeout) {
            Ok(health) => health,
            // A worker that accepted the connection and has not answered yet
            // is not ready; only one that cannot be reached is an error.
            Err(AgentError::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                return Ok(None)
            }
            Err(err) => return Err(err),
        };
        if health.get("state").and_then(serde_json::Value::as_str) != Some("ready") {
            return Ok(None);
        }
        Ok(health
            .get("active_model_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned))
    }
}

struct SystemLauncher {
    binary_path: PathBuf,
    stderr_sink: WorkerStderrSink,
}

impl ProcessLauncher for SystemLauncher {
    fn launch(&self, spec: &LaunchSpec) -> AgentResult<Box<dyn MemberProcess>> {
        let mut command = Command::new(&self.binary_path);
        command
            .arg("--config")
            .arg(&spec.config_path)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let (mut child, stream) = spawn_with_control(command).map_err(|e| {
            AgentError::WorkerControl(format!("spawn {}: {e}", self.binary_path.display()))
        })?;
        let stderr = Arc::new(Mutex::new(WorkerStderr::default()));
        if let Some(pipe) = child.stderr.take() {
            forward_stderr(pipe, self.stderr_sink.clone(), stderr.clone());
        }
        // Polling starts here, before anything waits on the model load: a
        // worker gives up on a control channel nobody reads.
        match ControlChannel::start(stream, spec.member.clone()) {
            Ok(channel) => Ok(Box::new(SystemProcess {
                child,
                status: None,
                stderr,
                channel,
            })),
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(AgentError::WorkerControl(format!(
                    "open the control channel of `{}` generation {}: {err}",
                    spec.member.deployment_id, spec.member.generation
                )))
            }
        }
    }
}

struct SystemProcess {
    child: Child,
    status: Option<ExitStatus>,
    stderr: Arc<Mutex<WorkerStderr>>,
    channel: ControlChannel,
}

impl MemberProcess for SystemProcess {
    fn exited(&mut self) -> AgentResult<bool> {
        if self.status.is_none() {
            self.status = self.child.try_wait()?;
        }
        Ok(self.status.is_some())
    }

    fn startup_failure(&mut self) -> AgentError {
        match self.status {
            Some(status) => startup_error(status, &self.stderr),
            None => AgentError::WorkerExited("exit status unknown".into()),
        }
    }

    fn control(&self) -> ControlContact {
        let snapshot = self.channel.snapshot();
        match snapshot.contact {
            ContactState::Failed => ControlContact::Failed,
            ContactState::InContact
                if snapshot.ledger.is_some() && snapshot.last_error.is_none() =>
            {
                ControlContact::Reporting
            }
            ContactState::InContact | ContactState::OutOfContact => ControlContact::Pending,
        }
    }

    fn terminate(&mut self) {
        // Not reaped yet, so the pid still names this child.
        if self.status.is_none() {
            if let Err(err) = send_sigterm(self.child.id()) {
                eprintln!(
                    "serving worker pid {} could not be asked to stop: {err}",
                    self.child.id()
                );
            }
        }
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        if let Ok(status) = self.child.wait() {
            self.status = Some(status);
        }
    }
}

impl Drop for SystemProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests;
