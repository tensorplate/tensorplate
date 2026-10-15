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
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::{Duration, Instant};

use tensorplate_protocol::agent_control::is_valid_deployment_id;
use tensorplate_protocol::agent_state::ErrorRecord;
use tensorplate_protocol::worker_control::{
    CandidateRef, MemberRef, WorkerControlResponse, WorkerStatusOutcome,
};
use tensorplate_protocol::MemberQuota;

use crate::bundle::StagedManifest;
use crate::config::{AgentConfig, WorkerConfig};
use crate::control_channel::{
    ChannelSnapshot, ContactState, ControlChannel, RuntimeCall, RuntimeCommand,
};
use crate::descriptor_store::{self, DescriptorStore};
use crate::error::{AgentError, AgentResult, ControlChannelError};
use crate::state::StateStore;
use crate::supervision::{MonotonicClock, SystemMonotonicClock};
use crate::worker::{
    forward_stderr, get_health_json, send_sigterm, spawn_with_control, startup_error, LiveMember,
    MemberEvent, MemberRole, ServingMember, StreamTarget, WorkerControl, WorkerReadiness,
    WorkerStderr, WorkerStderrSink, STDERR_DRAIN_WAIT,
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

/// How long a member is given to answer an admission fence or an activate.
pub const CONTROL_BUDGET: Duration = Duration::from_secs(3);

/// What a member's control channel has shown so far.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlContact {
    /// In contact, with no ledger yet or an error in place of the last one.
    Pending,
    /// Three polls in a row went unanswered; contact is not given up yet.
    Unanswered,
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

    /// What sends the process's control channel a runtime operation, from
    /// outside the registry's lock.
    fn control_caller(&self) -> Box<dyn RuntimeCall>;

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

/// The loopback ports one member generation holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemberPorts {
    pub http: u16,
    /// Held for the member's stream listener; none when the pool is the two
    /// single ports of a configuration without a range.
    pub stream: Option<u16>,
}

/// The ports the agent's configuration gives its workers: the pairs of
/// `worker.serving_port_range`, or without one the two single ports.
#[must_use]
pub fn configured_ports(worker: &WorkerConfig) -> Vec<MemberPorts> {
    match worker.serving_port_range {
        Some(range) => range
            .pairs()
            .map(|(http, stream)| MemberPorts {
                http,
                stream: Some(stream),
            })
            .collect(),
        None => [worker.serving_bind_port, worker.serving_candidate_bind_port]
            .into_iter()
            .map(|http| MemberPorts { http, stream: None })
            .collect(),
    }
}

/// Which of the configured ports are held.
struct PortPool {
    ports: Vec<MemberPorts>,
    held: Mutex<Vec<bool>>,
}

impl PortPool {
    fn new(ports: Vec<MemberPorts>) -> Arc<Self> {
        Arc::new(Self {
            held: Mutex::new(vec![false; ports.len()]),
            ports,
        })
    }

    /// The first ports nothing holds.
    fn lease(self: &Arc<Self>) -> Option<Arc<PortLease>> {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let index = held.iter().position(|held| !held)?;
        held[index] = true;
        Some(Arc::new(PortLease {
            ports: self.ports[index],
            index,
            pool: self.clone(),
        }))
    }
}

/// Ports held for one member: by its generations, from each one's start
/// until its process has been reaped, and by the restart owed to it. They
/// go back to the pool when the last holder lets go.
struct PortLease {
    ports: MemberPorts,
    index: usize,
    pool: Arc<PortPool>,
}

impl Drop for PortLease {
    fn drop(&mut self) {
        let mut held = self
            .pool
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        held[self.index] = false;
    }
}

/// A generation the registry starts by itself, in place of one that stopped.
pub struct Restamp<'a> {
    /// What the stopped generation was started from.
    pub recipe: &'a CandidateRef,
    /// The generation `recipe` was last stamped for, or last started as.
    pub stopped_generation: u64,
    pub generation: u64,
}

