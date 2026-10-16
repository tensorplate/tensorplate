// SPDX-License-Identifier: Apache-2.0

//! A serving member taken out of service by a promotion or an unload: sent
//! a retire, reaped by the supervision step once it exits, and stopped when
//! it does not acknowledge the retire or outlasts its drain.

use super::control::owed_a_reopen;
use super::*;

const ACTIVATE: WorkerOp = WorkerOp::Activate;
const RETIRE: WorkerOp = WorkerOp::Retire;
const TICK: Duration = Duration::from_millis(1);

fn sent(op: WorkerOp, transaction_id: &str) -> (WorkerOp, String) {
    (op, transaction_id.to_owned())
}

/// `stt` serving as generation 1 and fenced by `tx-2` for `tts`, warmed as
/// generation 2; returns the serving member.
fn fenced_for_tts(f: &Fixture) -> Arc<Handle> {
    let serving = f.serve("stt");
    f.prepare("tts").unwrap();
    f.health(CANDIDATE_PORT, "tts");
    assert!(f.warm("tts").unwrap().ready);
    f.registry.fence("tx-2", &candidate("tts")).unwrap();
    serving
}

fn activate_tts(f: &Fixture) {
    f.registry.activate("tx-2", &candidate("tts")).unwrap();
}

fn step(f: &Fixture, after: Duration) -> Vec<MemberEvent> {
    f.clock.advance(after);
    f.registry.supervise()
}

fn roles(f: &Fixture) -> Vec<(MemberRef, MemberRole)> {
    let live = f.registry.live_members().unwrap();
    live.into_iter().map(|m| (m.member, m.role)).collect()
}

fn tts_serves() -> [(MemberRef, MemberRole); 1] {
    [(MemberRef::new("tts", 2), MemberRole::Serving)]
}

fn draining(f: &Fixture, generation: u64) -> Option<LiveMember> {
    let live = f.registry.live_members().unwrap();
    live.into_iter()
        .find(|m| m.member.generation == generation && m.role == MemberRole::Draining)
}

/// Whether the answer to the retire of `generation` was recorded.
fn answered(f: &Fixture, generation: u64) -> bool {
    let members = f.registry.members().unwrap();
    let retiring = members.retiring.get(&generation).expect("retiring");
    !matches!(retiring.phase, Retirement::Sent { .. })
}

fn port_of(handle: &Handle) -> serde_json::Value {
    handle.config["bind"]["port"].clone()
}

#[test]
fn a_replacing_activate_queues_the_old_members_retire_before_the_candidates_activate() {
    let f = fixture();
    let old = fenced_for_tts(&f);
    let incoming = f.launcher.handle(2);
    let (parked, unpark) = park_next_thread(&f);
    let (activating, activate) = incoming.hold_call_of(ACTIVATE);

    std::thread::scope(|scope| {
        let (unpark, activate) = (unpark, activate);
        let promoting = scope.spawn(|| activate_tts(&f));
        activating.recv_timeout(GRACE).unwrap();
        assert_eq!(old.ops().last(), Some(&sent(RETIRE, "tx-2")));
        activate.send(()).unwrap();
        promoting.join().unwrap();
        parked.recv_timeout(GRACE).unwrap();
        unpark.send(()).unwrap();
    });
    retires_answered(&f);

    assert_eq!(incoming.ops(), [sent(ACTIVATE, "tx-2")]);
    let retire = old.requests().pop().unwrap();
    assert_eq!(retire.drain_timeout_ms, Some(30_000));
    assert!(old.calls().is_empty());
}

#[test]
fn the_candidate_is_activated_while_the_old_member_has_not_answered_its_retire() {
    let f = fixture();
    let old = fenced_for_tts(&f);
    let (asked, answer) = old.hold_call_of(RETIRE);

    activate_tts(&f);
    asked.recv_timeout(GRACE).unwrap();

    assert_eq!(f.launcher.handle(2).ops(), [sent(ACTIVATE, "tx-2")]);
    assert!(!answered(&f, 1));
    answer.send(()).unwrap();
    retires_answered(&f);
    assert!(answered(&f, 1));
}

