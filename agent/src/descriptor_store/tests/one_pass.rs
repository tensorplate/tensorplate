// SPDX-License-Identifier: Apache-2.0

//! What ONE start of the agent does about the transaction it finds: every
//! start here has a worker control that holds no worker, as a new agent
//! process does, unless the test says otherwise.

use super::deploy::{killed_after, FORMAT_0_1, STT, TTS};
use super::start_again::{host, Agent, Host, Worker};
use super::*;

use tensorplate_protocol::agent_control::{
    ControlRequest, DeployRequest, RecoveryAction, ResponseStatus, RollbackRequest,
};
use tensorplate_protocol::agent_state::TransactionKind;
use tensorplate_protocol::deploy_transaction::DeployState;
use tensorplate_protocol::MemberRef;

use crate::control::dispatch;
use crate::recovery::apply_startup;
use crate::registry::{durable_generations, Restamp};
use crate::worker::ServingMember;

impl Agent {
    /// The registry's restart stamp for the member `active` names: it is
    /// started again as `generation`.
    fn restart_serving(&self, host: &Host, generation: u64) -> AgentResult<CandidateRef> {
        let active = self.store.snapshot().unwrap().active.unwrap();
        let recipe = CandidateRef {
            deployment_id: active.deployment_id.clone(),
            staged_path: active.staged_path.clone(),
            bundle_digest: active.bundle_digest.clone(),
            backend_hint: active.backend_hint.clone(),
            model_class: active.model_class.clone(),
            bundle_name: Some(active.bundle_name.clone()),
            bundle_version: Some(active.bundle_version.clone()),
            artifact_relative_path: Some("entry.json".into()),
            generation: active.generation,
            descriptor_digest: active.descriptor_digest.clone(),
            configuration_digest: active.configuration_digest.clone(),
            streaming: false,
        };
        let descriptors = Arc::new(DescriptorStore::new(host.state_dir()));
        let stamp = restart_stamp(self.store.clone(), descriptors);
        stamp(&Restamp {
            recipe: &recipe,
            stopped_generation: active.generation.unwrap(),
            generation,
        })
    }
    /// The next generation, taken as the registry takes a restart's.
    fn next_generation(&self) -> u64 {
        durable_generations(self.store.clone())().unwrap()
    }
}

impl Host {
    /// A started agent whose `active` is `tts` at generation 1, described,
    /// and whose deploy of `stt`, transaction `tx`, was killed after it
    /// recorded `phase`.
    fn updating(&self, phase: DeployState) -> Agent {
        let (agent, _) = self.start(Worker::default());
        let serving = self.staged("tts").display().to_string();
        deploy(&agent.store, &self.descriptors, "tts", &serving).unwrap();
        let staged = self.staged("stt").display().to_string();
        let candidate = DeploymentRecord {
            generation: None,
            descriptor_digest: None,
            configuration_digest: None,
            promoted_monotonic_ns: None,
            ..record(&derive("stt", 1, &staged))
        };
        agent.in_flight(TransactionKind::Deploy, "tx", "stt");
        let recorded = |state: &mut AgentState| {
            state.candidate = Some(candidate);
            Ok(())
        };
        agent.store.update(recorded).unwrap();
        agent.store.record_phase("tx", phase, 1).unwrap();
        agent
    }
    /// Whether a start would read a candidate record or a transaction.
    pub(super) fn settled(&self) -> bool {
        let state = StateStore::open(self.state_dir()).unwrap();
        let state = state.snapshot().unwrap();
        state.candidate.is_none() && state.in_flight_transaction.is_none()
    }
    /// The one start of a test: what its recovery reports, and the agent.
    fn one_start(&self, worker: Worker) -> (RecoveryAction, Agent) {
        let (next, _) = self.start(worker);
        let summary = apply_startup(&next.coordinator).unwrap();
        (summary.action, next)
    }
    /// The deployment ids `active` and `previous_active` record.
    fn ids(&self) -> [String; 2] {
        let [active, previous, ..] = self.view();
        [active, previous].map(|shown| shown.split('@').next().unwrap().to_owned())
    }
    /// One start over what a transaction killed after one of its durable
    /// writes left, then another: the first settles everything.
    ///
    /// `serving` is the deployment the first start must start again and
    /// `retained` the one it must leave as previous; `ended_at` is the
    /// phase of the one quarantine record it makes, none for a transaction
    /// that had committed.
    fn settles_in_one_start(
        &self,
        serving: Option<&str>,
        retained: Option<&str>,
        ended_at: Option<DeployState>,
        case: &str,
    ) {
        let recorded = [serving, retained].map(|id| id.unwrap_or("-").to_owned());
        let quarantined = |host: &Self| {
            let (shown, _) = host.quarantined();
            let phase = shown.rsplit('@').next().unwrap().to_owned();
            (!shown.is_empty()).then_some((phase, shown.contains(' ')))
        };
        let ended = ended_at.map(|phase| (format!("{phase:?}"), false));

        let (action, next) = self.one_start(Worker::default());

        let expected = match (serving, ended_at) {
            (Some(_), _) => RecoveryAction::RestoreActive,
            (None, Some(_)) => RecoveryAction::QuarantineCandidate,
            (None, None) => RecoveryAction::NoOp,
        };
        assert_eq!(action, expected, "{case}");
        // Nothing of the transaction is run again: the one worker asked
        // for is the committed record's.
        match serving {
            Some(id) => {
                assert_eq!(next.asked().reference.deployment_id, id, "{case}");
                assert_eq!(self.last_error(), None, "{case}");
            }
            None => assert!(next.never_asked() && self.quarantined().1, "{case}"),
        }
        assert!(self.settled(), "{case}");
        assert_eq!(self.ids(), recorded, "{case}");
        assert_eq!(quarantined(self), ended, "{case}");

        // A second start finds a committed record and nothing else to do.
        let (again, after) = self.one_start(Worker::default());
        let restores = serving.map_or(RecoveryAction::NoOp, |_| RecoveryAction::RestoreActive);
        assert_eq!(again, restores, "{case}");
        assert_eq!(after.never_asked(), serving.is_none(), "{case}");
        assert_eq!((self.ids(), quarantined(self)), (recorded, ended), "{case}");
    }
}

