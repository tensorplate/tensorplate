// SPDX-License-Identifier: Apache-2.0

//! The serving member fenced before a commit and members activated after
//! one, over each member's control channel.

use super::*;

const FENCE: WorkerOp = WorkerOp::AdmissionFence;
const ACTIVATE: WorkerOp = WorkerOp::Activate;

fn sent(op: WorkerOp, transaction_id: &str) -> (WorkerOp, String) {
    (op, transaction_id.to_owned())
}

fn ready_candidate(f: &Fixture, id: &str) {
    f.prepare(id).unwrap();
    f.health(CANDIDATE_PORT, id);
    assert!(f.warm(id).unwrap().ready);
}

fn roles(f: &Fixture) -> Vec<(MemberRef, MemberRole)> {
    let live = f.registry.live_members().unwrap();
    live.into_iter().map(|m| (m.member, m.role)).collect()
}

fn quota_of(f: &Fixture, generation: u64) -> Option<MemberQuota> {
    let live = f.registry.live_members().unwrap();
    let held = live.into_iter().find(|m| m.member.generation == generation);
    held.and_then(|m| m.effective_quota)
}

fn control_of(f: &Fixture, generation: u64) -> Arc<ControlState> {
    let members = f.registry.members().unwrap();
    members.by_generation[&generation].control.clone()
}

fn step(f: &Fixture) -> Vec<MemberEvent> {
    f.clock.advance(Duration::from_secs(1));
    f.registry.supervise()
}

/// `stt` serving as generation 1, then stopped and restarted as generation
/// 2, which is loading.
fn restarted(f: &Fixture) -> Arc<Handle> {
    let first = f.serve("stt");
    first.exited.store(true, Ordering::SeqCst);
    f.registry.supervise();
    f.probe.models.lock().unwrap().remove(&ACTIVE_PORT);
    let started = [MemberEvent::Restarted {
        member: MemberRef::new("stt", 2),
    }];
    assert_eq!(step(f), started);
    f.launcher.handle(2)
}

#[test]
fn a_replacing_promotion_fences_the_serving_member_and_activates_the_candidate_after_the_commit() {
    let f = fixture();
    let serving = f.serve("stt");
    assert_eq!(serving.ops(), [sent(ACTIVATE, "tx")]);
    assert_eq!(quota_of(&f, 1), Some(MemberTarget::quota(0)));
    ready_candidate(&f, "tts");
    let incoming = f.launcher.handle(2);

    f.registry.fence("tx-2", &candidate("tts")).unwrap();

    assert_eq!(serving.ops(), [sent(ACTIVATE, "tx"), sent(FENCE, "tx-2")]);
    assert_eq!(serving.admission(), Admission::Fenced);
    assert!(serving.calls().is_empty());
    assert!(incoming.ops().is_empty());

    f.registry.activate("tx-2", &candidate("tts")).unwrap();

    assert_eq!(incoming.ops(), [sent(ACTIVATE, "tx-2")]);
    assert_eq!(quota_of(&f, 2), Some(MemberTarget::quota(0)));
    assert_eq!(serving.calls(), ["terminate"]);
}

#[test]
fn a_candidate_refused_at_its_fence_leaves_the_serving_member_unfenced() {
    let f = fixture();
    let serving = f.serve("stt");
    ready_candidate(&f, "tts");
    f.launcher.handle(2).exited.store(true, Ordering::SeqCst);

    let refused = f.registry.fence("tx-2", &candidate("tts"));

    assert!(
        matches!(refused, Err(AgentError::WorkerExited(_))),
        "{refused:?}"
    );
    assert_eq!(serving.ops(), [sent(ACTIVATE, "tx")]);
}

#[test]
fn a_fence_the_serving_member_does_not_answer_ok_is_refused_and_moves_nothing() {
    for case in ["unanswered", "unsupported", "broken"] {
        let f = fixture();
        let serving = f.serve("stt");
        ready_candidate(&f, "tts");
        match case {
            "unanswered" => serving.reply(Reply::Unanswered),
            "unsupported" => serving.target.lock().unwrap().unimplemented = true,
            _ => serving.reply(Reply::Broken),
        }

        let refused = f.registry.fence("tx-2", &candidate("tts")).unwrap_err();

        let says = match &refused {
            AgentError::WorkerTimeout(ms) => case == "unanswered" && *ms == 3_000,
            AgentError::WorkerControl(s) if case == "unsupported" => {
                s.contains("refused the admission fence: unsupported")
            }
            AgentError::WorkerControl(s) => {
                s.ends_with("admission fence: worker control peer closed")
            }
            _ => false,
        };
        assert!(says, "{case}: {refused:?}");
        let held = [
            (MemberRef::new("stt", 1), MemberRole::Serving),
            (MemberRef::new("tts", 2), MemberRole::Candidate),
        ];
        assert_eq!(roles(&f), held, "{case}");
        assert!(serving.calls().is_empty(), "{case}");
        assert_eq!(
            f.registry.active_deployment_id().unwrap().as_deref(),
            Some("stt")
        );
        assert_eq!(serving.ops().last(), Some(&sent(FENCE, "tx-2")), "{case}");
    }
}

