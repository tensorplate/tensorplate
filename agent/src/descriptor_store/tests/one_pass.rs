// SPDX-License-Identifier: Apache-2.0

//! What ONE start of the agent does about the transaction it finds: every
//! start here has a worker control that holds no worker, as a new agent
//! process does, unless the test says otherwise.

use super::deploy::killed_after;
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
    assert_eq!(host.last_error(), None);
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

#[test]
fn a_start_whose_restore_is_refused_by_what_is_on_disk_leaves_the_agent_up_with_the_error() {
    type Broken = fn(&Host);
    let cases: [(&str, Broken); 4] = [
        ("the staged directory is gone", |host| {
            fs::remove_dir_all(host.staged("stt")).unwrap();
        }),
        ("the staged manifest is gone", |host| {
            fs::remove_file(host.staged("stt").join("manifest.json")).unwrap();
        }),
        ("the staged manifest was half written", |host| {
            let manifest = host.staged("stt").join("manifest.json");
            let whole = fs::read(&manifest).unwrap();
            fs::write(&manifest, &whole[..whole.len() / 2]).unwrap();
        }),
        ("the descriptor document is gone", |host| {
            fs::remove_file(host.state_dir().join("descriptor-stt-1.json")).unwrap();
        }),
    ];
    for (case, broken) in cases {
        let host = host();
        drop(host.restoring(Worker::default()));
        broken(&host);

        let (next, _) = host.start(Worker::default());
        let started = apply_startup(&next.coordinator);

        assert!(started.is_ok(), "{case}: {started:?}");
        assert!(next.never_asked(), "{case}");
        assert!(host.last_error().is_some(), "{case}");
        // No generation is taken for a restore that starts nothing.
        assert_eq!(host.view()[3], "2", "{case}");
    }
}