/// The phase a start finds after each durable write of a transaction, up to
/// its commit. `names_a_generation` is whether one write, before the worker
/// is asked for, takes a generation and names a descriptor.
fn phases_before_the_commit(deploy: bool, names_a_generation: bool) -> Vec<DeployState> {
    use DeployState::{CapacityChecked, Prepared, Received, Staged, Verified, Warmed};
    // A deploy records its bundle digest and its candidate in writes of
    // their own, both found at `verified`.
    let mut phases = vec![Received, Verified];
    phases.extend(deploy.then_some([Verified, Verified]).into_iter().flatten());
    phases.extend([Staged, CapacityChecked]);
    phases.extend(names_a_generation.then_some(CapacityChecked));
    phases.extend([Prepared, Warmed]);
    phases
}

#[test]
fn one_start_settles_a_deploy_killed_after_any_of_its_durable_writes() {
    // The record serving before the deploy, the deploy, and whether its
    // bundle format names a descriptor.
    let cases = [
        (None, ("stt", STT), true),
        (Some(("tts", TTS)), ("stt", STT), true),
        (None, ("smoke", FORMAT_0_1), false),
        (
            Some(("smoke-old", FORMAT_0_1)),
            ("smoke", FORMAT_0_1),
            false,
        ),
    ];
    for (old, (id, bundle), described) in cases {
        let phases = phases_before_the_commit(true, described);
        // Then the commit and the end.
        let writes = phases.len() + 2;
        for landed in 1..=writes {
            let case = format!("{id} over {old:?}, killed after write {landed} of {writes}");
            let host = host();
            let (agent, _) = host.start(Worker::default());
            if let Some((old_id, old_bundle)) = old {
                agent.deploy(old_id, old_bundle).unwrap();
            }
            agent.store.kill_after_writes(landed);

            let deployed = agent.deploy(id, bundle);

            assert_eq!(deployed.is_ok(), landed == writes, "{case}");
            let old_id = old.map(|(old_id, _)| old_id);
            match phases.get(landed - 1) {
                Some(phase) => host.settles_in_one_start(old_id, None, Some(*phase), &case),
                None => host.settles_in_one_start(Some(id), old_id, None, &case),
            }
        }
    }
}

/// The format 0.1 fixture bundle at another version, which is another
/// bundle: its manifest's digest differs.
pub(super) fn another_format_0_1_bundle(host: &Host) -> PathBuf {
    let source = host.td.path().join("update");
    fs::create_dir_all(&source).unwrap();
    let manifest = fs::read_to_string(repo(FORMAT_0_1).join("manifest.json")).unwrap();
    let (was, is) = ("\"version\": \"0.1.0\"", "\"version\": \"0.1.1\"");
    assert!(manifest.contains(was));
    fs::write(source.join("manifest.json"), manifest.replace(was, is)).unwrap();
    let artifact = "x86-smoke.json";
    fs::copy(repo(FORMAT_0_1).join(artifact), source.join(artifact)).unwrap();
    source
}

