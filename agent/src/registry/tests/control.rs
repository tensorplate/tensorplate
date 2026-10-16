// SPDX-License-Identifier: Apache-2.0

//! The serving member fenced before a commit and members activated after
//! one, over each member's control channel.

use super::*;

const FENCE: WorkerOp = WorkerOp::AdmissionFence;
const ACTIVATE: WorkerOp = WorkerOp::Activate;
const RETIRE: WorkerOp = WorkerOp::Retire;

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

/// The transaction a member's fence names, and whether its activate is owed.
fn fence_of(control: &ControlState) -> Option<(String, bool)> {
    let fenced_by = control.fenced_by.lock().unwrap();
    let fence = fenced_by.as_ref();
    fence.map(|fence| (fence.transaction_id.clone(), fence.reopen_at.is_some()))
}

/// `stt` serving as generation 1, fenced by `tx-2` and owed its activate
/// after that transaction failed and its first activate was not answered.
pub(super) fn owed_a_reopen(f: &Fixture) -> Arc<Handle> {
    let serving = f.serve("stt");
    ready_candidate(f, "tts");
    f.registry.fence("tx-2", &candidate("tts")).unwrap();
    serving.reply(Reply::Unanswered);
    f.registry.abandon(&candidate("tts"));
    serving.reply(Reply::Answered);
    assert_eq!(fence_of(&control_of(f, 1)), Some(("tx-2".into(), true)));
    serving
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
    let retired = [
        sent(ACTIVATE, "tx"),
        sent(FENCE, "tx-2"),
        sent(RETIRE, "tx-2"),
    ];
    assert_eq!(serving.ops(), retired);
    assert!(serving.calls().is_empty());
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

    assert_eq!(fence_of(&control), Some(("tx-2".into(), false)));
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
fn the_replaced_member_is_sent_its_retire_before_the_new_one_is_activated() {
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
        assert_eq!(serving.ops().last(), Some(&sent(RETIRE, "tx-2")));
        answer.send(()).unwrap();
        activate.join().unwrap().unwrap();
    });

    assert_eq!(incoming.ops(), [sent(ACTIVATE, "tx-2")]);
    assert!(serving.calls().is_empty());
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
fn a_reopen_not_answered_ok_is_sent_again_from_the_supervision_step_until_it_is() {
    for case in ["unanswered", "broken", "loading"] {
        let f = fixture();
        let serving = f.serve("stt");
        ready_candidate(&f, "tts");
        f.registry.fence("tx-2", &candidate("tts")).unwrap();
        let refuse = |refused: bool| match case {
            "unanswered" if refused => serving.reply(Reply::Unanswered),
            "broken" if refused => serving.reply(Reply::Broken),
            "loading" => serving.target.lock().unwrap().ceiling = (!refused).then_some(0),
            _ => serving.reply(Reply::Answered),
        };
        refuse(true);

        f.registry.abandon(&candidate("tts"));
        step(&f);
        activations_answered(&f);
        assert_eq!(serving.admission(), Admission::Fenced, "{case}");
        refuse(false);
        step(&f);
        activations_answered(&f);
        step(&f);
        activations_answered(&f);

        assert_eq!(serving.admission(), Admission::Open, "{case}");
        assert_eq!(fence_of(&control_of(&f, 1)), None, "{case}");
        let reopened = [
            sent(ACTIVATE, "tx"),
            sent(FENCE, "tx-2"),
            sent(ACTIVATE, "tx-2"),
            sent(ACTIVATE, "tx-2"),
            sent(ACTIVATE, "tx-2"),
        ];
        assert_eq!(serving.ops(), reopened, "{case}");
        assert_eq!(quota_of(&f, 1), Some(MemberTarget::quota(0)), "{case}");
    }
}

#[test]
fn a_reopen_answered_unsupported_is_not_sent_again() {
    for case in ["at once", "when sent again"] {
        let f = fixture();
        let serving = f.serve("stt");
        ready_candidate(&f, "tts");
        let unimplemented = || serving.target.lock().unwrap().unimplemented = true;
        if case == "at once" {
            unimplemented();
            f.registry.fence("tx-2", &candidate("tts")).unwrap_err();
        } else {
            f.registry.fence("tx-2", &candidate("tts")).unwrap();
            serving.reply(Reply::Unanswered);
        }

        f.registry.abandon(&candidate("tts"));
        if case == "when sent again" {
            unimplemented();
            serving.reply(Reply::Answered);
            step(&f);
            activations_answered(&f);
        }
        step(&f);
        activations_answered(&f);

        assert_eq!(fence_of(&control_of(&f, 1)), None, "{case}");
        let reopens = serving
            .ops()
            .into_iter()
            .filter(|op| op.0 == ACTIVATE)
            .count();
        assert_eq!(reopens, if case == "at once" { 2 } else { 3 }, "{case}");
    }
}

#[test]
fn a_reopen_is_sent_again_at_most_once_a_poll_interval() {
    let f = fixture_with(
        |settings| RegistrySettings {
            status_poll_interval: Duration::from_secs(5),
            ..settings
        },
        |registry| registry,
    );
    let serving = owed_a_reopen(&f);
    serving.reply(Reply::Unanswered);
    let reopen = sent(ACTIVATE, "tx-2");
    let reopens = || serving.ops().iter().filter(|op| **op == reopen).count();

    f.registry.supervise();
    activations_answered(&f);
    f.registry.supervise();
    activations_answered(&f);
    assert_eq!(reopens(), 2);
    f.clock.advance(Duration::from_secs(4));
    f.registry.supervise();
    activations_answered(&f);
    assert_eq!(reopens(), 2);
    f.clock.advance(Duration::from_secs(1));
    f.registry.supervise();
    activations_answered(&f);
    assert_eq!(reopens(), 3);
}