#[test]
fn an_activate_returns_while_the_old_member_drains_and_a_step_reaps_it_once_it_exits() {
    let f = fixture();
    let old = fenced_for_tts(&f);
    old.keeps_draining.store(true, Ordering::SeqCst);

    activate_tts(&f);
    retires_answered(&f);

    let listed = draining(&f, 1).expect("the old member is listed draining");
    assert_eq!(listed.http_port, ACTIVE_PORT);
    step(&f, Duration::ZERO);
    assert!(draining(&f, 1).is_some());
    assert!(old.spec.config_path.exists());

    old.exited.store(true, Ordering::SeqCst);
    step(&f, Duration::ZERO);

    assert_eq!(roles(&f), tts_serves());
    assert!(!old.spec.config_path.exists());
    f.prepare("third").unwrap();
    assert_eq!(port_of(&f.launcher.handle(3)), ACTIVE_PORT);
    assert!(old.calls().is_empty());
}

/// Acknowledged, it is given the drain it was told and a cleanup after it,
/// then asked to stop, then killed once the stop deadline passes.
#[test]
fn a_retiring_member_that_outlasts_its_drain_is_asked_to_stop_and_then_killed() {
    let f = fixture();
    let old = fenced_for_tts(&f);
    old.keeps_draining.store(true, Ordering::SeqCst);
    old.obeys_terminate.store(false, Ordering::SeqCst);
    activate_tts(&f);
    retires_answered(&f);

    step(&f, Duration::from_secs(40) - TICK);
    assert!(old.calls().is_empty());
    step(&f, TICK);
    assert_eq!(old.calls(), ["terminate"]);
    step(&f, DRAIN - TICK);
    assert_eq!(old.calls(), ["terminate"]);
    assert!(draining(&f, 1).is_some());
    step(&f, TICK);

    assert_eq!(old.calls(), ["terminate", "kill"]);
    assert_eq!(roles(&f), tts_serves());
}

#[test]
fn a_retire_not_acknowledged_is_followed_by_a_stop_at_once_and_a_kill_at_the_deadline() {
    for case in ["unanswered", "broken", "unsupported"] {
        let f = fixture();
        let old = fenced_for_tts(&f);
        old.obeys_terminate.store(false, Ordering::SeqCst);
        match case {
            "unanswered" => old.reply(Reply::Unanswered),
            "broken" => old.reply(Reply::Broken),
            _ => old.target.lock().unwrap().unimplemented = true,
        }

        activate_tts(&f);
        retires_answered(&f);

        assert_eq!(old.ops().last(), Some(&sent(RETIRE, "tx-2")), "{case}");
        assert_eq!(old.calls(), ["terminate"], "{case}");
        let incoming = f.launcher.handle(2).ops();
        assert_eq!(incoming, [sent(ACTIVATE, "tx-2")], "{case}");
        step(&f, DRAIN - TICK);
        assert_eq!(old.calls(), ["terminate"], "{case}");
        assert!(draining(&f, 1).is_some(), "{case}");
        step(&f, TICK);
        assert_eq!(old.calls(), ["terminate", "kill"], "{case}");
        assert_eq!(roles(&f), tts_serves(), "{case}");
    }
}

/// Retired by a promotion or by an unload; the thread that waits for the
/// answer is held before it waits.
#[test]
fn a_member_whose_retire_answer_is_never_recorded_is_asked_to_stop_and_then_killed() {
    for unloaded in [false, true] {
        let f = fixture();
        let old = if unloaded {
            f.serve("stt")
        } else {
            fenced_for_tts(&f)
        };
        old.obeys_terminate.store(false, Ordering::SeqCst);
        let (parked, unpark) = park_next_thread(&f);

        if unloaded {
            f.registry.unload("stt");
        } else {
            activate_tts(&f);
        }
        parked.recv_timeout(GRACE).unwrap();

        step(&f, CONTROL_BUDGET + DRAIN - TICK);
        assert!(old.calls().is_empty(), "unloaded {unloaded}");
        step(&f, TICK);
        assert_eq!(old.calls(), ["terminate"], "unloaded {unloaded}");
        step(&f, DRAIN);
        assert_eq!(old.calls(), ["terminate", "kill"], "unloaded {unloaded}");
        assert!(draining(&f, 1).is_none(), "unloaded {unloaded}");
        unpark.send(()).unwrap();
        retires_answered(&f);
        assert_eq!(old.calls(), ["terminate", "kill"], "unloaded {unloaded}");
    }
}

#[test]
fn shutdown_stops_a_member_still_draining_after_its_retire() {
    for stubborn in [false, true] {
        let f = fixture();
        let old = fenced_for_tts(&f);
        old.keeps_draining.store(true, Ordering::SeqCst);
        old.obeys_terminate.store(!stubborn, Ordering::SeqCst);
        activate_tts(&f);
        retires_answered(&f);

        f.registry.shutdown();

        let stopped: &[&str] = if stubborn {
            &["terminate", "kill"]
        } else {
            &["terminate"]
        };
        assert_eq!(old.calls(), stopped, "stubborn {stubborn}");
        assert_eq!(roles(&f), [], "stubborn {stubborn}");
        assert!(!old.spec.config_path.exists(), "stubborn {stubborn}");
    }
}