/// Called before a restart renders the new generation's config, so that
/// whatever names the deployment's generation can be written for it.
/// Returns what the new generation is started from, and restarted from
/// after it; an error is a launch that failed. The runner profile was read
/// from the staged bundle before the call, so the stamp must keep the
/// recipe's staged path and bundle digest.
pub type RestartStamp = Arc<dyn Fn(&Restamp<'_>) -> AgentResult<CandidateRef> + Send + Sync>;

/// A launch that failed, and what a restart stamp had written by then.
struct FailedLaunch {
    error: AgentError,
    stamped: Option<Box<(u64, CandidateRef)>>,
    /// A restart that was not begun because a deploy holds the member.
    held_by_a_deploy: bool,
}

impl From<AgentError> for FailedLaunch {
    fn from(error: AgentError) -> Self {
        Self {
            error,
            stamped: None,
            held_by_a_deploy: false,
        }
    }
}

/// Hands out the next deployment generation, durably and never twice.
pub type GenerationSource = Arc<dyn Fn() -> AgentResult<u64> + Send + Sync>;

/// Where members listen and how the registry waits on them.
#[derive(Clone, Debug)]
pub struct RegistrySettings {
    pub bind_host: String,
    /// The ports members are given, in the order they are handed out.
    pub ports: Vec<MemberPorts>,
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
    generation: u64,
    /// Judged ready, by `warm` or by the supervision step after a restart.
    ready: bool,
    config_path: PathBuf,
    process: Box<dyn MemberProcess>,
    /// What this generation was started from, to start its successor.
    recipe: CandidateRef,
    /// Declared after `process`, as `outside` is: a member dropped while
    /// its process runs is killed and reaped before either is released.
    ports: Arc<PortLease>,
    /// Set while a thread holds the member outside the registry's map.
    outside: Option<Outside>,
    control: Arc<ControlState>,
}

/// What the agent's fence and activates left in force on one member,
/// shared with a thread that sends it one.
#[derive(Default)]
struct ControlState {
    /// The fence in force. Held while an operation is sent, so operations
    /// reach the member in the order they were decided.
    fenced_by: Mutex<Option<Fence>>,
    /// What the member's worker answered its last `activate` with, until
    /// one is answered `error`.
    quota: Mutex<Option<MemberQuota>>,
}

impl ControlState {
    /// Whether the activate a failed transaction owes the member is due at
    /// `now`; if so it is next due at `next`. An operation being sent is
    /// not waited for.
    fn reopen_due(&self, now: Instant, next: Instant) -> bool {
        let mut fenced_by = match self.fenced_by.try_lock() {
            Ok(fenced_by) => fenced_by,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return false,
        };
        match fenced_by.as_mut().map(|fence| &mut fence.reopen_at) {
            Some(Some(at)) if now >= *at => {
                *at = next;
                true
            }
            _ => false,
        }
    }
}

struct Fence {
    transaction_id: String,
    /// Set once the transaction failed and its activate was not answered
    /// `ok`: when the supervision step sends that activate again.
    reopen_at: Option<Instant>,
}

/// One member's control, taken out of the registry's lock to send it an
/// operation.
struct Reach {
    name: String,
    caller: Box<dyn RuntimeCall>,
    control: Arc<ControlState>,
}

impl Reach {
    /// `role` names the member in what is logged: `serving`, `candidate`
    /// or `restarted`.
    fn of(member: &Member, role: &str) -> Self {
        Self {
            name: format!(
                "{role} worker `{}` generation {}",
                member.deployment_id, member.generation
            ),
            caller: member.process.control_caller(),
            control: member.control.clone(),
        }
    }

    /// Recorded before it is sent: a transaction that fails re-activates a
    /// member whose answer was lost.
    fn fence(&self, transaction_id: &str) -> Result<WorkerControlResponse, ControlChannelError> {
        let mut fenced_by = lock(&self.control.fenced_by);
        *fenced_by = Some(Fence {
            transaction_id: transaction_id.to_owned(),
            reopen_at: None,
        });
        let command = RuntimeCommand::AdmissionFence {
            transaction_id: transaction_id.to_owned(),
        };
        self.caller.call(command, CONTROL_BUDGET)
    }

    fn activate(&self, transaction_id: &str, unless_fenced: bool) {
        let fenced_by = lock(&self.control.fenced_by);
        // The transaction that fenced it re-activates it or retires it.
        if unless_fenced && fenced_by.is_some() {
            return;
        }
        if let Err(why) = self.send_activate(transaction_id) {
            eprintln!("{}: activate {why}", self.name);
        }
    }

    /// Re-activate the member a transaction fenced and did not commit. One
    /// not answered `ok` stays fenced, owed the activate from `now` on.
    fn reopen(&self, now: Instant) {
        let mut fenced_by = lock(&self.control.fenced_by);
        let Some(fence) = fenced_by.as_mut() else {
            return;
        };
        let transaction_id = fence.transaction_id.clone();
        match self.send_activate(&transaction_id) {
            Ok(()) => *fenced_by = None,
            Err(why) => {
                if fence.reopen_at.replace(now).is_none() {
                    eprintln!(
                        "{}: activate after failed transaction `{transaction_id}` {why}; sent again until it is answered ok",
                        self.name
                    );
                }
            }
        }
    }

    /// Send again the activate a failed transaction owes the member, unless
    /// it was answered `ok` or another transaction fenced the member since.
    fn retry_reopen(&self) {
        let mut fenced_by = lock(&self.control.fenced_by);
        let owed = fenced_by.as_ref().filter(|fence| fence.reopen_at.is_some());
        let Some(transaction_id) = owed.map(|fence| fence.transaction_id.clone()) else {
            return;
        };
        if self.send_activate(&transaction_id).is_ok() {
            *fenced_by = None;
            eprintln!(
                "{}: activate after failed transaction `{transaction_id}` answered ok",
                self.name
            );
        }
    }

    /// Why an answer was not `ok`, for the caller to log: an activate
    /// follows a commit or a failure that it cannot undo.
    fn send_activate(&self, transaction_id: &str) -> Result<(), String> {
        let command = RuntimeCommand::Activate {
            transaction_id: transaction_id.to_owned(),
        };
        match self.caller.call(command, CONTROL_BUDGET) {
            Ok(answer) if answer.status == WorkerStatusOutcome::Ok => {
                *lock(&self.control.quota) = answer.quota;
                Ok(())
            }
            Ok(answer) => {
                // A quota the worker no longer admits by is worse than none.
                if answer.status == WorkerStatusOutcome::Error {
                    *lock(&self.control.quota) = None;
                }
                Err(format!("refused: {}", refusal(&answer)))
            }
            Err(err) => Err(format!("not answered: {err}")),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn refusal(answer: &WorkerControlResponse) -> String {
    match &answer.error {
        Some(error) => format!("{}: {}", error.code, error.message),
        None => format!("{:?}", answer.status),
    }
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

mod leaving {
    use super::{Arc, BTreeMap, Member, MemberPorts, Mutex, PoisonError};

    /// What the registry keeps of a member that left its map to be stopped.
    #[derive(Clone)]
    pub(super) struct Draining {
        pub(super) deployment_id: String,
        pub(super) ready: bool,
        pub(super) ports: MemberPorts,
    }

    /// The generations asked to stop whose process has not been dropped.
    #[derive(Default)]
    pub(super) struct DrainingSet(Arc<Mutex<BTreeMap<u64, Draining>>>);

    /// A member on its way to be stopped: `retire` takes nothing else, and
    /// one is only made by recording the member as draining.
    pub(super) struct Stopping {
        member: Member,
        /// Declared after `member`: the record goes once the member has
        /// been dropped, which reaps a process that still runs.
        _listed: Listed,
    }

    struct Listed {
        generation: u64,
        set: Arc<Mutex<BTreeMap<u64, Draining>>>,
    }

    impl Drop for Listed {
        fn drop(&mut self) {
            let mut set = self.set.lock().unwrap_or_else(PoisonError::into_inner);
            set.remove(&self.generation);
        }
    }

    impl DrainingSet {
        pub(super) fn record(&self, member: Member) -> Stopping {
            let mut set = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            set.insert(
                member.generation,
                Draining {
                    deployment_id: member.deployment_id.clone(),
                    ready: member.ready,
                    ports: member.ports.ports,
                },
            );
            Stopping {
                _listed: Listed {
                    generation: member.generation,
                    set: self.0.clone(),
                },
                member,
            }
        }

        pub(super) fn newest(&self) -> Option<u64> {
            let set = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            set.keys().next_back().copied()
        }

        /// Oldest generation first.
        pub(super) fn listed(&self) -> Vec<(u64, Draining)> {
            let set = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            set.iter()
                .map(|(generation, draining)| (*generation, draining.clone()))
                .collect()
        }
    }

    impl Stopping {
        pub(super) fn member(&mut self) -> &mut Member {
            &mut self.member
        }
    }
}

use leaving::{DrainingSet, Stopping};

#[derive(Default)]
struct Members {
    by_generation: BTreeMap<u64, Member>,
    draining: DrainingSet,
    serving: Option<u64>,
    candidate: Option<u64>,
    /// The candidate's deploy gave up on it; it is not promoted any more.
    candidate_abandoned: bool,
    /// Calls to `prepare` that have not returned, and holds not released.
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
    Due { owed: Box<Owed>, due: Instant },
    /// The supervision step is working on the member outside the lock.
    InHand { deployment_id: String },
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

/// What the successor of a stopped generation is started from.
struct Owed {
    recipe: CandidateRef,
    /// The stopped generation's ports, kept for its successor.
    ports: Arc<PortLease>,
    /// The generation `recipe` was last stamped for, or last started as.
    stamped: u64,
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
    ports: Arc<PortPool>,
    restart_stamp: Option<RestartStamp>,
    descriptors: Option<Arc<DescriptorStore>>,
    inner: Mutex<Members>,
    /// Runs once, just before a restart reads the staged manifest.
    #[cfg(test)]
    before_staged_read: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// The threads the supervision step sent an activate from.
    #[cfg(test)]
    activations: Mutex<Vec<std::thread::JoinHandle<()>>>,
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
            ports: PortPool::new(settings.ports.clone()),
            restart_stamp: None,
            descriptors: None,
            settings,
            launcher,
            probe,
            generations,
            clock: Arc::new(SystemMonotonicClock),
            inner: Mutex::new(Members::default()),
            #[cfg(test)]
            before_staged_read: Mutex::new(None),
            #[cfg(test)]
            activations: Mutex::default(),
        }
    }

    /// Call `stamp` for every generation a restart starts.
    #[must_use]
    pub fn with_restart_stamp(mut self, stamp: RestartStamp) -> Self {
        self.restart_stamp = Some(stamp);
        self
    }

    /// Read the descriptor a recipe names from `descriptors`.
    #[must_use]
    pub fn with_descriptor_store(mut self, descriptors: Arc<DescriptorStore>) -> Self {
        self.descriptors = Some(descriptors);
        self
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

    /// Whether a deploy holds the member: it has taken a hold, is preparing
    /// a candidate, or has one it may still promote. No restart starts
    /// meanwhile, so a generation allocated for the candidate, or taken
    /// under the hold and handed over, is newer than the one it replaces.
    fn held_by_a_deploy(&self) -> bool {
        self.preparing > 0 || (self.candidate.is_some() && !self.candidate_abandoned)
    }

    /// Whether a launch may start `deployment_id` as `generation`: one
    /// `handed` to the registry must be above every generation it holds, a
    /// member's, one that is draining or the one a restart is owed for; an
    /// allocated one must not be running.
    fn admissible(&self, deployment_id: &str, generation: u64, handed: bool) -> AgentResult<()> {
        if !handed {
            if self.by_generation.contains_key(&generation) {
                return Err(AgentError::Internal(format!(
                    "the generation source gave `{deployment_id}` generation {generation}, which is running"
                )));
            }
            return Ok(());
        }
        let owed = match &self.restart {
            Restart::Due { owed, .. } => Some(owed.stamped),
            _ => None,
        };
        let live = self.by_generation.keys().next_back().copied();
        let newest = live.max(owed).max(self.draining.newest());
        // A generation taken under a hold is above them all; one that is
        // not was handed twice, and another attempt takes a new one.
        match newest.filter(|held| generation <= *held) {
            Some(held) => Err(AgentError::Internal(format!(
                "`{deployment_id}` generation {generation} is not started: the registry holds generation {held}, and a generation handed to it must be above every one it holds"
            ))),
            None => Ok(()),
        }
    }

    fn restart_names(&self, deployment_id: &str) -> bool {
        match &self.restart {
            Restart::Due { owed, .. } => owed.recipe.deployment_id == deployment_id,
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
                Restart::InHand { deployment_id: owed } if owed == deployment_id
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

    /// Record `member`, which left the map under this lock, as draining.
    fn stopping(&self, member: Member) -> Stopping {
        self.draining.record(member)
    }

    fn live(&mut self, generation: u64, role: MemberRole) -> Option<LiveMember> {
        let member = self.by_generation.get_mut(&generation)?;
        let contact = member.process.control();
        Some(LiveMember {
            member: MemberRef::new(member.deployment_id.clone(), generation),
            role,
            // A process that cannot be inspected has not been seen to exit.
            running: !member.process.exited().unwrap_or(false),
            ready: member.ready,
            in_contact: Some(matches!(
                contact,
                ControlContact::Pending | ControlContact::Reporting
            )),
            http_port: member.ports.ports.http,
            stream_port: member.ports.ports.stream,
            effective_quota: *lock(&member.control.quota),
        })
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
            ports: configured_ports(&agent.worker),
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

    /// Ports for a new candidate: the first nothing holds. A worker that is
    /// still draining holds its ports, so this waits, until `timeout`, for
    /// one to be reaped.
    fn lease_ports(
        &self,
        candidate: &CandidateRef,
        timeout: Duration,
    ) -> AgentResult<Arc<PortLease>> {
        let deadline = later(Instant::now(), timeout);
        loop {
            if let Some(lease) = self.ports.lease() {
                return Ok(lease);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(AgentError::NoFreePort(format!(
                    "`{}` was not started: the configuration gives {} workers ports, and each is held by a worker that is running or draining, or kept for a restart",
                    candidate.deployment_id,
                    self.ports.ports.len()
                )));
            }
            std::thread::sleep(DRAIN_POLL.min(remaining));
        }
    }

    /// Start the next generation of `recipe` on `ports`. Refused once the
    /// registry is stopping; a launch that began before is waited for.
    /// `restart_of` is the generation a restart replaces. A deploy's recipe
    /// that names a descriptor is started as the generation it names, which
    /// must be above every generation the registry holds.
    fn launch(
        &self,
        recipe: &CandidateRef,
        ports: &Arc<PortLease>,
        runner_profile: Option<&str>,
        restart_of: Option<u64>,
    ) -> Result<(u64, Member), FailedLaunch> {
        // A restart's recipe still names the generation that stopped.
        let handed = descriptor_store::identity_of(recipe)?.filter(|_| restart_of.is_none());
        let outside = {
            let members = self.members()?;
            if members.stopping {
                return Err(AgentError::Stopping.into());
            }
            Outside::new(&members.outside)
        };
        let generation = match handed {
            Some(named) => named.generation,
            None => (self.generations)()?,
        };
        // A stop gives up waiting on a write that hangs; once the write
        // returns, nothing is started for it.
        {
            let members = self.members()?;
            if members.stopping {
                return Err(AgentError::Stopping.into());
            }
            members.admissible(&recipe.deployment_id, generation, handed.is_some())?;
            // Unloaded, promoted over or taken hold of by a deploy while the
            // staged manifest was read: nothing is stamped or started for it.
            if restart_of.is_some() {
                let held_by_a_deploy = members.held_by_a_deploy();
                if held_by_a_deploy || !members.restart_in_hand(&recipe.deployment_id) {
                    return Err(FailedLaunch {
                        held_by_a_deploy,
                        ..AgentError::WorkerNotReady.into()
                    });
                }
            }
        }
        let (recipe, stamped) = match (restart_of, &self.restart_stamp) {
            (Some(stopped_generation), Some(stamp)) => {
                let stamped = stamp(&Restamp {
                    recipe,
                    stopped_generation,
                    generation,
                })?;
                if stamped.deployment_id != recipe.deployment_id {
                    return Err(AgentError::Internal(format!(
                        "the restart of `{}` was stamped as `{}`",
                        recipe.deployment_id, stamped.deployment_id
                    ))
                    .into());
                }
                (stamped.clone(), Some(Box::new((generation, stamped))))
            }
            _ => (recipe.clone(), None),
        };
        // A stop that began while the stamp ran starts nothing.
        let stopped = match self.members() {
            Ok(members) => members.stopping.then_some(AgentError::Stopping),
            Err(poisoned) => Some(poisoned),
        };
        if let Some(error) = stopped {
            return Err(FailedLaunch {
                error,
                stamped,
                held_by_a_deploy: false,
            });
        }
        let rendered = self.render_config(&recipe, generation, ports.ports, runner_profile);
        let launched = rendered.and_then(|config_path| {
            let spec = LaunchSpec {
                member: MemberRef::new(recipe.deployment_id.clone(), generation),
                config_path,
            };
            self.launcher
                .launch(&spec)
                .map(|process| (spec.clone(), process))
                .map_err(|err| {
                    let _ = fs::remove_file(&spec.config_path);
                    err
                })
        });
        match launched {
            Ok((spec, process)) => Ok((
                generation,
                Member {
                    deployment_id: spec.member.deployment_id,
                    generation,
                    ready: false,
                    ports: ports.clone(),
                    config_path: spec.config_path,
                    process,
                    recipe,
                    outside: Some(outside),
                    control: Arc::default(),
                },
            )),
            Err(error) => Err(FailedLaunch {
                error,
                stamped,
                held_by_a_deploy: false,
            }),
        }
    }

    fn render_config(
        &self,
        candidate: &CandidateRef,
        generation: u64,
        ports: MemberPorts,
        runner_profile: Option<&str>,
    ) -> AgentResult<PathBuf> {
        let descriptor =
            descriptor_store::resolve(self.descriptors.as_deref(), candidate, generation)?;
        let runner_profile = descriptor.as_ref().map_or(runner_profile, |described| {
            let profile = described.configuration.runner_profile.as_ref();
            profile.map(|profile| profile.id.as_str())
        });
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
                "port": ports.http,
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
        if candidate.streaming {
            let Some(port) = ports.stream else {
                return Err(AgentError::Config(format!(
                    "`{}` serves stream sessions, and the agent's configuration holds no stream port for its worker: `worker.serving_port_range` gives one to each",
                    candidate.deployment_id
                )));
            };
            config["streaming"] = serde_json::json!({"enabled": true, "port": port});
            config["deployment"]["descriptor_digest"] = stream_digest(candidate).into();
        }
        fs::write(&path, serde_json::to_vec_pretty(&config)?)?;
        Ok(path)
    }

    /// Stop `members`: ask each to drain, then kill whatever is still
    /// running when the one stop deadline they share passes.
    fn retire(&self, mut members: Vec<Stopping>) {
        let deadline = self.ask_to_stop(&mut members);
        reap(members, deadline);
    }

    /// Ask each of `members` to drain; returns the stop deadline they share.
    fn ask_to_stop(&self, members: &mut [Stopping]) -> Instant {
        for stopping in members.iter_mut() {
            stopping.member().process.terminate();
        }
        Instant::now() + self.settings.stop_deadline
    }

    /// The prepared candidate's port and control contact. A candidate that
    /// exited, lost its control channel or cannot be inspected is removed
    /// and answered as the failure it is.
    fn candidate_progress(
        &self,
        candidate: &CandidateRef,
    ) -> AgentResult<(u64, u16, ControlContact)> {
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
            let exited = member.process.exited();
            let contact = member.process.control();
            if matches!(exited, Ok(false)) && contact != ControlContact::Failed {
                return Ok((member.generation, member.ports.ports.http, contact));
            }
            let Some((generation, member)) = members.take_candidate() else {
                return Err(AgentError::WorkerNotReady);
            };
            (generation, member, exited)
        };
        let _ = fs::remove_file(&member.config_path);
        match exited {
            Ok(true) => return Err(member.process.startup_failure()),
            // Not left running where nothing would look at it again.
            Err(err) => {
                member.process.kill();
                return Err(err);
            }
            Ok(false) => {}
        }
        // A worker that stopped answering its control channel is wedged; a
        // request to drain would not reach it either.
        member.process.kill();
        Err(AgentError::WorkerControl(format!(
            "serving worker `{}` generation {generation} lost its control channel before it served",
            member.deployment_id
        )))
    }

    /// Close the serving member's admission before the commit: it must
    /// answer `ok` within [`CONTROL_BUDGET`]. A member that exited or whose
    /// control channel failed admits nothing and is not fenced.
    fn fence_serving(&self, transaction_id: &str) -> AgentResult<()> {
        let Some(reach) = self.admitting_serving()? else {
            return Ok(());
        };
        let refused = match reach.fence(transaction_id) {
            Ok(answer) if answer.status == WorkerStatusOutcome::Ok => return Ok(()),
            Ok(answer) => AgentError::WorkerControl(format!(
                "{} refused the admission fence: {}",
                reach.name,
                refusal(&answer)
            )),
            Err(ControlChannelError::Deadline) => AgentError::WorkerTimeout(
                u64::try_from(CONTROL_BUDGET.as_millis()).unwrap_or(u64::MAX),
            ),
            Err(err) => {
                AgentError::WorkerControl(format!("{}: admission fence: {err}", reach.name))
            }
        };
        // One that exited or lost its channel meanwhile admits nothing either.
        if self.admitting_serving()?.is_some() {
            return Err(refused);
        }
        Ok(())
    }

    /// The serving member, unless it exited or its control channel failed.
    /// One that cannot be inspected may still admit. Refused once the
    /// registry stops, so no commit follows a fence a stop cut short.
    fn admitting_serving(&self) -> AgentResult<Option<Reach>> {
        let mut members = self.members()?;
        if members.stopping {
            return Err(AgentError::Stopping);
        }
        let Some(generation) = members.serving else {
            return Ok(None);
        };
        let Some(member) = members.by_generation.get_mut(&generation) else {
            return Ok(None);
        };
        if matches!(member.process.exited(), Ok(true))
            || member.process.control() == ControlContact::Failed
        {
            return Ok(None);
        }
        Ok(Some(Reach::of(member, "serving")))
    }

    /// Keep the judgement that `generation` is ready, for as long as the
    /// registry holds it.
    fn judged_ready(&self, generation: u64) -> AgentResult<()> {
        if let Some(member) = self.members()?.by_generation.get_mut(&generation) {
            member.ready = true;
        }
        Ok(())
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
                    let probe = (
                        member.ports.ports.http,
                        member.deployment_id.clone(),
                        contact,
                    );
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
                let activated = {
                    let Ok(mut members) = self.members() else {
                        return;
                    };
                    if !ready
                        || members.serving != Some(generation)
                        || !matches!(members.restart, Restart::Loading { .. })
                    {
                        return;
                    }
                    members.restart = Restart::Idle;
                    events.push(MemberEvent::Serving {
                        member: MemberRef::new(deployment_id, generation),
                    });
                    members.by_generation.get_mut(&generation).map(|member| {
                        member.ready = true;
                        Reach::of(member, "restarted")
                    })
                };
                if let Some(reach) = activated {
                    self.activate_restarted(reach);
                }
            }
        }
    }

    fn activate_restarted(&self, reach: Reach) {
        let transaction_id = format!("tx-{}", uuid::Uuid::new_v4());
        self.activate_on_thread(reach, move |reach| reach.activate(&transaction_id, true));
    }

    /// Send the serving member again the activate a failed transaction
    /// owes it, at most once a poll interval.
    fn retry_reopen(&self, now: Instant) {
        let reach = {
            let Ok(members) = self.members() else {
                return;
            };
            let serving = members.serving;
            let serving = serving.and_then(|generation| members.by_generation.get(&generation));
            let Some(member) = serving else {
                return;
            };
            let next = later(now, self.settings.status_poll_interval);
            if !member.control.reopen_due(now, next) {
                return;
            }
            Reach::of(member, "serving")
        };
        self.activate_on_thread(reach, Reach::retry_reopen);
    }

    /// An activate from the supervision step, on a thread of its own: the
    /// step does not wait for the answer.
    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn activate_on_thread(&self, reach: Reach, send: impl FnOnce(&Reach) + Send + 'static) {
        let name = reach.name.clone();
        let spawned = std::thread::Builder::new()
            .name("member-activate".into())
            .spawn(move || send(&reach));
        match spawned {
            #[cfg(test)]
            Ok(thread) => lock(&self.activations).push(thread),
            #[cfg(not(test))]
            Ok(_) => {}
            Err(err) => eprintln!("{name}: activate not sent: {err}"),
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
            let owed = Owed {
                recipe: member.recipe.clone(),
                ports: member.ports.clone(),
                stamped: generation,
            };
            events.extend(owe_restart(&mut members, owed, error, now));
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
        // Another bundle is in the tree: one an earlier agent staged every
        // deploy of the id into, or one changed on disk. Another attempt
        // would read the same tree, so the member is failed without one.
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
        let mut owed = {
            let Ok(mut members) = self.members() else {
                return;
            };
            if members.held_by_a_deploy()
                || !matches!(members.restart, Restart::Due { due, .. } if now >= due)
            {
                return;
            }
            let Restart::Due { owed, .. } = std::mem::take(&mut members.restart) else {
                return;
            };
            members.restart = Restart::InHand {
                deployment_id: owed.recipe.deployment_id.clone(),
            };
            *owed
        };
        let Some(staged) = self.staged_for_restart(&owed.recipe, events) else {
            return;
        };
        let launched = staged.map_err(FailedLaunch::from).and_then(|staged| {
            let profile = staged.runner_profile.as_deref();
            self.launch(&owed.recipe, &owed.ports, profile, Some(owed.stamped))
        });
        // What the restart stamp wrote stands, whatever became of the launch.
        match &launched {
            Ok((generation, member)) => {
                owed.stamped = *generation;
                owed.recipe = member.recipe.clone();
            }
            Err(FailedLaunch {
                stamped: Some(stamped),
                ..
            }) => {
                owed.stamped = stamped.0;
                owed.recipe = stamped.1.clone();
            }
            Err(_) => {}
        }
        let deployment_id = owed.recipe.deployment_id.clone();
        let mut yielded = None;
        if let Ok(mut members) = self.members() {
            let in_hand = members.restart_in_hand(&deployment_id);
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
                        member: MemberRef::new(deployment_id, generation),
                    });
                }
                // A deploy began, or the member was taken, while this
                // generation was starting: it yields and is not counted.
                Ok((_, member)) => {
                    if in_hand {
                        members.restart = Restart::Due {
                            owed: Box::new(owed),
                            due: now,
                        };
                    }
                    yielded = Some(members.stopping(member));
                }
                Err(failed) if in_hand && failed.held_by_a_deploy => {
                    members.restart = Restart::Due {
                        owed: Box::new(owed),
                        due: now,
                    };
                }
                Err(failed) if in_hand => {
                    members.restarts.push_back(now);
                    let error = failed.error.to_record();
                    events.push(MemberEvent::LaunchFailed {
                        deployment_id,
                        error: error.clone(),
                    });
                    events.extend(owe_restart(&mut members, owed, error, now));
                }
                Err(_) => {}
            }
        }
        self.retire(yielded.into_iter().collect());
    }
}

/// Wait for `members` to exit, killing whatever still runs at `deadline`.
/// Each is dropped once reaped, which is when its ports and its place among
/// the draining members are given up.
fn reap(members: Vec<Stopping>, deadline: Instant) {
    for mut stopping in members {
        let member = stopping.member();
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

fn later(now: Instant, wait: Duration) -> Instant {
    now.checked_add(wait).unwrap_or(now)
}

/// Schedule the successor of a generation that stopped, or fail the member
/// when its restarts inside the window are used up. Nothing is owed once
/// the member was promoted over, unloaded or stopped meanwhile.
fn owe_restart(
    members: &mut Members,
    owed: Owed,
    error: ErrorRecord,
    now: Instant,
) -> Option<MemberEvent> {
    let in_hand = matches!(
        &members.restart,
        Restart::InHand { deployment_id } if *deployment_id == owed.recipe.deployment_id
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
    let deployment_id = owed.recipe.deployment_id.clone();
    if let Some(&after) = RESTART_DELAYS.get(members.restarts.len()) {
        members.restart = Restart::Due {
            owed: Box::new(owed),
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
        timeout: Duration,
    ) -> AgentResult<()> {
        if !is_valid_deployment_id(&candidate.deployment_id) {
            return Err(AgentError::Config(format!(
                "deployment id `{}` cannot name a member: a member name is 1 to 128 of ASCII letters, digits, `-`, `_` or `.`",
                candidate.deployment_id
            )));
        }
        let described = descriptor_store::identity_of(candidate)?.is_some();
        let displaced = {
            let mut members = self.members()?;
            members.preparing += 1;
            let displaced = members.take_candidate();
            displaced.map(|(_, member)| members.stopping(member))
        };
        let _preparing = Preparing(self);
        self.retire(displaced.into_iter().collect());

        let staged_profile = if described {
            None
        } else {
            crate::bundle::staged_manifest(Path::new(&candidate.staged_path))?.runner_profile
        };
        let ports = self.lease_ports(candidate, timeout)?;
        let (generation, member) = self
            .launch(candidate, &ports, staged_profile.as_deref(), None)
            .map_err(|failed| failed.error)?;
        // The lock was released since the slot was emptied; whatever took it
        // in between is displaced as well, not forgotten.
        let (displaced, stopped) = {
            let mut members = self.members()?;
            if members.stopping {
                (Some(members.stopping(member)), true)
            } else {
                let displaced = members.take_candidate();
                members.admit(generation, member);
                members.candidate = Some(generation);
                let displaced = displaced.map(|(_, member)| members.stopping(member));
                (displaced, false)
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
            let (generation, port, contact) = self.candidate_progress(candidate)?;
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
                        self.judged_ready(generation)?;
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

    fn fence(&self, transaction_id: &str, candidate: &CandidateRef) -> AgentResult<()> {
        {
            let mut members = self.members()?;
            if members.stopping {
                return Err(AgentError::Stopping);
            }
            if members.candidate_abandoned {
                return Err(AgentError::WorkerNotReady);
            }
            let Some(prepared) = members
                .candidate
                .and_then(|generation| members.by_generation.get(&generation))
            else {
                return Err(AgentError::WorkerNotReady);
            };
            if prepared.deployment_id != candidate.deployment_id {
                let err = AgentError::WorkerControl(format!(
                    "prepared candidate `{}` does not match promote request `{}`",
                    prepared.deployment_id, candidate.deployment_id
                ));
                let prepared = members.take_candidate().map(|(_, m)| members.stopping(m));
                drop(members);
                self.retire(prepared.into_iter().collect());
                return Err(err);
            }
        }
        // A candidate that exited or lost its control channel since it was
        // warmed replaces nothing.
        self.candidate_progress(candidate)?;
        self.fence_serving(transaction_id)
    }

    fn activate(&self, transaction_id: &str, candidate: &CandidateRef) -> AgentResult<()> {
        let (retired, activated) = {
            let mut members = self.members()?;
            if members.stopping {
                return Err(AgentError::Stopping);
            }
            if !members.names(members.candidate, &candidate.deployment_id) {
                return Err(AgentError::WorkerNotReady);
            }
            let Some((generation, prepared)) = members.take_candidate() else {
                return Err(AgentError::WorkerNotReady);
            };
            // One that exited since the fence is found by the next
            // supervision step, as a serving member that stopped.
            let old = members.take_serving();
            members.admit(generation, prepared);
            members.serving = Some(generation);
            members.forget_restart();
            let activated = members.by_generation.get(&generation);
            let activated = activated.map(|member| Reach::of(member, "candidate"));
            (old.map(|member| members.stopping(member)), activated)
        };
        let mut retired: Vec<Stopping> = retired.into_iter().collect();
        // Asked before the activate is sent: a stop of the agent, which waits
        // for this member only as long as for any other, never waits on it.
        let deadline = self.ask_to_stop(&mut retired);
        if let Some(reach) = activated {
            reach.activate(transaction_id, false);
        }
        reap(retired, deadline);
        Ok(())
    }

    fn owe_restore(&self, candidate: &CandidateRef, error: ErrorRecord) -> bool {
        let now = self.clock.now();
        let restored = {
            let Ok(mut members) = self.members() else {
                return false;
            };
            if members.stopping || members.serving.is_some() || members.held_by_a_deploy() {
                return false;
            }
            if !matches!(members.restart, Restart::Idle) {
                return false;
            }
            let held = if members.names(members.candidate, &candidate.deployment_id) {
                members.take_candidate()
            } else {
                None
            };
            let owed = match &held {
                Some((generation, member)) => Owed {
                    recipe: member.recipe.clone(),
                    ports: member.ports.clone(),
                    stamped: *generation,
                },
                None => {
                    let Some(ports) = self.ports.lease() else {
                        return false;
                    };
                    let named = descriptor_store::identity_of(candidate).ok().flatten();
                    Owed {
                        recipe: candidate.clone(),
                        ports,
                        stamped: named.map_or(0, |named| named.generation),
                    }
                }
            };
            members.restart = Restart::InHand {
                deployment_id: candidate.deployment_id.clone(),
            };
            let _ = owe_restart(&mut members, owed, error, now);
            held
        };
        // Held only after its warm timed out or its process could not be
        // inspected: it would not hear a request to drain.
        if let Some((_, mut member)) = restored {
            member.process.kill();
            let _ = fs::remove_file(&member.config_path);
        }
        true
    }

    fn abandon(&self, candidate: &CandidateRef) {
        let serving = {
            let Ok(mut members) = self.members() else {
                return;
            };
            if members.names(members.candidate, &candidate.deployment_id) {
                members.candidate_abandoned = true;
            }
            let serving = members.serving;
            serving
                .and_then(|generation| members.by_generation.get(&generation))
                .map(|member| Reach::of(member, "serving"))
        };
        if let Some(reach) = serving {
            reach.reopen(self.clock.now());
        }
    }

    fn hold(&self) {
        if let Ok(mut members) = self.members() {
            members.preparing += 1;
        }
    }

    fn release(&self) {
        if let Ok(mut members) = self.members() {
            members.preparing = members.preparing.saturating_sub(1);
        }
    }

    fn unload(&self, deployment_id: &str) {
        let mut stopped = Vec::new();
        if let Ok(mut members) = self.members() {
            if members.names(members.serving, deployment_id) {
                let serving = members.take_serving();
                stopped.extend(serving.map(|member| members.stopping(member)));
            }
            if members.names(members.candidate, deployment_id) {
                let candidate = members.take_candidate();
                stopped.extend(candidate.map(|(_, member)| members.stopping(member)));
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

    fn serving_member(&self) -> AgentResult<Option<ServingMember>> {
        self.serving(|member| ServingMember {
            member: MemberRef::new(member.deployment_id.clone(), member.generation),
            bundle_digest: member.recipe.bundle_digest.clone(),
            descriptor_digest: member.recipe.descriptor_digest.clone(),
            configuration_digest: member.recipe.configuration_digest.clone(),
            serving_url: format!(
                "http://{}:{}/infer",
                self.settings.bind_host, member.ports.ports.http
            ),
            stream: member
                .ports
                .ports
                .stream
                .filter(|_| member.recipe.streaming)
                .map(|port| StreamTarget {
                    endpoint: host_port(&self.settings.bind_host, port),
                    descriptor_digest: stream_digest(&member.recipe).to_owned(),
                }),
        })
    }

    fn live_members(&self) -> Option<Vec<LiveMember>> {
        let mut members = self.members().ok()?;
        let prepared = if members.candidate_abandoned {
            MemberRole::Abandoned
        } else {
            MemberRole::Candidate
        };
        let slots = [
            (members.serving, MemberRole::Serving),
            (members.candidate, prepared),
        ];
        let mut live: Vec<LiveMember> = slots
            .into_iter()
            .filter_map(|(slot, role)| members.live(slot?, role))
            .collect();
        live.extend(
            members
                .draining
                .listed()
                .into_iter()
                .map(|(generation, draining)| LiveMember {
                    member: MemberRef::new(draining.deployment_id, generation),
                    role: MemberRole::Draining,
                    running: true,
                    ready: draining.ready,
                    in_contact: None,
                    http_port: draining.ports.http,
                    stream_port: draining.ports.stream,
                    effective_quota: None,
                }),
        );
        Some(live)
    }

    fn supervise(&self) -> Vec<MemberEvent> {
        let mut events = Vec::new();
        let now = self.clock.now();
        self.watch_serving(now, &mut events);
        self.retry_reopen(now);
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
            let stopped: Vec<Stopping> = std::mem::take(&mut members.by_generation)
                .into_values()
                .map(|member| members.stopping(member))
                .collect();
            (stopped, members.outside.clone())
        };
        self.retire(members);
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
        let mut members = std::mem::take(
            self.inner
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let stopped = std::mem::take(&mut members.by_generation)
            .into_values()
            .map(|member| members.stopping(member))
            .collect();
        self.retire(stopped);
    }
}

/// The digest a stream Open names for a worker started from `candidate`.
fn stream_digest(candidate: &CandidateRef) -> &str {
    candidate
        .descriptor_digest
        .as_deref()
        .unwrap_or(&candidate.bundle_digest)
}

fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
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

fn contact_of(snapshot: &ChannelSnapshot) -> ControlContact {
    match snapshot.contact {
        ContactState::Failed => ControlContact::Failed,
        ContactState::InContact if snapshot.ledger.is_some() && snapshot.last_error.is_none() => {
            ControlContact::Reporting
        }
        ContactState::InContact => ControlContact::Pending,
        ContactState::OutOfContact => ControlContact::Unanswered,
    }
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
        contact_of(&self.channel.snapshot())
    }

    fn control_caller(&self) -> Box<dyn RuntimeCall> {
        Box::new(self.channel.caller())
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