#[test]
fn a_serving_member_that_exited_or_lost_its_channel_is_not_fenced_and_an_uninspectable_one_is() {
    for case in ["exited", "channel failed", "uninspectable"] {
        let f = fixture();
        let serving = f.serve("stt");
        ready_candidate(&f, "tts");
        match case {
            "exited" => serving.exited.store(true, Ordering::SeqCst),
            "channel failed" => serving.set_contact(ControlContact::Failed),
            _ => serving.uninspectable.store(true, Ordering::SeqCst),
        }

        f.registry.fence("tx-2", &candidate("tts")).unwrap();

        let fenced = serving.ops().contains(&sent(FENCE, "tx-2"));
        assert_eq!(fenced, case == "uninspectable", "{case}");
    }
}

#[test]
fn a_serving_member_that_exits_or_loses_its_channel_while_it_is_fenced_is_not_refused() {
    for case in ["exited", "channel failed"] {
        let f = fixture();
        let serving = f.serve("stt");
        ready_candidate(&f, "tts");
        serving.reply(Reply::Broken);
        let (asked, answer) = serving.hold_call();

        let fenced = std::thread::scope(|scope| {
            let answer = answer;
            let fence = scope.spawn(|| f.registry.fence("tx-2", &candidate("tts")));
            asked.recv_timeout(GRACE).unwrap();
            match case {
                "exited" => serving.exited.store(true, Ordering::SeqCst),
                _ => serving.set_contact(ControlContact::Failed),
            }
            answer.send(()).unwrap();
            fence.join().unwrap()
        });

        assert!(fenced.is_ok(), "{case}: {fenced:?}");
    }
}

#[test]
fn the_fencing_transaction_holds_the_members_operations_until_its_fence_is_answered() {
    let f = fixture();
    let serving = f.serve("stt");
    ready_candidate(&f, "tts");
    let (asked, answer) = serving.hold_call();
    let control = control_of(&f, 1);

    std::thread::scope(|scope| {
        let answer = answer;
        let fence = scope.spawn(|| f.registry.fence("tx-2", &candidate("tts")));
        asked.recv_timeout(GRACE).unwrap();
        assert!(control.fenced_by.try_lock().is_err());
        answer.send(()).unwrap();
        fence.join().unwrap().unwrap();
    });

    assert_eq!(*control.fenced_by.lock().unwrap(), Some("tx-2".to_owned()));
}

#[test]
fn abandon_reopens_the_serving_member_its_transaction_fenced_once_and_nothing_else() {
    let f = fixture();
    let serving = f.serve("stt");
    ready_candidate(&f, "tts");
    f.registry.abandon(&candidate("tts"));
    assert_eq!(serving.ops(), [sent(ACTIVATE, "tx")]);

    ready_candidate(&f, "tts");
    f.registry.fence("tx-3", &candidate("tts")).unwrap();
    assert_eq!(serving.admission(), Admission::Fenced);
    f.registry.abandon(&candidate("tts"));
    f.registry.abandon(&candidate("tts"));

    let reopened = [
        sent(ACTIVATE, "tx"),
        sent(FENCE, "tx-3"),
        sent(ACTIVATE, "tx-3"),
    ];
    assert_eq!(serving.ops(), reopened);
    assert_eq!(serving.admission(), Admission::Open);
}

#[test]
fn an_activate_not_answered_ok_keeps_the_promotion_and_records_no_quota() {
    for case in ["unanswered", "broken", "unsupported", "loading"] {
        let f = fixture();
        f.prepare("stt").unwrap();
        f.health(ACTIVE_PORT, "stt");
        assert!(f.warm("stt").unwrap().ready);
        let incoming = f.launcher.handle(1);
        match case {
            "unanswered" => incoming.reply(Reply::Unanswered),
            "broken" => incoming.reply(Reply::Broken),
            "unsupported" => incoming.target.lock().unwrap().unimplemented = true,
            _ => incoming.target.lock().unwrap().ceiling = None,
        }

        f.registry.promote("tx", &candidate("stt")).unwrap();

        assert_eq!(incoming.ops(), [sent(ACTIVATE, "tx")], "{case}");
        let active = f.registry.active_deployment_id().unwrap();
        assert_eq!(active.as_deref(), Some("stt"), "{case}");
        assert_eq!(quota_of(&f, 1), None, "{case}");
    }
}

#[test]
fn a_restarted_member_judged_ready_is_activated_without_holding_the_supervision_step() {
    let f = fixture();
    let successor = restarted(&f);
    let (asked, answer) = successor.hold_call();
    let control = control_of(&f, 2);
    f.health(ACTIVE_PORT, "stt");

    let serving = [MemberEvent::Serving {
        member: MemberRef::new("stt", 2),
    }];
    assert_eq!(step(&f), serving);
    asked.recv_timeout(GRACE).unwrap();
    assert_eq!(quota_of(&f, 2), None);
    assert!(control.fenced_by.try_lock().is_err());
    answer.send(()).unwrap();
    activations_answered(&f);

    assert_eq!(quota_of(&f, 2), Some(MemberTarget::quota(0)));
    let [(op, transaction_id)] = successor.ops().try_into().unwrap();
    assert_eq!(op, ACTIVATE);
    assert!(transaction_id.starts_with("tx-"), "{transaction_id}");
}