#[test]
fn one_start_after_a_format_0_1_update_of_the_serving_id_killed_after_any_write_restores_the_record(
) {
    let original = repo(FORMAT_0_1);
    let digest_of = |bundle: &Path| parse_bundle(bundle).unwrap().manifest_digest;
    // The update's state writes up to `warmed`. Its bundle is staged after
    // the third, in a tree of its own, which the fourth names.
    let phases = phases_before_the_commit(true, false);
    // `another`: the update is another bundle than the one serving.
    for another in [true, false] {
        for landed in 1..=phases.len() {
            let case = format!("another bundle: {another}, killed after write {landed}");
            let host = host();
            let (agent, _) = host.start(Worker::default());
            agent.deploy("smoke", FORMAT_0_1).unwrap();
            let serving = host.trees("smoke");
            let update = if another {
                another_format_0_1_bundle(&host)
            } else {
                original.clone()
            };
            agent.store.kill_after_writes(landed);
            let coordinator = &agent.coordinator;
            let updated = coordinator.deploy("smoke", &update, BTreeMap::new(), None, None);
            assert!(matches!(updated, Err(AgentError::Io(_))), "{case}");
            drop(agent);
            let staged = host.trees("smoke").len() - serving.len();
            assert_eq!(staged, usize::from(landed >= 3), "{case}");

            let (action, next) = host.one_start(Worker::default());

            // The update is ended, and the record is started from its tree.
            assert_eq!(action, RecoveryAction::RestoreActive, "{case}");
            assert!(host.settled(), "{case}");
            assert_eq!(host.ids(), ["smoke", "-"], "{case}");
            let ended = format!("@{:?}", phases[landed - 1]);
            let (quarantined, _) = host.quarantined();
            let once = quarantined.ends_with(&ended) && !quarantined.contains(' ');
            assert!(once, "{case}: {quarantined}");
            let asked = next.asked().reference;
            assert_eq!(asked.bundle_digest, digest_of(&original), "{case}");
            assert_eq!(
                digest_of(Path::new(&asked.staged_path)),
                digest_of(&original)
            );
            assert_eq!(host.last_error(), None, "{case}");
            // A tree no record named went at that start; one the candidate
            // named goes at the next.
            let named = usize::from(landed >= 4);
            assert_eq!(host.trees("smoke").len() - serving.len(), named, "{case}");
            drop(host.start(Worker::default()));
            assert_eq!(host.trees("smoke"), serving, "{case}");
        }
    }
}

#[test]
fn one_start_settles_a_rollback_killed_after_any_of_its_durable_writes() {
    let cases = [
        (("stt", STT), ("tts", TTS), true),
        (("smoke", FORMAT_0_1), ("smoke-2", FORMAT_0_1), false),
    ];
    for ((first, first_bundle), (second, second_bundle), described) in cases {
        let phases = phases_before_the_commit(false, described);
        let writes = phases.len() + 2;
        for landed in 1..=writes {
            let case = format!("{second} back to {first}, killed after write {landed} of {writes}");
            let host = host();
            let (agent, _) = host.start(Worker::default());
            agent.deploy(first, first_bundle).unwrap();
            agent.deploy(second, second_bundle).unwrap();
            agent.store.kill_after_writes(landed);

            let rolled_back = agent.coordinator.rollback(None);

            assert_eq!(rolled_back.is_ok(), landed == writes, "{case}");
            let (first, second) = (Some(first), Some(second));
            match phases.get(landed - 1) {
                Some(phase) => host.settles_in_one_start(second, first, Some(*phase), &case),
                None => host.settles_in_one_start(first, second, None, &case),
            }
        }
    }
}

#[test]
fn a_start_killed_after_any_write_of_its_pass_is_finished_by_the_next_start() {
    // The pass over an interrupted deploy beside a described record writes
    // three times: the settle, the restore's generation, the cleared error.
    for landed in 0..3 {
        let host = host();
        drop(host.updating(DeployState::Warmed));
        let (killed, _) = host.start(Worker::default());
        killed.store.kill_after_writes(landed);

        let error = apply_startup(&killed.coordinator).unwrap_err();

        assert!(matches!(error, AgentError::Io(_)), "{landed}: {error}");
        let ended = (landed > 0).then_some(DeployState::Warmed);
        let found = host.quarantined().0;
        assert_eq!(found, ended.map_or(String::new(), |_| "tx@Warmed".into()));
        // The settle is not made twice, and the record is started once more.
        let (action, next) = host.one_start(Worker::default());
        assert_eq!(action, RecoveryAction::RestoreActive, "{landed}");
        assert_eq!(next.asked().reference.deployment_id, "tts", "{landed}");
        assert!(host.settled(), "{landed}");
        assert_eq!(host.quarantined().0, "tx@Warmed", "{landed}");
        assert_eq!(host.last_error(), None, "{landed}");
    }
}

#[test]
fn a_start_whose_settling_write_did_not_land_leaves_what_it_found_for_the_next_start() {
    // A committed deploy to clear.
    let host = host();
    let (agent, _) = host.start(Worker::default());
    let writes = phases_before_the_commit(true, true).len() + 2;
    agent.store.kill_after_writes(writes - 1);
    agent.deploy("stt", STT).unwrap_err();
    let found = host.view();
    let (killed, _) = host.start(Worker::default());
    killed.store.kill_after_writes(0);

    let error = apply_startup(&killed.coordinator).unwrap_err();

    assert!(matches!(error, AgentError::Io(_)), "{error}");
    assert!(killed.never_asked() && host.view() == found && !host.settled());
    host.settles_in_one_start(Some("stt"), None, None, "a committed deploy");

    // A rollback recorded failed and not cleared, to clear.
    let host = self::host();
    let (agent, _) = host.rolling_back(Worker::default());
    agent.rollback_up_to(DeployState::Failed);
    let found = host.view();
    let (killed, _) = host.start(Worker::default());
    killed.store.kill_after_writes(0);

    let error = apply_startup(&killed.coordinator).unwrap_err();

    assert!(matches!(error, AgentError::Io(_)), "{error}");
    assert!(killed.never_asked() && host.view() == found && !host.settled());
    let (action, next) = host.one_start(Worker::default());
    assert_eq!(action, RecoveryAction::RestoreActive);
    assert_eq!(next.asked().reference.deployment_id, "tts");
    assert!(host.settled() && host.quarantined().0.is_empty());
    assert_eq!(host.last_error(), None);
}

