// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use tensorplate_protocol::agent_control::RecoveryAction;
use tensorplate_protocol::agent_state::{
    ErrorRecord, QuarantineRecord, TransactionKind, TransactionRecord,
};
use tensorplate_protocol::deploy_transaction::DeployState;

use crate::coordinator::Coordinator;
use crate::recovery::apply_startup;
use crate::worker::{WorkerControl, WorkerReadiness};

/// Puts a directory where a state write makes its first temporary file, so
/// no later write lands, or takes it away again.
pub(super) fn refuse_state_writes(state_dir: &Path, refuse: bool) {
    let in_the_way = state_dir.join("state.json.bak.tmp");
    if refuse {
        fs::create_dir(in_the_way).unwrap();
    } else {
        fs::remove_dir(in_the_way).unwrap();
    }
}

/// What a start would read in `state_dir`: what `active`, `previous_active`
/// and the in-flight transaction name, then the generation counter.
/// `stt@3` names generation 3 of `stt` and its document reads back as that
/// descriptor, `stt@3?` names one whose document does not, `stt` names
/// none, `-` is an empty slot.
pub(super) fn view(state_dir: &Path) -> [String; 4] {
    let state = StateStore::open(state_dir).unwrap().snapshot().unwrap();
    let record = |slot: &Option<DeploymentRecord>| shown(state_dir, slot);
    let transaction = match &state.in_flight_transaction {
        Some(tx) => show(
            state_dir,
            &tx.deployment_id,
            tx.descriptor_identity().unwrap(),
        ),
        None => "-".to_owned(),
    };
    let counter = state
        .next_generation
        .map_or("-".to_owned(), |n| n.to_string());
    let records = [record(&state.active), record(&state.previous_active)];
    let [active, previous] = records;
    [active, previous, transaction, counter]
}

fn show(state_dir: &Path, id: &str, identity: Option<DescriptorIdentity<'_>>) -> String {
    let Some(named) = identity else {
        return id.to_owned();
    };
    let read = DescriptorStore::new(state_dir).read(id, named);
    format!("{id}@{}{}", named.generation, read.map_or("?", |_| ""))
}

/// What the record in `slot` names, as [`view`] shows it.
pub(super) fn shown(state_dir: &Path, slot: &Option<DeploymentRecord>) -> String {
    match slot {
        Some(record) => show(
            state_dir,
            &record.deployment_id,
            record.descriptor_identity().unwrap(),
        ),
        None => "-".to_owned(),
    }
}

/// One call to `prepare`: the reference, what a start would have read at
/// that moment, and the descriptor the registry would have read for it.
pub(super) struct Asked {
    pub(super) reference: CandidateRef,
    pub(super) view: [String; 4],
    pub(super) resolved: Option<DeploymentDescriptor>,
    /// What the candidate record named at that moment.
    pub(super) candidate: String,
    /// What a start would have read when the hold in force was taken; none
    /// when the worker control was not held.
    pub(super) held_since: Option<[String; 4]>,
}