#[test]
fn shutdown_while_a_retire_is_being_sent_stops_the_member_without_waiting_for_the_answer() {
    let f = fixture();
    let old = fenced_for_tts(&f);
    let (asked, answer) = old.hold_call_of(RETIRE);

    activate_tts(&f);
    asked.recv_timeout(GRACE).unwrap();
    f.registry.shutdown();
    assert_eq!(old.calls(), ["terminate"]);
    assert_eq!(roles(&f), []);
    answer.send(()).unwrap();
    retires_answered(&f);

    assert_eq!(old.calls(), ["terminate"]);
    assert!(f.registry.members().unwrap().retiring.is_empty());
    assert_eq!(f.launcher.handle(2).calls(), ["terminate"]);
}

#[test]
fn unload_sends_the_serving_member_a_retire_of_its_own_and_stops_its_candidate() {
    let f = fixture();
    let serving = f.serve("stt");
    serving.keeps_draining.store(true, Ordering::SeqCst);
    f.prepare("stt").unwrap();
    let prepared = f.launcher.handle(2);

    f.registry.unload("stt");

    let (op, transaction_id) = serving.ops().pop().unwrap();
    assert_eq!(op, RETIRE);
    assert!(transaction_id.starts_with("tx-"), "{transaction_id}");
    assert!(serving.calls().is_empty());
    assert_eq!(prepared.calls(), ["terminate"]);
    assert!(prepared.ops().is_empty());
    let left = [(MemberRef::new("stt", 1), MemberRole::Draining)];
    assert_eq!(roles(&f), left);
    assert_eq!(f.registry.active_deployment_id().unwrap(), None);
}

/// Parks the next thread an operation is sent from or its answer waited
/// for on: a supervision step's activate, or a retire's answer; returns
/// what tells that it is parked and what lets it go on.
fn park_next_thread(f: &Fixture) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (parked, is_parked) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    *f.registry.before_thread_sends.lock().unwrap() = Some(Box::new(move || {
        let _ = parked.send(());
        let _ = released.recv_timeout(GRACE);
    }));
    (is_parked, release)
}

fn activated_after_its_retire(handle: &Handle) -> bool {
    let ops = handle.ops();
    let retired = ops.iter().position(|(op, _)| *op == RETIRE);
    let after = &ops[retired.expect("the member is retired")..];
    after.iter().any(|(op, _)| *op == ACTIVATE)
}

/// The reopen a failed transaction owes the member is already on its way
/// from the supervision step when the member is unloaded.
#[test]
fn an_unloaded_member_owed_a_reopen_is_not_sent_it_after_its_retire() {
    let f = fixture();
    let serving = owed_a_reopen(&f);
    serving.keeps_draining.store(true, Ordering::SeqCst);
    let (is_parked, release) = park_next_thread(&f);

    step(&f, Duration::from_secs(1));
    is_parked
        .recv_timeout(GRACE)
        .expect("the reopen is sent from a thread");
    f.registry.unload("stt");
    release.send(()).unwrap();
    activations_answered(&f);

    assert!(!activated_after_its_retire(&serving), "{:?}", serving.ops());
}

/// The activate the supervision step sends a restarted generation it
/// judged ready is already on its way when the generation is unloaded.
#[test]
fn an_unloaded_restart_is_not_sent_its_activate_after_its_retire() {
    let f = fixture();
    f.serve("stt").exited.store(true, Ordering::SeqCst);
    f.registry.supervise();
    let restarted = [MemberEvent::Restarted {
        member: MemberRef::new("stt", 2),
    }];
    assert_eq!(step(&f, Duration::from_secs(1)), restarted);
    let successor = f.launcher.handle(2);
    successor.keeps_draining.store(true, Ordering::SeqCst);
    let (is_parked, release) = park_next_thread(&f);

    let serving = [MemberEvent::Serving {
        member: MemberRef::new("stt", 2),
    }];
    assert_eq!(step(&f, Duration::ZERO), serving);
    is_parked
        .recv_timeout(GRACE)
        .expect("the activate is sent from a thread");
    f.registry.unload("stt");
    release.send(()).unwrap();
    activations_answered(&f);

    assert!(
        !activated_after_its_retire(&successor),
        "{:?}",
        successor.ops()
    );
}