#[test]
fn a_committed_transaction_whose_end_write_failed_is_busy_until_a_start_clears_it() {
    for rollback in [false, true] {
        let case = if rollback { "rollback" } else { "deploy" };
        let host = host();
        let (agent, _) = host.start(Worker::default());
        agent.deploy("tts", TTS).unwrap();
        if rollback {
            agent.deploy("stt", STT).unwrap();
        }
        // Every write lands but the one that ends the transaction.
        let writes = phases_before_the_commit(!rollback, true).len() + 2;
        agent.store.kill_after_writes(writes - 1);

        let ended = if rollback {
            agent.coordinator.rollback(None).map(|_| ())
        } else {
            agent.deploy("stt", STT)
        };

        assert!(matches!(ended, Err(AgentError::Io(_))), "{case}");
        // The store writes again; the transaction stays, committed.
        agent.store.kill_after_writes(usize::MAX);
        let state = agent.store.snapshot().unwrap();
        let in_flight = state.in_flight_transaction.expect("still in flight");
        assert_eq!(in_flight.phase, DeployState::Promoted, "{case}");
        let (serving, retained) = if rollback {
            ("tts", "stt")
        } else {
            ("stt", "tts")
        };
        assert_eq!(host.ids(), [serving, retained], "{case}");
        // What a client is answered: `busy`, and no transaction id.
        let deploy = DeployRequest {
            bundle_path: repo(FORMAT_0_1).display().to_string(),
            deployment_id: "smoke".into(),
            ..DeployRequest::default()
        };
        let requests = [
            ControlRequest::deploy(None, deploy),
            ControlRequest::rollback(None, RollbackRequest::default()),
        ];
        for request in requests {
            let op = request.op;
            let answered = dispatch(&agent.coordinator, request).unwrap();
            assert_eq!(answered.status, ResponseStatus::Busy, "{case} {op}");
            assert_eq!(answered.transaction_id, None, "{case} {op}");
            let error = answered.error.expect("a busy response carries an error");
            let told = (error.code, error.message.as_str());
            assert_eq!(told, (ErrorCode::NotReady, "agent_busy"), "{case} {op}");
            assert_eq!(answered.agent_status, None, "{case} {op}");
        }
        let still = agent.store.snapshot().unwrap().in_flight_transaction;
        assert_eq!(still, Some(in_flight), "{case}");
        assert_eq!(host.ids(), [serving, retained], "{case}");

        host.settles_in_one_start(Some(serving), Some(retained), None, case);
    }
}

#[test]
fn a_committed_rollback_whose_record_a_member_restart_named_again_is_cleared_by_the_next_start() {
    // The second case rolls back between two records of one id and one
    // configuration.
    for (second, bundle) in [("stt", STT), ("tts", TTS)] {
        let host = host();
        let (agent, _) = host.start(Worker::default());
        agent.deploy("tts", TTS).unwrap();
        agent.deploy(second, bundle).unwrap();
        // Nine writes; every one lands but the one that ends the transaction.
        agent.store.kill_after_writes(8);
        let ended = agent.coordinator.rollback(None);
        assert!(matches!(ended, Err(AgentError::Io(_))), "{ended:?}");
        agent.store.kill_after_writes(usize::MAX);
        let state = agent.store.snapshot().unwrap();
        let tx = state.in_flight_transaction.unwrap();
        assert_eq!(tx.phase, DeployState::Promoted);
        let active = state.active.unwrap();
        assert_eq!(active.deployment_id, "tts");
        assert_eq!(active.generation, tx.generation);
        let [_, retained, ..] = host.view();

        // The serving member stops and is restarted, twice; the stamp of a
        // third restart does not land.
        for _ in 0..2 {
            let generation = agent.next_generation();
            agent.restart_serving(&host, generation).unwrap();
        }
        let generation = agent.next_generation();
        agent.store.kill_after_writes(0);
        agent.restart_serving(&host, generation).unwrap_err();
        drop(agent);

        let (action, next) = host.one_start(Worker::default());

        assert_eq!(action, RecoveryAction::RestoreActive, "{second}");
        assert_eq!(next.asked().reference.deployment_id, "tts", "{second}");
        let [_, previous, ..] = host.view();
        assert_eq!(previous, retained, "{second}");
        assert!(
            host.settled() && host.quarantined().0.is_empty(),
            "{second}"
        );
        assert_eq!(host.last_error(), None, "{second}");
    }
}