#[test]
fn a_restarted_member_a_transaction_fenced_is_left_to_that_transaction_to_reopen() {
    let f = fixture();
    let successor = restarted(&f);
    ready_candidate(&f, "tts");
    f.registry.fence("tx-3", &candidate("tts")).unwrap();
    f.health(ACTIVE_PORT, "stt");

    step(&f);
    activations_answered(&f);

    assert_eq!(successor.ops(), [sent(FENCE, "tx-3")]);
    assert_eq!(successor.admission(), Admission::Fenced);
    f.registry.abandon(&candidate("tts"));
    let reopened = [sent(FENCE, "tx-3"), sent(ACTIVATE, "tx-3")];
    assert_eq!(successor.ops(), reopened);
    assert_eq!(successor.admission(), Admission::Open);
}

#[test]
fn the_replaced_member_is_asked_to_stop_before_the_new_one_is_activated() {
    let f = fixture();
    let serving = f.serve("stt");
    ready_candidate(&f, "tts");
    f.registry.fence("tx-2", &candidate("tts")).unwrap();
    let incoming = f.launcher.handle(2);
    let (asked, answer) = incoming.hold_call();

    std::thread::scope(|scope| {
        let answer = answer;
        let activate = scope.spawn(|| f.registry.activate("tx-2", &candidate("tts")));
        asked.recv_timeout(GRACE).unwrap();
        assert_eq!(serving.calls(), ["terminate"]);
        answer.send(()).unwrap();
        activate.join().unwrap().unwrap();
    });

    assert_eq!(incoming.ops(), [sent(ACTIVATE, "tx-2")]);
}

#[test]
fn a_fence_a_stop_cuts_short_fails_with_the_stop() {
    let f = fixture();
    let serving = f.serve("stt");
    ready_candidate(&f, "tts");
    serving.reply(Reply::Broken);
    let (asked, answer) = serving.hold_call();

    let fenced = std::thread::scope(|scope| {
        let answer = answer;
        let fence = scope.spawn(|| f.registry.fence("tx-2", &candidate("tts")));
        asked.recv_timeout(GRACE).unwrap();
        f.registry.shutdown();
        answer.send(()).unwrap();
        fence.join().unwrap()
    });

    assert!(matches!(fenced, Err(AgentError::Stopping)), "{fenced:?}");
}

#[test]
fn a_reopen_holds_the_members_operations_until_it_is_answered() {
    let f = fixture();
    let serving = f.serve("stt");
    ready_candidate(&f, "tts");
    f.registry.fence("tx-2", &candidate("tts")).unwrap();
    let (asked, answer) = serving.hold_call();
    let control = control_of(&f, 1);

    std::thread::scope(|scope| {
        let answer = answer;
        let abandon = scope.spawn(|| f.registry.abandon(&candidate("tts")));
        asked.recv_timeout(GRACE).unwrap();
        assert!(control.fenced_by.try_lock().is_err());
        answer.send(()).unwrap();
        abandon.join().unwrap();
    });

    assert_eq!(serving.admission(), Admission::Open);
}

#[test]
fn a_loading_restart_a_transaction_fenced_is_reopened_by_its_own_activate_once_ready() {
    let f = fixture();
    let successor = restarted(&f);
    successor.target.lock().unwrap().ceiling = None;
    ready_candidate(&f, "tts");
    f.registry.fence("tx-3", &candidate("tts")).unwrap();

    // Refused `not_ready`: the worker stays fenced, the record does not.
    f.registry.abandon(&candidate("tts"));
    assert_eq!(successor.admission(), Admission::Fenced);
    successor.target.lock().unwrap().ceiling = Some(0);
    f.health(ACTIVE_PORT, "stt");
    step(&f);
    activations_answered(&f);

    assert_eq!(successor.admission(), Admission::Open);
    let ops = successor.ops();
    assert_eq!(ops[..2], [sent(FENCE, "tx-3"), sent(ACTIVATE, "tx-3")]);
    assert_eq!(ops.len(), 3, "{ops:?}");
    assert_eq!(ops[2].0, ACTIVATE);
}

#[test]
fn an_activate_refused_after_one_answered_ok_keeps_the_quota_answered() {
    let f = fixture();
    let serving = f.serve("stt");
    ready_candidate(&f, "tts");
    f.registry.fence("tx-2", &candidate("tts")).unwrap();
    serving.target.lock().unwrap().admission = Admission::Retiring;

    f.registry.abandon(&candidate("tts"));

    assert_eq!(serving.ops().last(), Some(&sent(ACTIVATE, "tx-2")));
    assert_eq!(quota_of(&f, 1), Some(MemberTarget::quota(0)));
}
