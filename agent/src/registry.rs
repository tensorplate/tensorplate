// SPDX-License-Identifier: Apache-2.0

//! Generation-keyed registry of the serving workers this agent runs.
//!
//! Every worker process the agent starts is a member generation here: the
//! registry allocates its generation, renders its serving config, starts it
//! with its control socket, judges its readiness, and stops and reaps it.
//! The deploy coordinator and startup recovery reach it through
//! [`WorkerControl`].

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tensorplate_protocol::agent_control::is_valid_deployment_id;
use tensorplate_protocol::worker_control::{CandidateRef, MemberRef};

use crate::config::AgentConfig;
use crate::control_channel::{ContactState, ControlChannel};
use crate::error::{AgentError, AgentResult};
use crate::state::StateStore;
use crate::worker::{
    forward_stderr, get_health_json, spawn_with_control, startup_error, WorkerControl,
    WorkerReadiness, WorkerStderr, WorkerStderrSink,
};

/// How long a member asked to stop is given to drain before it is killed.
pub const STOP_DRAIN: Duration = Duration::from_secs(5);

const DRAIN_POLL: Duration = Duration::from_millis(10);

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

impl Members {
    fn take_candidate(&mut self) -> Option<(u64, Member)> {
        let generation = self.candidate.take()?;
        Some((generation, self.by_generation.remove(&generation)?))
    }

    fn take_serving(&mut self) -> Option<Member> {
        let generation = self.serving.take()?;
        self.by_generation.remove(&generation)
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
            stop_drain: STOP_DRAIN,
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

    /// The port a new candidate binds: the one the serving member is not on.
    fn candidate_port(&self, members: &Members) -> u16 {
        let serving = members
            .serving
            .and_then(|generation| members.by_generation.get(&generation));
        match serving.map(|member| member.port) {
            Some(port) if port != self.settings.candidate_port => self.settings.candidate_port,
            _ => self.settings.active_port,
        }
    }

    fn render_config(
        &self,
        candidate: &CandidateRef,
        generation: u64,
        port: u16,
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
        if let Some(profile) =
            crate::bundle::staged_runner_profile(Path::new(&candidate.staged_path))?
        {
            config["deployment"]["model"]["runner_profile"] = profile.into();
        }
        fs::write(&path, serde_json::to_vec_pretty(&config)?)?;
        Ok(path)
    }

    /// Stop `members`: ask each to drain, then kill whatever is still
    /// running when the one drain deadline they share passes.
    fn retire(&self, mut members: Vec<Member>) {
        for member in &mut members {
            member.process.terminate();
        }
        let deadline = Instant::now() + self.settings.stop_drain;
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

    /// Read the serving member, after forgetting one whose process exited.
    fn serving<T>(&self, read: impl FnOnce(&Member) -> T) -> AgentResult<Option<T>> {
        let mut members = self.members()?;
        let Some(generation) = members.serving else {
            return Ok(None);
        };
        let exited = match members.by_generation.get_mut(&generation) {
            Some(member) => member.process.exited()?,
            None => true,
        };
        if exited {
            if let Some(member) = members.take_serving() {
                let _ = fs::remove_file(member.config_path);
            }
            return Ok(None);
        }
        Ok(members.by_generation.get(&generation).map(read))
    }
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
        let displaced = self.members()?.take_candidate();
        self.retire(displaced.map(|(_, member)| member).into_iter().collect());

        let generation = (self.generations)()?;
        let port = self.candidate_port(&*self.members()?);
        let spec = LaunchSpec {
            member: MemberRef::new(candidate.deployment_id.clone(), generation),
            config_path: self.render_config(candidate, generation, port)?,
        };
        let process = match self.launcher.launch(&spec) {
            Ok(process) => process,
            Err(err) => {
                let _ = fs::remove_file(&spec.config_path);
                return Err(err);
            }
        };
        let mut members = self.members()?;
        members.by_generation.insert(
            generation,
            Member {
                deployment_id: spec.member.deployment_id,
                port,
                config_path: spec.config_path,
                process,
            },
        );
        members.candidate = Some(generation);
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
        let (retired, outcome) = {
            let mut members = self.members()?;
            let Some((generation, prepared)) = members.take_candidate() else {
                return Err(AgentError::WorkerNotReady);
            };
            if prepared.deployment_id == candidate.deployment_id {
                let old = members.take_serving();
                members.by_generation.insert(generation, prepared);
                members.serving = Some(generation);
                (old, Ok(()))
            } else {
                let err = AgentError::WorkerControl(format!(
                    "prepared candidate `{}` does not match promote request `{}`",
                    prepared.deployment_id, candidate.deployment_id
                ));
                (Some(prepared), Err(err))
            }
        };
        self.retire(retired.into_iter().collect());
        outcome
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
        }
        self.retire(stopped);
    }

    fn active_deployment_id(&self) -> AgentResult<Option<String>> {
        self.serving(|member| member.deployment_id.clone())
    }

    fn active_serving_url(&self) -> AgentResult<Option<String>> {
        self.serving(|member| format!("http://{}:{}/infer", self.settings.bind_host, member.port))
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
            None => AgentError::WorkerNotReady,
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
            request_stop(self.child.id());
        }
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        if let Ok(status) = self.child.wait() {
            self.status = Some(status);
        }
    }
}

/// Send SIGTERM with the shell's builtin: the standard library sends only
/// SIGKILL, the workspace denies `unsafe`, and a `kill` binary is not part
/// of every base system. A request that cannot be sent is reported; the
/// drain deadline still ends in a kill.
fn request_stop(pid: u32) {
    let sent = Command::new("/bin/sh")
        .args(["-c", "kill -TERM \"$0\"", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !matches!(&sent, Ok(status) if status.success()) {
        eprintln!("serving worker pid {pid} could not be asked to stop: {sent:?}");
    }
}

impl Drop for SystemProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests;