#[test]
fn a_committed_rollback_between_two_records_of_one_id_and_one_bundle_is_cleared_by_the_next_start()
{
    // Neither names a descriptor; then the record rolled back to names none,
    // as an agent before descriptors left it, under a described record.
    for (id, bundle, described) in [("smoke", FORMAT_0_1, false), ("tts", TTS, true)] {
        let host = host();
        let (agent, _) = host.start(Worker::default());
        for revision in ["1", "2"] {
            let labels = BTreeMap::from([("revision".to_owned(), revision.to_owned())]);
            let deployed = agent
                .coordinator
                .deploy(id, &repo(bundle), labels, None, None);
            deployed.unwrap();
        }
        if described {
            let undescribe = |state: &mut AgentState| {
                let record = state.previous_active.as_mut().unwrap();
                record.generation = None;
                record.descriptor_digest = None;
                record.configuration_digest = None;
                Ok(())
            };
            agent.store.update(undescribe).unwrap();
        }
        let revision = || {
            let state = StateStore::open(host.state_dir()).unwrap();
            let active = state.snapshot().unwrap().active.unwrap();
            active.labels["revision"].clone()
        };
        // Every write lands but the one that ends the transaction.
        let writes = phases_before_the_commit(false, false).len() + 2;
        agent.store.kill_after_writes(writes - 1);
        let ended = agent.coordinator.rollback(None);
        assert!(matches!(ended, Err(AgentError::Io(_))), "{id}: {ended:?}");
        assert_eq!(revision(), "1", "{id}");
        drop(agent);

        let (action, next) = host.one_start(Worker::default());

        assert_eq!(action, RecoveryAction::RestoreActive, "{id}");
        assert_eq!(next.asked().reference.deployment_id, id, "{id}");
        assert_eq!(revision(), "1", "{id}");
        assert!(host.settled() && host.quarantined().0.is_empty(), "{id}");
        assert_eq!(host.last_error(), None, "{id}");
    }
}

#[test]
fn a_rollback_found_at_promoted_before_its_swap_is_ended_between_records_of_one_configuration() {
    // Only an earlier build leaves this: it recorded `promoted` in a write
    // of its own, before the swap. `restarted`: the serving member was
    // restarted while the rollback ran, as a generation taken before the
    // rollback named its own, the only order the registry's hold allows.
    for restarted in [false, true] {
        let host = host();
        let (agent, _) = host.start(Worker::default());
        agent.deploy("tts", TTS).unwrap();
        agent.deploy("tts", TTS).unwrap();
        let earlier = agent.next_generation();
        // Every write lands up to `warmed`; the swap and the end do not.
        let writes = phases_before_the_commit(false, true).len() + 2;
        agent.store.kill_after_writes(writes - 2);
        agent.coordinator.rollback(None).unwrap_err();
        agent.store.kill_after_writes(usize::MAX);
        if restarted {
            agent.restart_serving(&host, earlier).unwrap();
        }
        let tx = agent.store.snapshot().unwrap().in_flight_transaction;
        let tx = tx.unwrap().transaction_id;
        agent
            .store
            .record_phase(&tx, DeployState::Promoted, 1)
            .unwrap();
        let serving = if restarted { "tts@3" } else { "tts@2" };
        assert_eq!(host.view(), [serving, "tts@1", "tts@4", "5"], "{restarted}");
        drop(agent);

        let (action, next) = host.one_start(Worker::default());

        assert_eq!(action, RecoveryAction::RestoreActive, "{restarted}");
        assert_eq!(next.asked().reference.generation, Some(5), "{restarted}");
        assert_eq!(host.view(), ["tts@5", "tts@1", "-", "6"], "{restarted}");
        assert_eq!(host.quarantined().0, format!("{tx}@Promoted"));
    }
}

#[test]
fn a_rollback_found_before_its_swap_is_ended_whatever_generation_a_later_restart_named() {
    // The rollback's warm fails and the write that ends it does not land:
    // its hold on the member is over, the agent lives on, and the serving
    // member is restarted as a later generation than the rollback named.
    let host = host();
    let (first, _) = host.start(Worker::default());
    first.deploy("tts", TTS).unwrap();
    first.deploy("tts", TTS).unwrap();
    drop(first);
    let (agent, _) = host.start(Worker::failing_in("warm"));
    // Six writes land, up to `prepared`.
    agent.store.kill_after_writes(6);
    let ended = agent.coordinator.rollback(None);
    assert!(matches!(ended, Err(AgentError::Io(_))), "{ended:?}");
    agent.store.kill_after_writes(usize::MAX);
    let later = agent.next_generation();
    agent.restart_serving(&host, later).unwrap();
    assert_eq!(host.view(), ["tts@4", "tts@1", "tts@3", "5"]);
    drop(agent);

    let (action, next) = host.one_start(Worker::default());

    assert_eq!(action, RecoveryAction::RestoreActive);
    assert_eq!(next.asked().reference.generation, Some(5));
    assert_eq!(host.view(), ["tts@5", "tts@1", "-", "6"]);
    assert!(host.quarantined().0.ends_with("@Prepared"));
}

