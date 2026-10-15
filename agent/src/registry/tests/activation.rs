// SPDX-License-Identifier: Apache-2.0

//! A promotion's two halves on either side of the transaction's commit, and
//! a start's restore handed to the restarts.

use super::*;

fn ready_candidate(f: &Fixture, id: &str) {
    f.prepare(id).unwrap();
    f.health(CANDIDATE_PORT, id);
    assert!(f.warm(id).unwrap().ready);
}

fn roles(f: &Fixture) -> Vec<(MemberRef, MemberRole)> {
    let live = f.registry.live_members().unwrap();
    live.into_iter().map(|m| (m.member, m.role)).collect()
}

fn active(f: &Fixture) -> Option<String> {
    f.registry.active_deployment_id().unwrap()
}

fn step(f: &Fixture, after: Duration) -> Vec<MemberEvent> {
    f.clock.advance(after);
    f.registry.supervise()
}

fn restarted(id: &str, generation: u64) -> [MemberEvent; 1] {
    let member = MemberRef::new(id, generation);
    [MemberEvent::Restarted { member }]
}

fn failure() -> ErrorRecord {
    AgentError::WorkerNotReady.to_record()
}

#[test]
fn a_fenced_candidate_moves_nothing_until_it_is_activated() {
    let f = fixture();
    let serving = f.serve("stt");
    ready_candidate(&f, "tts");

    f.registry.fence("tx", &candidate("tts")).unwrap();

    assert!(serving.calls().is_empty());
    assert_eq!(active(&f).as_deref(), Some("stt"));
    let held = [
        (MemberRef::new("stt", 1), MemberRole::Serving),
        (MemberRef::new("tts", 2), MemberRole::Candidate),
    ];
    assert_eq!(roles(&f), held);

    f.registry.activate("tx", &candidate("tts")).unwrap();

    assert_eq!(serving.calls(), ["terminate"]);
    assert_eq!(active(&f).as_deref(), Some("tts"));
}

#[test]
fn a_candidate_that_exits_after_its_fence_takes_the_slot_and_is_restarted() {
    let f = fixture();
    let serving = f.serve("stt");
    ready_candidate(&f, "tts");
    f.registry.fence("tx", &candidate("tts")).unwrap();
    f.launcher.handle(2).exited.store(true, Ordering::SeqCst);

    f.registry.activate("tx", &candidate("tts")).unwrap();

    assert_eq!(serving.calls(), ["terminate"]);
    let stopped = step(&f, Duration::ZERO);
    let owed = stopped.iter().any(|event| {
        matches!(event, MemberEvent::RestartDue { deployment_id, .. } if deployment_id == "tts")
    });
    assert!(owed, "{stopped:?}");
    assert_eq!(step(&f, Duration::from_secs(1)), restarted("tts", 3));
}

#[test]
fn activate_is_refused_for_a_candidate_not_held_and_once_the_registry_stops() {
    let f = fixture();
    f.prepare("stt").unwrap();

    let other = f.registry.activate("tx", &candidate("tts"));

    assert!(
        matches!(other, Err(AgentError::WorkerNotReady)),
        "{other:?}"
    );
    let held = [(MemberRef::new("stt", 1), MemberRole::Candidate)];
    assert_eq!(roles(&f), held);
    f.registry.shutdown();
    let stopped = f.registry.activate("tx", &candidate("stt"));
    assert!(matches!(stopped, Err(AgentError::Stopping)), "{stopped:?}");
}

#[test]
fn a_restore_handed_over_is_started_again_on_the_ports_of_the_candidate_it_held() {
    let f = fixture();
    f.prepare("stt").unwrap();
    // Not ready in time: the warm gave it up.
    f.registry.abandon(&candidate("stt"));

    assert!(f.registry.owe_restore(&candidate("stt"), failure()));

    assert_eq!(f.launcher.handle(1).calls(), ["kill"]);
    assert_eq!(f.configs(), 0);
    assert_eq!(step(&f, Duration::from_millis(999)), []);
    assert_eq!(step(&f, Duration::from_millis(1)), restarted("stt", 2));
    let port = &f.launcher.handle(2).config["bind"]["port"];
    assert_eq!(*port, ACTIVE_PORT);
}

#[test]
fn a_restore_with_no_candidate_held_is_started_again_on_free_ports() {
    let f = fixture();

    assert!(f.registry.owe_restore(&candidate("stt"), failure()));

    assert_eq!(step(&f, Duration::from_secs(1)), restarted("stt", 1));
    let leased = f.registry.ports.lease().unwrap();
    assert_eq!(leased.ports.http, CANDIDATE_PORT);
    drop(leased);
    let without_a_port = fixture();
    let taken = [(); 2].map(|()| without_a_port.registry.ports.lease().unwrap());
    let restore = candidate("stt");
    assert!(!without_a_port.registry.owe_restore(&restore, failure()));
    drop(taken);
    assert_eq!(step(&without_a_port, Duration::from_secs(10)), []);
}

#[test]
fn no_restore_is_owed_while_a_member_serves_or_a_deploy_holds_the_registry() {
    let serves = fixture();
    let serving = serves.serve("stt");
    assert!(!serves.registry.owe_restore(&candidate("stt"), failure()));
    assert_eq!(step(&serves, Duration::from_secs(10)), []);
    assert!(serving.calls().is_empty());

    let held = fixture();
    held.registry.hold();
    assert!(!held.registry.owe_restore(&candidate("stt"), failure()));
    held.registry.release();
    assert_eq!(step(&held, Duration::from_secs(10)), []);
    assert_eq!(held.launcher.count(), 0);
}

#[test]
fn no_restore_is_owed_once_the_registry_stops_or_while_a_restart_is_owed() {
    let stopped = fixture();
    stopped.registry.shutdown();
    assert!(!stopped.registry.owe_restore(&candidate("stt"), failure()));
    assert_eq!(stopped.launcher.count(), 0);

    let owed = fixture();
    owed.serve("stt").exited.store(true, Ordering::SeqCst);
    owed.registry.supervise();
    assert!(!owed.registry.owe_restore(&candidate("tts"), failure()));
    assert_eq!(step(&owed, Duration::from_secs(1)), restarted("stt", 2));
}
