// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use tensorplate_protocol::agent_control::RecoveryAction;
use tensorplate_protocol::agent_state::{ErrorRecord, TransactionKind, TransactionRecord};
use tensorplate_protocol::deploy_transaction::DeployState;

use crate::coordinator::Coordinator;
use crate::recovery::apply_startup;
use crate::worker::{WorkerControl, WorkerReadiness};

/// Puts a directory where a state write makes its first temporary file, so
/// no later write lands, or takes it away again.
fn refuse_state_writes(state_dir: &Path, refuse: bool) {
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
fn view(state_dir: &Path) -> [String; 4] {
    let state = StateStore::open(state_dir).unwrap().snapshot().unwrap();
    let descriptors = DescriptorStore::new(state_dir);
    let show = |id: &str, identity: Option<DescriptorIdentity<'_>>| match identity {
        None => id.to_owned(),
        Some(named) => {
            let unread = descriptors.read(id, named).map_or("?", |_| "");
            format!("{id}@{}{unread}", named.generation)
        }
    };
    let record = |slot: &Option<DeploymentRecord>| match slot {
        Some(record) => show(&record.deployment_id, record.descriptor_identity().unwrap()),
        None => "-".to_owned(),
    };
    let transaction = match &state.in_flight_transaction {
        Some(tx) => show(&tx.deployment_id, tx.descriptor_identity().unwrap()),
        None => "-".to_owned(),
    };
    let counter = state
        .next_generation
        .map_or("-".to_owned(), |n| n.to_string());
    let records = [record(&state.active), record(&state.previous_active)];
    let [active, previous] = records;
    [active, previous, transaction, counter]
}

/// One call to `prepare`: the reference, what a start would have read at
/// that moment, and the descriptor the registry would have read for it.
struct Asked {
    reference: CandidateRef,
    view: [String; 4],
    resolved: Option<DeploymentDescriptor>,
}

/// A worker control that records what it is asked to prepare.
#[derive(Default)]
struct Worker {
    state_dir: PathBuf,
    asked: Mutex<Vec<Asked>>,
    /// `prepare` ends the agent as a kill would: no later state write lands.
    killed_at_prepare: bool,
    stopping: AtomicBool,
    active: Option<String>,
}

impl Worker {
    fn killed() -> Self {
        Self {
            killed_at_prepare: true,
            ..Self::default()
        }
    }
    fn serving(deployment_id: &str) -> Self {
        Self {
            active: Some(deployment_id.into()),
            ..Self::default()
        }
    }
}