#[test]
fn a_failed_transaction_whose_one_ending_write_did_not_land_is_ended_by_the_next_start() {
    // A deploy refused when its descriptor is derived: six writes, then the
    // one that ends it. Killed before that one, the start ends it instead.
    for landed in [6, 7] {
        let host = host();
        let (agent, _) = host.start_probed(Worker::default(), BTreeMap::new());
        agent.store.kill_after_writes(landed);

        agent.deploy("stt", STT).unwrap_err();

        let ended = host.settled();
        assert_eq!(ended, landed == 7, "{landed}");
        let (action, next) = host.one_start(Worker::default());
        let expected = if ended {
            RecoveryAction::NoOp
        } else {
            RecoveryAction::QuarantineCandidate
        };
        assert_eq!(action, expected, "{landed}");
        assert!(next.never_asked() && host.settled(), "{landed}");
        let (quarantined, is_last_error) = host.quarantined();
        let once = quarantined.ends_with("@CapacityChecked") && !quarantined.contains(' ');
        assert!(once && is_last_error, "{landed}: {quarantined}");
    }
    // A rollback refused when its document is missing: four writes, then the
    // one that ends it, which makes no quarantine record; the start's does.
    for landed in [4, 5] {
        let host = host();
        let (agent, _) = host.rolling_back(Worker::default());
        fs::remove_file(host.state_dir().join("descriptor-stt-1.json")).unwrap();
        agent.store.kill_after_writes(landed);

        agent.coordinator.rollback(None).unwrap_err();

        let ended = host.settled();
        assert_eq!(ended, landed == 5, "{landed}");
        assert_eq!(host.last_error().is_some(), ended, "{landed}");
        let (action, next) = host.one_start(Worker::default());
        assert_eq!(action, RecoveryAction::RestoreActive, "{landed}");
        assert_eq!(next.asked().reference.deployment_id, "tts", "{landed}");
        assert!(host.settled() && host.last_error().is_none(), "{landed}");
        let quarantined = host.quarantined().0;
        let expected = if ended { "" } else { "@CapacityChecked" };
        assert!(quarantined.ends_with(expected), "{landed}: {quarantined}");
        assert_eq!(quarantined.is_empty(), ended, "{landed}");
    }
}

#[test]
fn one_start_restores_the_active_record_past_a_rollback_killed_after_its_last_phase() {
    // Killed after `rolled_back`, before its transaction was cleared.
    let host = host();
    let (agent, first) = host.rolling_back(Worker::default());
    agent.rollback_up_to(DeployState::Promoted);
    agent.store.swap_active_with_previous(2).unwrap();
    let ended = DeployState::RolledBack;
    agent.store.record_phase("tx", ended, 2).unwrap();
    assert_eq!(host.view(), ["stt@3", "tts@2", "stt@3", "4"]);

    let (action, next) = host.one_start(Worker::default());

    assert_eq!(action, RecoveryAction::RestoreActive);
    assert_eq!(next.asked().resolved, Some(first.restamp(4).unwrap()));
    assert_eq!(host.view(), ["stt@4", "tts@2", "-", "5"]);
    assert_eq!(
        (host.quarantined().0.as_str(), host.last_error()),
        ("", None)
    );
}

#[test]
fn one_start_restores_the_active_record_past_a_deploy_killed_after_its_last_phase() {
    // Killed after `active`, before its transaction was cleared.
    let host = host();
    killed_after(&host, DeployState::Promoted);
    let store = StateStore::open(host.state_dir()).unwrap();
    store.promote_candidate(2).unwrap();
    store.record_phase("tx", DeployState::Active, 2).unwrap();

    let (action, next) = host.one_start(Worker::default());

    assert_eq!(action, RecoveryAction::RestoreActive);
    assert_eq!(next.asked().reference.generation, Some(2));
    assert_eq!(host.view(), ["stt@2", "-", "-", "3"]);
    assert_eq!(
        (host.quarantined().0.as_str(), host.last_error()),
        ("", None)
    );
}

#[test]
fn one_start_restores_the_active_record_past_a_rollback_killed_after_it_was_recorded_failed() {
    let host = host();
    let (agent, _) = host.rolling_back(Worker::default());
    agent.rollback_up_to(DeployState::Failed);
    assert_eq!(host.view(), ["tts@2", "stt@1", "stt@3", "4"]);

    let (action, next) = host.one_start(Worker::default());

    assert_eq!(action, RecoveryAction::RestoreActive);
    let asked = next.asked().reference;
    assert_eq!(
        (asked.deployment_id.as_str(), asked.generation),
        ("tts", Some(4))
    );
    assert_eq!(host.view(), ["tts@4", "stt@1", "-", "5"]);
    // The rollback had failed, not been interrupted: nothing is quarantined.
    assert_eq!(
        (host.quarantined().0.as_str(), host.last_error()),
        ("", None)
    );
}

#[test]
fn one_start_ends_a_deploy_killed_before_its_commit_and_restores_the_active_record() {
    use DeployState::{CapacityChecked, Prepared, Promoted, Received, Staged, Verified, Warmed};
    for phase in [
        Received,
        Verified,
        Staged,
        CapacityChecked,
        Prepared,
        Warmed,
        Promoted,
    ] {
        let host = host();
        drop(host.updating(phase));
        assert_eq!(host.view(), ["tts@1", "-", "stt", "2"], "{phase:?}");

        let (action, next) = host.one_start(Worker::default());

        // Nothing of the deploy is run again: the one worker asked for is
        // the active record's.
        assert_eq!(action, RecoveryAction::RestoreActive, "{phase:?}");
        let asked = next.asked().reference;
        assert_eq!(
            (asked.deployment_id.as_str(), asked.generation),
            ("tts", Some(2))
        );
        assert_eq!(host.view(), ["tts@2", "-", "-", "3"], "{phase:?}");
        assert!(host.settled(), "{phase:?}");
        // The quarantine record is what is left of the deploy.
        assert_eq!(host.quarantined().0, format!("tx@{phase:?}"));
        assert_eq!(host.last_error(), None, "{phase:?}");
    }
}

