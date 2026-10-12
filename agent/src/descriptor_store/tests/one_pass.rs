// SPDX-License-Identifier: Apache-2.0

//! What ONE start of the agent does about the transaction it finds: every
//! start here has a worker control that holds no worker, as a new agent
//! process does, unless the test says otherwise.

use super::deploy::{killed_after, FORMAT_0_1, STT, TTS};
use super::start_again::{host, Agent, Host, Worker};
use super::*;

use tensorplate_protocol::agent_control::RecoveryAction;
use tensorplate_protocol::agent_state::TransactionKind;
use tensorplate_protocol::deploy_transaction::DeployState;

use crate::recovery::apply_startup;

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
    fn settled(&self) -> bool {
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

    assert_ne!(action, RecoveryAction::FinalizePromotion);
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

    assert_ne!(action, RecoveryAction::FinalizePromotion);
    assert_eq!(host.quarantined().0, "tx@Warmed");
    let [active, previous, transaction, _] = host.view();
    assert!(active.starts_with("stt@") && active != "stt@1" && active != "stt@3");
    assert_eq!((previous.as_str(), transaction.as_str()), ("stt@1", "-"));
}