impl WorkerControl for Worker {
    fn prepare(&self, _: &str, candidate: &CandidateRef, _: Duration) -> AgentResult<()> {
        let descriptors = DescriptorStore::new(&self.state_dir);
        let generation = candidate.generation.unwrap_or(0);
        self.asked.lock().unwrap().push(Asked {
            reference: candidate.clone(),
            view: view(&self.state_dir),
            resolved: resolve(Some(&descriptors), candidate, generation).unwrap(),
        });
        if self.killed_at_prepare {
            refuse_state_writes(&self.state_dir, true);
            self.stopping.store(true, Ordering::SeqCst);
            return Err(AgentError::WorkerExited("killed".into()));
        }
        Ok(())
    }
    fn warm(&self, _: &str, candidate: &CandidateRef, _: Duration) -> AgentResult<WorkerReadiness> {
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
struct Agent {
    store: Arc<StateStore>,
    worker: Arc<Worker>,
    coordinator: Coordinator,
}

impl Agent {
    /// The one call the worker control got to `prepare`.
    fn asked(&self) -> Asked {
        let mut asked = self.worker.asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        asked.remove(0)
    }
    fn never_asked(&self) -> bool {
        self.worker.asked.lock().unwrap().is_empty()
    }
    /// Put a transaction in flight that is about to ask for its worker and
    /// names no descriptor.
    fn in_flight(&self, kind: TransactionKind, id: &str, deployment_id: &str) {
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
        start_again(&self.store, record, "entry.json".into(), start)
    }
    /// The `active` and the `previous_active` record.
    fn records(&self) -> [DeploymentRecord; 2] {
        let state = self.store.snapshot().unwrap();
        [state.active.unwrap(), state.previous_active.unwrap()]
    }
}

/// Where the agent of a test keeps its state and its staged bundles.
struct Host {
    td: TempDir,
    descriptors: DescriptorStore,
}

fn host() -> Host {
    let td = TempDir::new().unwrap();
    let descriptors = DescriptorStore::new(td.path().join("state"));
    Host { td, descriptors }
}

impl Host {
    fn state_dir(&self) -> PathBuf {
        self.td.path().join("state")
    }
    /// The speech-to-text fixture bundle staged for `id`, as a deploy
    /// stages it.
    fn staged(&self, id: &str) -> PathBuf {
        let staged = self.td.path().join("staging").join(id);
        fs::create_dir_all(&staged).unwrap();
        for file in ["manifest.json", "entry.json"] {
            let from = repo("test/models/bundles/v0_2/speech_stt_streaming").join(file);
            fs::copy(from, staged.join(file)).unwrap();
        }
        staged
    }
    /// Stage another bundle for `stt`; returns its digest.
    fn restage(&self) -> String {
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
    fn start(&self, worker: Worker) -> (Agent, usize) {
        let config = serde_json::json!({
            "socket_path": self.td.path().join("agent.sock"),
            "state_dir": self.state_dir(),
            "staging_dir": self.td.path().join("staging"),
            "available_backends": ["mock"],
            "device_memory_bytes": 8_u64 << 30,
        });
        let config: crate::config::AgentConfig = serde_json::from_value(config).unwrap();
        let store = Arc::new(StateStore::open(self.state_dir()).unwrap());
        let pruned = prune_at_start(&store).unwrap();
        let state_dir = self.state_dir();
        let worker = Arc::new(Worker {
            state_dir,
            ..worker
        });
        let config = config.validate().unwrap();
        let coordinator = Coordinator::new(config, store.clone(), worker.clone());
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
    fn view(&self) -> [String; 4] {
        view(&self.state_dir())
    }
    fn last_error(&self) -> Option<ErrorRecord> {
        let read = StateStore::open(self.state_dir()).unwrap();
        read.snapshot().unwrap().last_error
    }
    /// The documents in the state directory, as `stt-1 tts-2`.
    fn documents(&self) -> String {
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
    assert_eq!(asked.view, ["stt", "-", "-", "2"]);
    assert_eq!(host.view(), ["stt", "-", "-", "2"]);
    assert_eq!(host.documents(), "stt-1");
}

#[test]
fn a_restore_is_refused_unless_the_document_and_the_staged_bundle_are_the_records() {
    type Broken = (fn(&Host, &Agent), &'static str);
    let missing: Broken = (
        |host, _| fs::remove_file(host.document()).unwrap(),
        "descriptor-stt-1.json` is missing",
    );
    let another: Broken = (
        |host, _| {
            let elsewhere = host.staged("elsewhere").display().to_string();
            let other = derive("stt", 1, &elsewhere);
            host.descriptors.write(&other).unwrap();
        },
        "descriptor-stt-1.json` is generation 1 of `stt` (",
    );
    let restaged: Broken = (
        |host, _| drop(host.restage()),
        "`stt` is not started again: the bundle staged at",
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
    );
    for (broken, says) in [missing, another, restaged, renamed] {
        let host = host();
        let (agent, _) = host.restoring(Worker::default());
        broken(&host, &agent);
        let before = host.view();

        let error = apply_startup(&agent.coordinator).unwrap_err().to_record();

        let load_failed = error.code == ErrorCode::LoadFailed && !error.recoverable;
        assert!(load_failed && error.message.contains(says), "{error:?}");
        assert_eq!(host.last_error(), Some(error));
        assert_eq!((host.view(), before[3].as_str()), (before.clone(), "2"));
        assert!(agent.never_asked() && host.documents() == "stt-1", "{says}");
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
    // The writes of a rollback up to its swap, and none after.
    let [_, previous] = agent.records();
    agent.in_flight(TransactionKind::Rollback, "tx", "stt");
    agent.again(&previous, Start::Rollback("tx")).unwrap();
    let promoted = DeployState::Promoted;
    agent.store.record_phase("tx", promoted, 1).unwrap();
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