/// A worker control that records what it is asked to prepare.
#[derive(Default)]
pub(super) struct Worker {
    state_dir: PathBuf,
    asked: Mutex<Vec<Asked>>,
    /// `prepare` ends the agent as a kill would: no later state write lands.
    killed_at_prepare: bool,
    /// The same, at the first hold.
    killed_at_hold: bool,
    /// The same, in this call (`prepare` or `warm`) for this generation.
    killed_in: Option<(&'static str, u64)>,
    stopping: AtomicBool,
    active: Option<String>,
    /// The view at each hold that has not been released.
    holds: Mutex<Vec<[String; 4]>>,
    /// Releases of a hold that was never taken.
    unmatched_releases: AtomicUsize,
}

impl Worker {
    pub(super) fn killed() -> Self {
        Self {
            killed_at_prepare: true,
            ..Self::default()
        }
    }
    pub(super) fn killed_at_its_hold() -> Self {
        Self {
            killed_at_hold: true,
            ..Self::default()
        }
    }
    pub(super) fn killed_in(call: &'static str, generation: u64) -> Self {
        Self {
            killed_in: Some((call, generation)),
            ..Self::default()
        }
    }
    /// End the agent as a kill would: no later state write lands.
    fn kill(&self) -> AgentError {
        refuse_state_writes(&self.state_dir, true);
        self.stopping.store(true, Ordering::SeqCst);
        AgentError::WorkerExited("killed".into())
    }
    pub(super) fn serving(deployment_id: &str) -> Self {
        Self {
            active: Some(deployment_id.into()),
            ..Self::default()
        }
    }
    /// Whether every hold was released and nothing else was.
    pub(super) fn released(&self) -> bool {
        self.holds.lock().unwrap().is_empty() && self.unmatched_releases.load(Ordering::SeqCst) == 0
    }
}

impl WorkerControl for Worker {
    fn hold(&self) {
        self.holds.lock().unwrap().push(view(&self.state_dir));
        if self.killed_at_hold {
            refuse_state_writes(&self.state_dir, true);
            self.stopping.store(true, Ordering::SeqCst);
        }
    }
    fn release(&self) {
        if self.holds.lock().unwrap().pop().is_none() {
            self.unmatched_releases.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn prepare(&self, _: &str, candidate: &CandidateRef, _: Duration) -> AgentResult<()> {
        let descriptors = DescriptorStore::new(&self.state_dir);
        let generation = candidate.generation.unwrap_or(0);
        let read = StateStore::open(&self.state_dir).unwrap();
        self.asked.lock().unwrap().push(Asked {
            reference: candidate.clone(),
            view: view(&self.state_dir),
            resolved: resolve(Some(&descriptors), candidate, generation).unwrap(),
            candidate: shown(&self.state_dir, &read.snapshot().unwrap().candidate),
            held_since: self.holds.lock().unwrap().last().cloned(),
        });
        if self.killed_at_prepare || self.killed_in == Some(("prepare", generation)) {
            return Err(self.kill());
        }
        Ok(())
    }
    fn warm(&self, _: &str, candidate: &CandidateRef, _: Duration) -> AgentResult<WorkerReadiness> {
        let generation = candidate.generation.unwrap_or(0);
        if self.killed_in == Some(("warm", generation)) {
            return Err(self.kill());
        }
        let deployment_id = candidate.deployment_id.clone();
        let ready = true;
        Ok(WorkerReadiness {
            deployment_id,
            ready,
        })
    }
    fn promote(&self, _: &str, _: &CandidateRef) -> AgentResult<()> {
        Ok(())
    }
    fn unload(&self, _: &str) {}
    fn active_deployment_id(&self) -> AgentResult<Option<String>> {
        Ok(self.active.clone())
    }
    fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }
}

/// One run of the agent over a state directory.
pub(super) struct Agent {
    pub(super) store: Arc<StateStore>,
    pub(super) worker: Arc<Worker>,
    pub(super) coordinator: Coordinator,
}

impl Agent {
    /// The one call the worker control got to `prepare`, which ended the
    /// hold it was made under.
    pub(super) fn asked(&self) -> Asked {
        let mut asked = self.worker.asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert!(self.worker.released());
        asked.remove(0)
    }
    pub(super) fn never_asked(&self) -> bool {
        self.worker.asked.lock().unwrap().is_empty() && self.worker.released()
    }
    /// Put a transaction in flight that is about to ask for its worker and
    /// names no descriptor.
    pub(super) fn in_flight(&self, kind: TransactionKind, id: &str, deployment_id: &str) {
        let phase = DeployState::CapacityChecked;
        let transaction = serde_json::json!({
            "transaction_id": id, "deployment_id": deployment_id, "phase": phase, "kind": kind,
        });
        let transaction: TransactionRecord = serde_json::from_value(transaction).unwrap();
        let begin = |state: &mut AgentState| {
            state.in_flight_transaction = Some(transaction);
            Ok(())
        };
        self.store.update(begin).unwrap();
    }
    fn again(&self, record: &DeploymentRecord, start: Start<'_>) -> AgentResult<CandidateRef> {
        let worker = self.worker.as_ref();
        let in_hand = start_again(&self.store, worker, record, "entry.json".into(), start)?;
        Ok(in_hand.reference().clone())
    }
    /// The `active` and the `previous_active` record.
    fn records(&self) -> [DeploymentRecord; 2] {
        let state = self.store.snapshot().unwrap();
        [state.active.unwrap(), state.previous_active.unwrap()]
    }
    /// The state writes of a rollback to the previous record, `stt`, up to
    /// the one that records `phase`, and none after.
    fn rollback_up_to(&self, phase: DeployState) {
        let [_, previous] = self.records();
        self.in_flight(TransactionKind::Rollback, "tx", "stt");
        self.again(&previous, Start::Rollback("tx")).unwrap();
        self.store.record_phase("tx", phase, 1).unwrap();
    }
}

/// Where the agent of a test keeps its state and its staged bundles.
pub(super) struct Host {
    pub(super) td: TempDir,
    pub(super) descriptors: DescriptorStore,
}

pub(super) fn host() -> Host {
    let td = TempDir::new().unwrap();
    let descriptors = DescriptorStore::new(td.path().join("state"));
    Host { td, descriptors }
}

impl Host {
    pub(super) fn state_dir(&self) -> PathBuf {
        self.td.path().join("state")
    }
    /// The speech-to-text fixture bundle staged for `id`, as a deploy
    /// stages it.
    pub(super) fn staged(&self, id: &str) -> PathBuf {
        let staged = self.td.path().join("staging").join(id);
        fs::create_dir_all(&staged).unwrap();
        for file in ["manifest.json", "entry.json"] {
            let from = repo("test/models/bundles/v0_2/speech_stt_streaming").join(file);
            fs::copy(from, staged.join(file)).unwrap();
        }
        staged
    }
    /// Stage another bundle for `stt`; returns its digest.
    pub(super) fn restage(&self) -> String {
        let staged = self.staged("stt");
        let manifest = fs::read_to_string(staged.join("manifest.json")).unwrap();
        let (was, is) = ("\"version\": \"0.2.0\"", "\"version\": \"0.2.1\"");
        assert!(manifest.contains(was));
        fs::write(staged.join("manifest.json"), manifest.replace(was, is)).unwrap();
        crate::bundle::staged_manifest(&staged)
            .unwrap()
            .bundle_digest
    }
    /// Start the agent as `main` does up to its recovery: open the state,
    /// then prune. Returns how many documents the prune removed.
    pub(super) fn start(&self, worker: Worker) -> (Agent, usize) {
        self.start_probed(worker, probes())
    }
    /// [`Self::start`] for an agent that holds `probes`.
    pub(super) fn start_probed(
        &self,
        worker: Worker,
        probes: BTreeMap<String, BackendProbeReport>,
    ) -> (Agent, usize) {
        let config = agent_config(self.td.path());
        let store = Arc::new(StateStore::open(self.state_dir()).unwrap());
        let pruned = prune_at_start(&store).unwrap();
        let state_dir = self.state_dir();
        let worker = Arc::new(Worker {
            state_dir,
            ..worker
        });
        let coordinator = Coordinator::new(config, store.clone(), worker.clone());
        let coordinator = coordinator.with_backend_probes(probes);
        let agent = Agent {
            store,
            worker,
            coordinator,
        };
        (agent, pruned)
    }
    /// A started agent whose `active` is `stt` at generation 1, described.
    fn restoring(&self, worker: Worker) -> (Agent, DeploymentDescriptor) {
        let (agent, _) = self.start(worker);
        let staged = self.staged("stt").display().to_string();
        let first = deploy(&agent.store, &self.descriptors, "stt", &staged).unwrap();
        (agent, first)
    }
    /// A started agent whose `previous_active` is `stt` at generation 1 and
    /// whose `active` is `tts` at generation 2, both described.
    fn rolling_back(&self, worker: Worker) -> (Agent, DeploymentDescriptor) {
        let (agent, first) = self.restoring(worker);
        let retire = |state: &mut AgentState| {
            state.previous_active = state.active.take();
            Ok(())
        };
        agent.store.update(retire).unwrap();
        let staged = self.staged("tts").display().to_string();
        deploy(&agent.store, &self.descriptors, "tts", &staged).unwrap();
        (agent, first)
    }
    pub(super) fn view(&self) -> [String; 4] {
        view(&self.state_dir())
    }
    pub(super) fn last_error(&self) -> Option<ErrorRecord> {
        let read = StateStore::open(self.state_dir()).unwrap();
        read.snapshot().unwrap().last_error
    }
    /// The quarantined transactions, as `tx@Promoted`, and whether the last
    /// error is that of the latest one.
    pub(super) fn quarantined(&self) -> (String, bool) {
        let read = StateStore::open(self.state_dir()).unwrap();
        let state = read.snapshot().unwrap();
        let show = |q: &QuarantineRecord| format!("{}@{:?}", q.transaction_id, q.phase);
        let shown: Vec<_> = state.quarantined.iter().map(show).collect();
        let latest = state.quarantined.last().map(|q| &q.error);
        (shown.join(" "), latest == state.last_error.as_ref())
    }
    /// The documents in the state directory, as `stt-1 tts-2`.
    pub(super) fn documents(&self) -> String {
        let entries = fs::read_dir(self.state_dir()).unwrap();
        let names = entries.map(|e| e.unwrap().file_name().into_string().unwrap());
        let mut documents: Vec<_> = names.filter(|name| is_document(name)).collect();
        documents.sort();
        let short = |name: &String| name[PREFIX.len()..name.len() - SUFFIX.len()].to_owned();
        documents.iter().map(short).collect::<Vec<_>>().join(" ")
    }
    /// The document of `stt` at generation 1.
    fn document(&self) -> PathBuf {
        self.state_dir().join("descriptor-stt-1.json")
    }
}

/// Clear the descriptor identity of the record `slot` picks.
fn undescribe(agent: &Agent, slot: fn(&mut AgentState) -> Option<&mut DeploymentRecord>) {
    let clear = |state: &mut AgentState| {
        let record = slot(state).unwrap();
        record.generation = None;
        record.descriptor_digest = None;
        record.configuration_digest = None;
        Ok(())
    };
    agent.store.update(clear).unwrap();
}

#[test]
fn a_restore_names_and_writes_the_next_generation_before_it_asks_for_a_worker() {
    let host = host();
    let (agent, first) = host.restoring(Worker::default());

    let summary = apply_startup(&agent.coordinator).unwrap();

    assert_eq!(summary.action, RecoveryAction::RestoreActive);
    let asked = agent.asked();
    // The hold began before the generation was taken.
    let before = ["stt@1", "-", "-", "2"].map(String::from);
    assert_eq!(asked.held_since, Some(before));
    assert_eq!(asked.view, ["stt@2", "-", "-", "3"]);
    assert_eq!(asked.resolved, Some(first.restamp(2).unwrap()));
    assert_eq!(host.view(), ["stt@2", "-", "-", "3"]);
    assert_eq!(host.last_error(), None);
    // The document the restore replaced stays until the next start.
    assert_eq!(host.documents(), "stt-1 stt-2");
    assert_eq!(host.start(Worker::default()).1, 1);
    assert_eq!(host.documents(), "stt-2");
}

#[test]
fn a_restore_of_a_record_that_names_no_descriptor_writes_and_names_nothing() {
    let host = host();
    let (agent, first) = host.restoring(Worker::default());
    undescribe(&agent, |state| state.active.as_mut());

    apply_startup(&agent.coordinator).unwrap();

    let asked = agent.asked();
    assert_eq!(asked.reference, naming(&reference(&first), [false; 3]));
    assert_eq!(asked.held_since, None);
    assert_eq!(asked.view, ["stt", "-", "-", "2"]);
    assert_eq!(host.view(), ["stt", "-", "-", "2"]);
    assert_eq!(host.documents(), "stt-1");
}

#[test]
fn a_restore_of_a_record_that_names_no_descriptor_starts_whatever_is_staged_under_its_id() {
    let host = host();
    let (agent, first) = host.restoring(Worker::default());
    undescribe(&agent, |state| state.active.as_mut());
    let staged = host.restage();

    apply_startup(&agent.coordinator).unwrap();

    // As before descriptors: the staged bundle is not compared with the record's.
    let asked = agent.asked();
    assert_ne!(asked.reference.bundle_digest, staged);
    assert_eq!(asked.reference, naming(&reference(&first), [false; 3]));
    assert_eq!(host.last_error(), None);
}

#[test]
fn a_restore_fails_the_start_unless_the_staged_copy_reads_and_the_document_is_the_records() {
    type Broken = (fn(&Host, &Agent), &'static str, ErrorCode);
    let load_failed = ErrorCode::LoadFailed;
    let missing: Broken = (
        |host, _| fs::remove_file(host.document()).unwrap(),
        "descriptor-stt-1.json` is missing",
        load_failed,
    );
    let another: Broken = (
        |host, _| {
            let elsewhere = host.staged("elsewhere").display().to_string();
            let other = derive("stt", 1, &elsewhere);
            host.descriptors.write(&other).unwrap();
        },
        "descriptor-stt-1.json` is generation 1 of `stt` (",
        load_failed,
    );
    // Whether the staged bundle is the record's cannot be told.
    let unreadable: Broken = (
        |host, _| fs::remove_file(host.staged("stt").join("manifest.json")).unwrap(),
        "bundle manifest is invalid",
        ErrorCode::ConfigInvalid,
    );
    // The record and the staged bundle agree; the document describes another.
    let renamed: Broken = (
        |host, agent| {
            let digest = host.restage();
            let rename = |state: &mut AgentState| {
                state.active.as_mut().unwrap().bundle_digest = digest;
                Ok(())
            };
            agent.store.update(rename).unwrap();
        },
        "descriptor-stt-1.json` is not re-stamped: it describes bundle `sha256:",
        load_failed,
    );
    // The staged manifest is the record's, so its digest compares equal; the
    // restore fails on the artifact the manifest names.
    let gone: Broken = (
        |host, _| fs::remove_file(host.staged("stt").join("entry.json")).unwrap(),
        "artifact `entry.json` failed integrity check: missing on disk",
        load_failed,
    );
    let altered: Broken = (
        |host, _| fs::write(host.staged("stt").join("entry.json"), b"{}").unwrap(),
        "artifact `entry.json` failed integrity check: digest mismatch",
        load_failed,
    );
    let no_directory: Broken = (
        |host, _| fs::remove_dir_all(host.staged("stt")).unwrap(),
        "does not exist or is not a directory",
        ErrorCode::ConfigInvalid,
    );
    for (broken, says, code) in [
        missing,
        another,
        unreadable,
        renamed,
        gone,
        altered,
        no_directory,
    ] {
        let host = host();
        let (agent, _) = host.restoring(Worker::default());
        broken(&host, &agent);
        let before = (host.view(), host.documents());

        let error = apply_startup(&agent.coordinator)
            .expect_err(says)
            .to_record();

        let refused = error.code == code && !error.recoverable;
        assert!(refused && error.message.contains(says), "{error:?}");
        assert_eq!(host.last_error(), Some(error));
        assert_eq!((host.view(), host.documents()), before, "{says}");
        assert!(agent.never_asked() && before.0[3] == "2", "{says}");
    }
}

#[test]
fn a_restore_killed_after_its_document_leaves_it_unnamed_for_the_next_start_to_remove() {
    let host = host();
    let (agent, first) = host.restoring(Worker::default());
    refuse_state_writes(&host.state_dir(), true);

    let error = apply_startup(&agent.coordinator).unwrap_err();

    assert!(matches!(error, AgentError::Io(_)), "{error}");
    refuse_state_writes(&host.state_dir(), false);
    assert_eq!(host.view(), ["stt@1", "-", "-", "2"]);
    assert!(agent.never_asked() && host.documents() == "stt-1 stt-2");

    // The next start removes it, reads the old record and is handed the
    // same generation.
    let (next, pruned) = host.start(Worker::default());
    assert_eq!((pruned, host.documents().as_str()), (1, "stt-1"));
    apply_startup(&next.coordinator).unwrap();
    assert_eq!(next.asked().resolved, Some(first.restamp(2).unwrap()));
    assert_eq!(host.view(), ["stt@2", "-", "-", "3"]);
}

#[test]
fn a_restore_killed_after_it_named_the_generation_is_restored_from_that_generation() {
    let host = host();
    let (agent, first) = host.restoring(Worker::killed());

    apply_startup(&agent.coordinator).unwrap_err();

    refuse_state_writes(&host.state_dir(), false);
    assert_eq!(host.view(), ["stt@2", "-", "-", "3"]);
    assert_eq!(host.last_error(), None);

    let (next, pruned) = host.start(Worker::default());
    assert_eq!((pruned, host.documents().as_str()), (1, "stt-2"));
    apply_startup(&next.coordinator).unwrap();
    assert_eq!(next.asked().resolved, Some(first.restamp(3).unwrap()));
    assert_eq!(host.view(), ["stt@3", "-", "-", "4"]);
}

#[test]
fn a_rollback_names_the_next_generation_on_its_transaction_and_the_swap_on_the_record() {
    let host = host();
    let (agent, first) = host.rolling_back(Worker::default());

    let outcome = agent.coordinator.rollback(None).unwrap();

    assert_eq!(outcome.deployment_id, "stt");
    let asked = agent.asked();
    // The hold began before the generation was taken.
    let before = ["tts@2", "stt@1", "stt", "3"].map(String::from);
    assert_eq!(asked.held_since, Some(before));
    assert_eq!(asked.view, ["tts@2", "stt@1", "stt@3", "4"]);
    assert_eq!(asked.resolved, Some(first.restamp(3).unwrap()));
    assert_eq!(host.view(), ["stt@3", "tts@2", "-", "4"]);
    assert_eq!(host.last_error(), None);
    // The document the rollback replaced stays until the next start.
    assert_eq!(host.documents(), "stt-1 stt-3 tts-2");
    assert_eq!(host.start(Worker::default()).1, 1);
    assert_eq!(host.documents(), "stt-3 tts-2");
}

#[test]
fn a_rollback_to_a_record_that_names_no_descriptor_writes_and_names_nothing() {
    let host = host();
    let (agent, first) = host.rolling_back(Worker::default());
    undescribe(&agent, |state| state.previous_active.as_mut());

    agent.coordinator.rollback(None).unwrap();

    let asked = agent.asked();
    assert_eq!(asked.reference, naming(&reference(&first), [false; 3]));
    assert_eq!(asked.held_since, None);
    assert_eq!(asked.view, ["tts@2", "stt", "stt", "3"]);
    assert_eq!(host.view(), ["stt", "tts@2", "-", "3"]);
    assert_eq!(host.documents(), "stt-1 tts-2");
}

#[test]
fn a_rollback_is_refused_when_the_recorded_document_is_missing() {
    let host = host();
    let (agent, _) = host.rolling_back(Worker::default());
    fs::remove_file(host.document()).unwrap();

    let error = agent.coordinator.rollback(None).unwrap_err().to_record();

    let load_failed = error.code == ErrorCode::LoadFailed && !error.recoverable;
    let says = "descriptor-stt-1.json` is missing";
    assert!(load_failed && error.message.ends_with(says), "{error:?}");
    assert_eq!(host.last_error(), Some(error));
    assert_eq!(host.view(), ["tts@2", "stt@1?", "-", "3"]);
    assert!(agent.never_asked() && host.documents() == "tts-2");
}

#[test]
fn a_rollback_killed_after_its_document_leaves_it_unnamed_for_the_next_start_to_remove() {
    let host = host();
    let (agent, first) = host.rolling_back(Worker::default());
    let [_, previous] = agent.records();
    agent.in_flight(TransactionKind::Rollback, "tx", "stt");
    refuse_state_writes(&host.state_dir(), true);

    let error = agent.again(&previous, Start::Rollback("tx")).unwrap_err();

    assert!(matches!(error, AgentError::Io(_)), "{error}");
    refuse_state_writes(&host.state_dir(), false);
    assert_eq!(host.view(), ["tts@2", "stt@1", "stt", "3"]);
    assert_eq!(host.documents(), "stt-1 stt-3 tts-2");

    // The next start removes it and resumes the rollback from the old
    // record, which is handed the same generation.
    let (next, pruned) = host.start(Worker::default());
    assert_eq!((pruned, host.documents().as_str()), (1, "stt-1 tts-2"));
    let summary = apply_startup(&next.coordinator).unwrap();
    assert_eq!(summary.action, RecoveryAction::ResumePrepare);
    assert_eq!(next.asked().resolved, Some(first.restamp(3).unwrap()));
    assert_eq!(host.view(), ["stt@3", "tts@2", "-", "4"]);
}

#[test]
fn a_rollback_killed_after_it_named_the_generation_starts_over_from_the_previous_record() {
    let host = host();
    let (agent, first) = host.rolling_back(Worker::killed());

    let error = agent.coordinator.rollback(None).unwrap_err();

    assert!(matches!(error, AgentError::Stopping), "{error}");
    refuse_state_writes(&host.state_dir(), false);
    assert_eq!(host.view(), ["tts@2", "stt@1", "stt@3", "4"]);
    assert_eq!(host.last_error(), None);

    // Every document is named, so the next start removes none; its
    // rollback starts from the previous record's own document.
    let (next, pruned) = host.start(Worker::default());
    assert_eq!(pruned, 0);
    apply_startup(&next.coordinator).unwrap();
    assert_eq!(next.asked().resolved, Some(first.restamp(4).unwrap()));
    assert_eq!(host.view(), ["stt@4", "tts@2", "-", "5"]);
    assert_eq!(host.start(Worker::default()).1, 2);
    assert_eq!(host.documents(), "stt-4 tts-2");
}

#[test]
fn a_rollback_killed_after_its_swap_is_finalized_with_the_identity_the_swap_recorded() {
    let host = host();
    let (agent, _) = host.rolling_back(Worker::default());
    agent.rollback_up_to(DeployState::Promoted);
    agent.store.swap_active_with_previous(2).unwrap();
    assert_eq!(host.view(), ["stt@3", "tts@2", "stt@3", "4"]);

    // The next start keeps the swapped record's document and removes the
    // one the rollback replaced.
    let (next, pruned) = host.start(Worker::serving("stt"));
    assert_eq!((pruned, host.documents().as_str()), (1, "stt-3 tts-2"));
    let summary = apply_startup(&next.coordinator).unwrap();
    assert_eq!(summary.action, RecoveryAction::FinalizePromotion);
    assert_eq!(host.view(), ["stt@3", "tts@2", "-", "4"]);
    assert!(next.never_asked());
}

#[test]
fn a_rollback_killed_after_its_swap_is_quarantined_by_a_start_that_finds_nothing_serving() {
    let host = host();
    let (agent, first) = host.rolling_back(Worker::default());
    agent.rollback_up_to(DeployState::Promoted);
    agent.store.swap_active_with_previous(2).unwrap();
    assert_eq!(host.view(), ["stt@3", "tts@2", "stt@3", "4"]);

    // A worker control reports nothing active when the agent starts. The
    // start removes the document the rollback replaced, quarantines the
    // transaction and restores nothing.
    let (next, pruned) = host.start(Worker::default());
    assert_eq!((pruned, host.documents().as_str()), (1, "stt-3 tts-2"));
    let summary = apply_startup(&next.coordinator).unwrap();
    assert_eq!(summary.action, RecoveryAction::QuarantineCandidate);
    assert_eq!(host.view(), ["stt@3", "tts@2", "-", "4"]);
    assert_eq!(host.quarantined(), ("tx@Promoted".to_owned(), true));
    assert!(next.never_asked() && host.documents() == "stt-3 tts-2");

    // The start after it restores the swapped record at the next generation.
    let (after, pruned) = host.start(Worker::default());
    assert_eq!((pruned, host.documents().as_str()), (0, "stt-3 tts-2"));
    let summary = apply_startup(&after.coordinator).unwrap();
    assert_eq!(summary.action, RecoveryAction::RestoreActive);
    assert_eq!(after.asked().resolved, Some(first.restamp(4).unwrap()));
    assert_eq!(host.view(), ["stt@4", "tts@2", "-", "5"]);
    assert_eq!(host.last_error(), None);
    assert_eq!(host.documents(), "stt-3 stt-4 tts-2");
}

#[test]
fn a_rollback_killed_before_its_swap_keeps_its_document_until_the_start_after_its_quarantine() {
    use DeployState::{Prepared, Promoted, Warmed};
    for phase in [Prepared, Warmed, Promoted] {
        let host = host();
        let (agent, _) = host.rolling_back(Worker::default());
        agent.rollback_up_to(phase);
        assert_eq!(host.view(), ["tts@2", "stt@1", "stt@3", "4"]);

        // Every document is named, so the next start removes none. It
        // quarantines the transaction, and the document it named stays.
        let (next, pruned) = host.start(Worker::default());
        assert_eq!(pruned, 0, "{phase:?}");
        let summary = apply_startup(&next.coordinator).unwrap();
        assert_eq!(summary.action, RecoveryAction::QuarantineCandidate);
        assert_eq!(host.view(), ["tts@2", "stt@1", "-", "4"]);
        assert_eq!(host.quarantined(), (format!("tx@{phase:?}"), true));
        assert!(next.never_asked() && host.documents() == "stt-1 stt-3 tts-2");

        // No record names it now: the start after that removes it, and
        // restores the active record.
        let (after, pruned) = host.start(Worker::default());
        assert_eq!((pruned, host.documents().as_str()), (1, "stt-1 tts-2"));
        apply_startup(&after.coordinator).unwrap();
        assert_eq!(host.view(), ["tts@4", "stt@1", "-", "5"]);
        assert_eq!(host.documents(), "stt-1 tts-2 tts-4");
    }
}

#[test]
fn start_again_refuses_a_record_out_of_its_slot_and_a_rollback_that_is_not_in_flight() {
    use TransactionKind::{Deploy, Rollback};
    let host = host();
    let (agent, _) = host.rolling_back(Worker::default());
    let [active, previous] = agent.records();
    let refused = |record: &DeploymentRecord, start: Start<'_>| refusal(agent.again(record, start));

    let says = "descriptor-stt-1.json` is not re-stamped: the active deployment record does not";
    assert!(refused(&previous, Start::Restore).contains(says));
    let says = "descriptor-tts-2.json` is not re-stamped: the previous active deployment record";
    assert!(refused(&active, Start::Rollback("tx")).contains(says));

    let mut partial = previous.clone();
    partial.descriptor_digest = None;
    assert!(refused(&partial, Start::Rollback("tx")).contains("all three or none"));

    let says = "is not re-stamped: rollback transaction `tx` of `stt` is not in flight";
    assert!(refused(&previous, Start::Rollback("tx")).contains(says));
    let others = [
        (Rollback, "another", "stt"),
        (Rollback, "tx", "tts"),
        (Deploy, "tx", "stt"),
    ];
    for (kind, id, deployment_id) in others {
        agent.in_flight(kind, id, deployment_id);
        let message = refused(&previous, Start::Rollback("tx"));
        assert!(message.contains(says), "{id} {deployment_id}: {message}");
        assert_eq!(host.view(), ["tts@2", "stt@1", deployment_id, "3"]);
    }
    assert_eq!(host.documents(), "stt-1 tts-2");
}

#[test]
fn start_again_spends_no_generation_when_the_document_cannot_be_written() {
    let host = host();
    let (agent, _) = host.restoring(Worker::default());
    let active = agent.store.snapshot().unwrap().active.unwrap();
    let unfinished = host.state_dir().join("descriptor-stt-2.json.tmp");
    fs::create_dir(&unfinished).unwrap();

    let error = agent.again(&active, Start::Restore).unwrap_err();

    assert!(matches!(error, AgentError::Io(_)), "{error}");
    assert_eq!(host.view(), ["stt@1", "-", "-", "2"]);
    assert_eq!(agent.store.snapshot().unwrap().next_generation, Some(2));
    fs::remove_dir(unfinished).unwrap();
    agent.again(&active, Start::Restore).unwrap();
    assert_eq!(host.view(), ["stt@2", "-", "-", "3"]);
}

#[test]
fn the_prune_at_a_start_keeps_the_documents_either_state_file_names() {
    let host = host();
    let (agent, first) = host.restoring(Worker::default());
    // A write that stops between its renames: the backup holds the newer
    // state, the primary the one before it.
    let primary = host.state_dir().join("state.json");
    let older = fs::read(&primary).unwrap();
    let active = agent.store.snapshot().unwrap().active.unwrap();
    agent.again(&active, Start::Restore).unwrap();
    fs::write(&primary, older).unwrap();
    host.descriptors.write(&first.restamp(9).unwrap()).unwrap();
    assert_eq!(host.view(), ["stt@1", "-", "-", "2"]);

    let (_, pruned) = host.start(Worker::default());

    assert_eq!((pruned, host.documents().as_str()), (1, "stt-1 stt-2"));
    // A start that finds the primary damaged reads the backup, and the
    // document it names.
    fs::write(&primary, "{").unwrap();
    assert_eq!(host.view(), ["stt@2", "-", "-", "3"]);
}

#[test]
fn the_prune_at_a_start_removes_nothing_when_a_state_file_cannot_be_read() {
    let host = host();
    let (agent, first) = host.restoring(Worker::default());
    host.descriptors.write(&first.restamp(9).unwrap()).unwrap();
    let backup = host.state_dir().join("state.json.bak");
    fs::remove_file(&backup).unwrap();
    fs::create_dir(&backup).unwrap();

    let error = prune_at_start(&agent.store).unwrap_err();

    assert!(matches!(error, AgentError::Io(_)), "{error}");
    assert_eq!(host.documents(), "stt-1 stt-9");
    fs::remove_dir(&backup).unwrap();
    assert_eq!(prune_at_start(&agent.store).unwrap(), 1);
    assert_eq!(host.documents(), "stt-1");
}