#[test]
fn a_transaction_that_fences_a_member_owed_a_reopen_takes_it_over() {
    let f = fixture();
    let serving = owed_a_reopen(&f);
    ready_candidate(&f, "tts");
    serving.target.lock().unwrap().ceiling = None;
    let (asked, answer) = serving.hold_call();
    let control = control_of(&f, 1);

    step(&f);
    asked.recv_timeout(GRACE).unwrap();
    std::thread::scope(|scope| {
        let answer = answer;
        let fence = scope.spawn(|| f.registry.fence("tx-4", &candidate("tts")));
        assert!(control.fenced_by.try_lock().is_err());
        answer.send(()).unwrap();
        fence.join().unwrap().unwrap();
    });
    activations_answered(&f);
    assert_eq!(fence_of(&control), Some(("tx-4".into(), false)));
    serving.target.lock().unwrap().ceiling = Some(0);
    step(&f);
    activations_answered(&f);
    f.registry.activate("tx-4", &candidate("tts")).unwrap();
    step(&f);
    activations_answered(&f);

    let ops = serving.ops();
    let taken_over = [
        sent(ACTIVATE, "tx-2"),
        sent(FENCE, "tx-4"),
        sent(RETIRE, "tx-4"),
    ];
    assert_eq!(ops[ops.len() - 3..], taken_over, "{ops:?}");
    let served = f.registry.active_deployment_id().unwrap();
    assert_eq!(served.as_deref(), Some("tts"));
}

#[test]
fn a_reopen_sent_again_once_another_transaction_fenced_the_member_sends_nothing() {
    let f = fixture();
    let serving = owed_a_reopen(&f);
    ready_candidate(&f, "tts");
    let reach = {
        let members = f.registry.members().unwrap();
        Reach::of(&members.by_generation[&1], "serving")
    };
    f.registry.fence("tx-4", &candidate("tts")).unwrap();

    reach.retry_reopen();

    assert_eq!(serving.ops().last(), Some(&sent(FENCE, "tx-4")));
    assert_eq!(serving.admission(), Admission::Fenced);
}

#[test]
fn the_supervision_step_does_not_wait_for_an_operation_being_sent() {
    let f = fixture();
    let serving = owed_a_reopen(&f);
    ready_candidate(&f, "tts");
    let (asked, answer) = serving.hold_call();

    std::thread::scope(|scope| {
        let answer = answer;
        let fence = scope.spawn(|| f.registry.fence("tx-4", &candidate("tts")));
        asked.recv_timeout(GRACE).unwrap();
        step(&f);
        answer.send(()).unwrap();
        fence.join().unwrap().unwrap();
    });
}

#[test]
fn a_member_owed_a_reopen_that_exits_is_sent_it_no_more() {
    let f = fixture();
    let serving = owed_a_reopen(&f);
    serving.exited.store(true, Ordering::SeqCst);

    step(&f);
    step(&f);
    activations_answered(&f);

    let ops = serving.ops();
    assert_eq!(ops.last(), Some(&sent(ACTIVATE, "tx-2")), "{ops:?}");
    assert_eq!(ops.len(), 3, "{ops:?}");
}

#[test]
fn a_loading_restart_a_transaction_fenced_is_reopened_by_the_step_once_its_worker_admits() {
    let f = fixture();
    let successor = restarted(&f);
    successor.target.lock().unwrap().ceiling = None;
    ready_candidate(&f, "tts");
    f.registry.fence("tx-3", &candidate("tts")).unwrap();

    // Refused `not_ready`: the worker stays fenced, and so does the record,
    // which holds back the restart's own activate.
    f.registry.abandon(&candidate("tts"));
    assert_eq!(successor.admission(), Admission::Fenced);
    // Judged ready while its worker still refuses, so the record is still
    // there when the restart's activate reads it, whichever thread runs first.
    f.health(ACTIVE_PORT, "stt");
    step(&f);
    activations_answered(&f);
    assert_eq!(successor.admission(), Admission::Fenced);
    successor.target.lock().unwrap().ceiling = Some(0);
    step(&f);
    activations_answered(&f);

    assert_eq!(successor.admission(), Admission::Open);
    let reopened = [
        sent(FENCE, "tx-3"),
        sent(ACTIVATE, "tx-3"),
        sent(ACTIVATE, "tx-3"),
        sent(ACTIVATE, "tx-3"),
    ];
    assert_eq!(successor.ops(), reopened);
}

#[test]
fn an_activate_answered_error_clears_the_quota_and_one_not_answered_keeps_it() {
    for (case, kept) in [
        ("error", None),
        ("unanswered", Some(MemberTarget::quota(0))),
    ] {
        let f = fixture();
        let serving = f.serve("stt");
        assert_eq!(quota_of(&f, 1), Some(MemberTarget::quota(0)), "{case}");
        ready_candidate(&f, "tts");
        f.registry.fence("tx-2", &candidate("tts")).unwrap();
        match case {
            "error" => serving.target.lock().unwrap().admission = Admission::Retiring,
            _ => serving.reply(Reply::Unanswered),
        }

        f.registry.abandon(&candidate("tts"));

        assert_eq!(
            serving.ops().last(),
            Some(&sent(ACTIVATE, "tx-2")),
            "{case}"
        );
        assert_eq!(quota_of(&f, 1), kept, "{case}");
    }
}