#[test]
fn one_start_ends_a_rollback_killed_before_its_swap_and_restores_the_active_record() {
    for phase in [
        DeployState::Prepared,
        DeployState::Warmed,
        DeployState::Promoted,
    ] {
        let host = host();
        let (agent, _) = host.rolling_back(Worker::default());
        agent.rollback_up_to(phase);
        assert_eq!(host.view(), ["tts@2", "stt@1", "stt@3", "4"], "{phase:?}");

        let (action, next) = host.one_start(Worker::default());

        assert_eq!(action, RecoveryAction::RestoreActive, "{phase:?}");
        let asked = next.asked().reference;
        assert_eq!(
            (asked.deployment_id.as_str(), asked.generation),
            ("tts", Some(4))
        );
        assert_eq!(host.view(), ["tts@4", "stt@1", "-", "5"], "{phase:?}");
        assert_eq!(host.quarantined().0, format!("tx@{phase:?}"));
        assert_eq!(host.last_error(), None, "{phase:?}");
    }
}

#[test]
fn one_start_never_runs_a_rollback_again_that_was_killed_before_it_asked_for_its_worker() {
    let host = host();
    let (agent, _) = host.rolling_back(Worker::default());
    agent.in_flight(TransactionKind::Rollback, "tx", "stt");
    assert_eq!(host.view(), ["tts@2", "stt@1", "stt", "3"]);

    let (action, next) = host.one_start(Worker::default());

    assert_eq!(action, RecoveryAction::RestoreActive);
    assert_eq!(next.asked().reference.deployment_id, "tts");
    assert_eq!(host.view(), ["tts@3", "stt@1", "-", "4"]);
    assert_eq!(host.quarantined().0, "tx@CapacityChecked");
}

#[test]
fn one_start_restores_what_a_deploy_promoted_before_it_was_killed() {
    let host = host();
    killed_after(&host, DeployState::Promoted);
    let store = StateStore::open(host.state_dir()).unwrap();
    store.promote_candidate(2).unwrap();
    assert_eq!(host.view(), ["stt@1", "-", "stt@1", "2"]);

    let (action, next) = host.one_start(Worker::default());

    assert_eq!(action, RecoveryAction::RestoreActive);
    assert_eq!(next.asked().reference.generation, Some(2));
    assert_eq!(host.view(), ["stt@2", "-", "-", "3"]);
    // Nothing was ended as failed, so nothing is quarantined.
    assert_eq!(
        (host.quarantined().0.as_str(), host.last_error()),
        ("", None)
    );
}

#[test]
fn one_start_restores_what_a_rollback_swapped_before_it_was_killed() {
    let host = host();
    let (agent, first) = host.rolling_back(Worker::default());
    agent.rollback_up_to(DeployState::Promoted);
    agent.store.swap_active_with_previous(2).unwrap();

    let (action, next) = host.one_start(Worker::default());

    assert_eq!(action, RecoveryAction::RestoreActive);
    assert_eq!(next.asked().resolved, Some(first.restamp(4).unwrap()));
    assert_eq!(host.view(), ["stt@4", "tts@2", "-", "5"]);
    assert_eq!(
        (host.quarantined().0.as_str(), host.last_error()),
        ("", None)
    );
}

#[test]
fn a_start_restores_the_active_record_unless_the_serving_member_was_started_from_it() {
    for case in ["its own", "another bundle", "another deployment"] {
        let host = host();
        let (agent, _) = host.restoring(Worker::default());
        let active = agent.store.snapshot().unwrap().active.unwrap();
        drop(agent);
        let mut serving = ServingMember {
            member: MemberRef::new("stt", 1),
            bundle_digest: active.bundle_digest,
            descriptor_digest: active.descriptor_digest,
            configuration_digest: active.configuration_digest,
            serving_url: "http://127.0.0.1:18080".into(),
            stream: None,
        };
        match case {
            // As an update of `stt` under its own id leaves a worker.
            "another bundle" => serving.bundle_digest = format!("sha256:{}", "0".repeat(64)),
            "another deployment" => serving.member = MemberRef::new("tts", 1),
            _ => {}
        }

        let (action, next) = host.one_start(Worker::holding(serving));

        if case == "its own" {
            assert_eq!(action, RecoveryAction::NoOp, "{case}");
            assert!(next.never_asked(), "{case}");
            assert_eq!(host.view(), ["stt@1", "-", "-", "2"], "{case}");
        } else {
            assert_eq!(action, RecoveryAction::RestoreActive, "{case}");
            assert_eq!(next.asked().reference.deployment_id, "stt", "{case}");
            assert_eq!(host.view(), ["stt@2", "-", "-", "3"], "{case}");
        }
    }
}

#[test]
fn a_promotion_is_not_read_from_a_deployment_id_the_active_record_shares_with_its_update() {
    // An update of `stt` under its own id, killed before its commit, found
    // by a worker control that reports `stt` serving.
    let host = host();
    let (agent, _) = host.restoring(Worker::default());
    let staged = host.staged("stt").display().to_string();
    let candidate = DeploymentRecord {
        generation: None,
        descriptor_digest: None,
        configuration_digest: None,
        promoted_monotonic_ns: None,
        ..record(&derive("stt", 1, &staged))
    };
    agent.in_flight(TransactionKind::Deploy, "tx", "stt");
    let recorded = |state: &mut AgentState| {
        state.candidate = Some(candidate);
        Ok(())
    };
    agent.store.update(recorded).unwrap();
    agent
        .store
        .record_phase("tx", DeployState::Warmed, 1)
        .unwrap();

    let (action, _) = host.one_start(Worker::serving("stt"));

    assert_eq!(action, RecoveryAction::QuarantineCandidate);
    assert!(host.settled());
    assert_eq!(host.quarantined().0, "tx@Warmed");
}

#[test]
fn a_swap_is_not_read_from_a_deployment_id_both_records_share() {
    // A rollback between two records of `stt`, killed before its swap,
    // found by a worker control that reports `stt` serving.
    let host = host();
    let staged = host.staged("stt").display().to_string();
    let (agent, _) = host.restoring(Worker::default());
    let retire = |state: &mut AgentState| {
        state.previous_active = state.active.take();
        Ok(())
    };
    agent.store.update(retire).unwrap();
    deploy(&agent.store, &host.descriptors, "stt", &staged).unwrap();
    agent.rollback_up_to(DeployState::Warmed);
    assert_eq!(host.view(), ["stt@2", "stt@1", "stt@3", "4"]);

    let (action, _) = host.one_start(Worker::serving("stt"));

    assert_eq!(action, RecoveryAction::QuarantineCandidate);
    assert_eq!(host.quarantined().0, "tx@Warmed");
    let [active, previous, transaction, _] = host.view();
    assert!(active.starts_with("stt@") && active != "stt@1" && active != "stt@3");
    assert_eq!((previous.as_str(), transaction.as_str()), ("stt@1", "-"));
}

#[test]
fn a_candidate_record_left_without_its_deploy_is_cleared_by_the_start_that_finds_it() {
    // What the start finds beside the candidate record, which only a file
    // of an earlier build holds without a deploy in flight; whether the
    // pass has a settling write to clear it in; the record it restores.
    let cases = [
        ("nothing", false, None),
        ("an active record", false, Some("tts")),
        ("a committed rollback", true, Some("stt")),
        ("a rollback recorded failed", true, Some("tts")),
    ];
    for (case, settles, restored) in cases {
        let host = host();
        let agent = match case {
            "nothing" => host.start(Worker::default()).0,
            "an active record" => {
                let (agent, _) = host.start(Worker::default());
                let staged = host.staged("tts").display().to_string();
                deploy(&agent.store, &host.descriptors, "tts", &staged).unwrap();
                agent
            }
            _ => {
                let (agent, _) = host.rolling_back(Worker::default());
                if case == "a committed rollback" {
                    agent.rollback_up_to(DeployState::Warmed);
                    agent.store.swap_active_with_previous(2).unwrap();
                } else {
                    agent.rollback_up_to(DeployState::Failed);
                }
                agent
            }
        };
        let staged = host.staged("left").display().to_string();
        let left = DeploymentRecord {
            generation: None,
            descriptor_digest: None,
            configuration_digest: None,
            promoted_monotonic_ns: None,
            ..record(&derive("left", 1, &staged))
        };
        let leave = |state: &mut AgentState| {
            state.candidate = Some(left);
            Ok(())
        };
        agent.store.update(leave).unwrap();
        let version = |host: &Host| {
            let state = StateStore::open(host.state_dir()).unwrap();
            state.snapshot().unwrap().store_version
        };
        let found = version(&host);
        // The settling write when the pass has one, or one of its own; a
        // restore then names its generation and clears the error.
        let writes = 1 + if restored.is_some() { 2 } else { 0 };
        drop(agent);

        let (_, first) = host.one_start(Worker::default());

        assert!(host.settled(), "{case}");
        assert_eq!(version(&host), found + writes, "{case}: settles {settles}");
        match restored {
            Some(id) => assert_eq!(first.asked().reference.deployment_id, id, "{case}"),
            None => assert!(first.never_asked(), "{case}"),
        }
        assert!(host.quarantined().0.is_empty(), "{case}");

        // The second start finds no candidate record: it writes only what
        // a restore writes.
        let settled = version(&host);
        let (_, second) = host.one_start(Worker::default());
        assert_eq!(version(&host), settled + writes - 1, "{case}");
        assert_eq!(second.never_asked(), restored.is_none(), "{case}");
        assert!(host.settled(), "{case}");
    }
}
