// SPDX-License-Identifier: Apache-2.0

use super::*;

/// One supervision step, `after` later on the registry's clock.
fn step(f: &Fixture, after: Duration) -> Vec<MemberEvent> {
    f.clock.advance(after);
    f.registry.supervise()
}

/// How long a test waits for a launch it holds to begin.
const HELD_LAUNCH: Duration = Duration::from_secs(10);

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn exit(handle: &Handle) {
    handle.exited.store(true, Ordering::SeqCst);
}

fn member(id: &str, generation: u64) -> MemberRef {
    MemberRef::new(id, generation)
}

fn port_of(handle: &Handle) -> u64 {
    handle.config["bind"]["port"].as_u64().unwrap()
}

fn owed(events: &[MemberEvent]) -> Option<Duration> {
    events.iter().find_map(|event| match event {
        MemberEvent::RestartDue { after, .. } => Some(*after),
        _ => None,
    })
}

fn failed(events: &[MemberEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, MemberEvent::Failed { .. }))
}

/// Stop the serving generation and take its successor through to serving.
/// Returns the wait the registry announced before the restart.
fn crash_and_recover(f: &Fixture, generation: u64) -> Duration {
    exit(&f.launcher.handle(generation));
    let after = owed(&step(f, Duration::ZERO)).expect("a restart is owed");
    assert_eq!(
        step(f, after),
        [MemberEvent::Restarted {
            member: member("stt", generation + 1)
        }]
    );
    assert_eq!(
        step(f, Duration::ZERO),
        [MemberEvent::Serving {
            member: member("stt", generation + 1)
        }]
    );
    after
}

#[test]
fn a_serving_member_that_exits_is_started_again_as_a_new_generation() {
    let f = fixture();
    let first = f.serve("stt");
    exit(&first);

    let events = f.registry.supervise();
    assert!(
        matches!(&events[..], [
            MemberEvent::Stopped { member: stopped, .. },
            MemberEvent::RestartDue { deployment_id, after },
        ] if *stopped == member("stt", 1) && deployment_id == "stt" && *after == secs(1)),
        "{events:?}"
    );
    assert_eq!(f.configs(), 0);

    assert_eq!(step(&f, Duration::from_millis(999)), []);
    assert_eq!(f.launcher.count(), 1);

    assert_eq!(
        step(&f, Duration::from_millis(1)),
        [MemberEvent::Restarted {
            member: member("stt", 2)
        }]
    );
    let second = f.launcher.handle(2);
    assert_eq!(second.config["deployment"]["generation"], 2);
    assert_eq!(second.config["deployment"]["endpoint"], "stt");
    assert_eq!(port_of(&second), port_of(&first));
    assert_eq!(
        second.config["deployment"]["model"],
        first.config["deployment"]["model"]
    );
    assert_eq!(f.configs(), 1);
}

#[test]
fn a_restarted_generation_serves_only_once_it_is_ready() {
    let f = fixture();
    exit(&f.serve("stt"));
    f.probe.silent.store(true, Ordering::SeqCst);
    f.registry.supervise();
    step(&f, secs(1));

    assert_eq!(step(&f, secs(1)), []);
    assert_eq!(f.registry.active_deployment_id().unwrap(), None);
    assert_eq!(f.registry.active_serving_url().unwrap(), None);

    // Ready on `/health` with no ledger reported is still not serving.
    f.probe.silent.store(false, Ordering::SeqCst);
    f.launcher.handle(2).set_contact(ControlContact::Pending);
    assert_eq!(step(&f, secs(1)), []);
    assert_eq!(f.registry.active_serving_url().unwrap(), None);

    f.launcher.handle(2).set_contact(ControlContact::Reporting);
    assert_eq!(
        step(&f, secs(1)),
        [MemberEvent::Serving {
            member: member("stt", 2)
        }]
    );
    assert_eq!(
        f.registry.active_deployment_id().unwrap().as_deref(),
        Some("stt")
    );
    assert!(f.registry.active_serving_url().unwrap().is_some());
}

#[test]
fn the_three_restarts_wait_one_two_and_four_seconds() {
    let f = fixture();
    f.serve("stt");

    let waits: Vec<Duration> = (1..=3).map(|g| crash_and_recover(&f, g)).collect();

    assert_eq!(waits, RESTART_DELAYS);
    assert_eq!(waits, [secs(1), secs(2), secs(4)]);
}

#[test]
fn a_member_that_stops_with_three_restarts_in_five_minutes_is_failed_and_not_started_again() {
    let f = fixture();
    f.serve("stt");
    for generation in 1..=3 {
        crash_and_recover(&f, generation);
    }
    assert_eq!(f.registry.member_failure(), None);

    exit(&f.launcher.handle(4));
    let events = f.registry.supervise();

    assert!(
        matches!(&events[..], [
            MemberEvent::Stopped { member: stopped, error },
            MemberEvent::Failed { deployment_id, error: reported },
        ] if *stopped == member("stt", 4)
            && deployment_id == "stt"
            && reported.code == error.code
            && reported.message.contains("3 restarts inside 300 s")
            && reported.message.ends_with(&error.message)),
        "{events:?}"
    );
    for _ in 0..12 {
        assert_eq!(step(&f, secs(60)), []);
    }
    assert_eq!(f.launcher.count(), 4);
    assert_eq!(f.configs(), 0);
    assert_eq!(f.registry.active_deployment_id().unwrap(), None);
    let failure = f
        .registry
        .member_failure()
        .expect("the failure is reported");
    assert!(failure.message.contains("generation 4"), "{failure:?}");
}

#[test]
fn a_restart_older_than_five_minutes_no_longer_counts() {
    let f = fixture();
    f.serve("stt");
    // Restarts at 1 s, 3 s and 7 s on the fake clock.
    for generation in 1..=3 {
        crash_and_recover(&f, generation);
    }

    // At 300 s the first restart is 299 s old: all three still count.
    f.clock.advance(secs(293));
    // At 301 s it is five minutes old and the other two are not.
    f.clock.advance(secs(1));
    exit(&f.launcher.handle(4));
    assert_eq!(owed(&f.registry.supervise()), Some(secs(4)));
    assert_eq!(
        step(&f, secs(4)),
        [MemberEvent::Restarted {
            member: member("stt", 5)
        }]
    );
    assert_eq!(step(&f, Duration::ZERO).len(), 1);

    // At 600 s only that last restart, made at 305 s, is inside the window.
    f.clock.advance(secs(295));
    exit(&f.launcher.handle(5));
    assert_eq!(owed(&f.registry.supervise()), Some(secs(2)));
}

#[test]
fn a_restart_exactly_five_minutes_old_is_outside_the_window() {
    let f = fixture();
    f.serve("stt");
    for generation in 1..=3 {
        crash_and_recover(&f, generation);
    }

    // The first restart was made at 1 s; this is 300 s.
    f.clock.advance(secs(293));
    exit(&f.launcher.handle(4));
    let at_299 = f.registry.supervise();
    assert!(failed(&at_299), "299 s after the first restart: {at_299:?}");

    let g = fixture();
    g.serve("stt");
    for generation in 1..=3 {
        crash_and_recover(&g, generation);
    }
    g.clock.advance(secs(294));
    exit(&g.launcher.handle(4));
    assert_eq!(owed(&g.registry.supervise()), Some(secs(4)));
}

#[test]
fn a_serving_member_whose_control_channel_failed_is_killed_and_restarted() {
    let f = fixture();
    let first = f.serve("stt");
    first.set_contact(ControlContact::Failed);

    let events = f.registry.supervise();

    assert!(
        matches!(&events[..], [
            MemberEvent::Stopped { member: stopped, error },
            MemberEvent::RestartDue { .. },
        ] if *stopped == member("stt", 1) && error.message.contains("control channel")),
        "{events:?}"
    );
    assert_eq!(first.calls(), ["kill"]);
    assert_eq!(
        step(&f, secs(1)),
        [MemberEvent::Restarted {
            member: member("stt", 2)
        }]
    );
}

#[test]
fn a_serving_member_is_not_restarted_for_slow_answers() {
    let f = fixture();
    let serving = f.serve("stt");
    // Neither a silent `/health` nor unanswered polls short of the control
    // channel's own deadline stop a member.
    f.probe.silent.store(true, Ordering::SeqCst);
    serving.set_contact(ControlContact::Pending);

    for _ in 0..10 {
        assert_eq!(step(&f, secs(60)), []);
    }

    assert_eq!(f.launcher.count(), 1);
    assert!(serving.calls().is_empty());
    assert_eq!(
        f.registry.active_deployment_id().unwrap().as_deref(),
        Some("stt")
    );
}

#[test]
fn a_restarted_generation_not_ready_within_the_warm_timeout_is_killed_and_counted() {
    let f = fixture();
    exit(&f.serve("stt"));
    f.probe.silent.store(true, Ordering::SeqCst);
    f.registry.supervise();
    step(&f, secs(1));

    assert_eq!(step(&f, RESTART_WARM - Duration::from_millis(1)), []);
    let events = step(&f, Duration::from_millis(1));

    assert!(
        matches!(&events[..], [
            MemberEvent::Stopped { member: stopped, error },
            MemberEvent::RestartDue { after, .. },
        ] if *stopped == member("stt", 2)
            && error.message.contains("not ready")
            && *after == secs(2)),
        "{events:?}"
    );
    assert_eq!(f.launcher.handle(2).calls(), ["kill"]);
    assert_eq!(f.configs(), 0);
}

#[test]
fn a_restart_that_cannot_be_launched_counts_against_the_member() {
    let f = fixture();
    exit(&f.serve("stt"));
    f.launcher.refuse.store(true, Ordering::SeqCst);
    assert_eq!(owed(&f.registry.supervise()), Some(secs(1)));

    for wait in [1, 2] {
        let events = step(&f, secs(wait));
        assert!(
            matches!(&events[..], [
                MemberEvent::LaunchFailed { deployment_id, error },
                MemberEvent::RestartDue { after, .. },
            ] if deployment_id == "stt"
                && error.message.contains("spawn refused")
                && *after == secs(wait * 2)),
            "{events:?}"
        );
    }
    let events = step(&f, secs(4));
    assert!(
        matches!(
            &events[..],
            [MemberEvent::LaunchFailed { .. }, MemberEvent::Failed { .. }]
        ),
        "{events:?}"
    );
    assert_eq!(f.launcher.count(), 1);
    assert_eq!(f.configs(), 0);
    assert_eq!(step(&f, secs(600)), []);
}

#[test]
fn a_restart_waits_while_a_deploy_holds_the_member() {
    let f = fixture();
    let old = f.serve("stt");
    f.prepare("tts").unwrap();
    exit(&old);
    assert_eq!(owed(&f.registry.supervise()), Some(secs(1)));

    assert_eq!(step(&f, secs(60)), []);
    assert_eq!(f.launcher.count(), 2);

    // The deploy completes: its candidate serves and nothing is owed.
    let candidate_handle = f.launcher.handle(2);
    f.health(u16::try_from(port_of(&candidate_handle)).unwrap(), "tts");
    assert!(f.warm("tts").unwrap().ready);
    f.registry.promote("tx", &candidate("tts")).unwrap();
    assert_eq!(step(&f, secs(60)), []);
    assert_eq!(f.launcher.count(), 2);
    assert_eq!(
        f.registry.active_deployment_id().unwrap().as_deref(),
        Some("tts")
    );
}

#[test]
fn a_restart_starts_once_the_deploy_that_held_the_member_gave_up() {
    let f = fixture();
    let old = f.serve("stt");
    f.prepare("tts").unwrap();
    exit(&old);
    f.registry.supervise();
    assert_eq!(step(&f, secs(60)), []);

    assert!(!f.warm("tts").unwrap().ready);

    assert_eq!(
        step(&f, Duration::ZERO),
        [MemberEvent::Restarted {
            member: member("stt", 3)
        }]
    );
    assert_ne!(
        port_of(&f.launcher.handle(3)),
        port_of(&f.launcher.handle(2))
    );
    // A candidate given up on is older than the generation now serving.
    assert!(matches!(
        f.registry.promote("tx", &candidate("tts")),
        Err(AgentError::WorkerNotReady)
    ));
    assert_eq!(
        step(&f, Duration::ZERO),
        [MemberEvent::Serving {
            member: member("stt", 3)
        }]
    );
}

#[test]
fn a_successor_started_as_a_deploy_began_is_stopped_and_owed_again() {
    let f = fixture();
    exit(&f.serve("stt"));
    f.registry.supervise();
    f.clock.advance(secs(1));
    let (started, has_started) = mpsc::channel();
    let (release, released) = mpsc::channel();
    *f.launcher.hold_first.lock().unwrap() = Some((started, released));

    std::thread::scope(|scope| {
        // Dropped if this closure panics, which lets the held launch end.
        let release = release;
        let restart = scope.spawn(|| f.registry.supervise());
        has_started
            .recv_timeout(HELD_LAUNCH)
            .expect("the restart launches");
        // Generation 2 is mid-launch; the deploy's candidate takes 3.
        f.prepare("stt").unwrap();
        release.send(()).unwrap();
        assert_eq!(restart.join().unwrap(), []);
    });

    assert_eq!(f.launcher.handle(2).calls(), ["terminate"]);
    assert!(f.launcher.handle(3).calls().is_empty());
    // The deploy holds the member now, so the restart stays owed.
    assert_eq!(step(&f, secs(60)), []);
    assert_eq!(f.launcher.count(), 3);
}

#[test]
fn a_restart_does_not_start_while_a_prepare_is_running() {
    let f = fixture();
    exit(&f.serve("stt"));
    f.registry.supervise();
    f.clock.advance(secs(1));
    let (started, has_started) = mpsc::channel();
    let (release, released) = mpsc::channel();
    *f.launcher.hold_first.lock().unwrap() = Some((started, released));

    std::thread::scope(|scope| {
        let release = release;
        let held = scope.spawn(|| f.prepare("tts"));
        has_started
            .recv_timeout(HELD_LAUNCH)
            .expect("the launch starts");
        // The candidate is not in its slot yet; the prepare already holds.
        assert_eq!(f.registry.supervise(), []);
        assert_eq!(f.next_generation.load(Ordering::SeqCst), 3);
        release.send(()).unwrap();
        held.join().unwrap().unwrap();
    });

    assert_eq!(f.launcher.count(), 2);
}

#[test]
fn unloading_a_deployment_drops_the_restart_owed_to_it() {
    let f = fixture();
    exit(&f.serve("stt"));
    assert_eq!(owed(&f.registry.supervise()), Some(secs(1)));

    f.registry.unload("tts");
    f.registry.unload("stt");

    assert_eq!(step(&f, secs(60)), []);
    assert_eq!(f.launcher.count(), 1);
}

#[test]
fn a_candidate_prepared_while_a_restart_is_owed_takes_the_other_port() {
    let f = fixture();
    let old = f.serve("stt");
    exit(&old);
    f.registry.supervise();

    f.prepare("tts").unwrap();

    assert_ne!(port_of(&f.launcher.handle(2)), port_of(&old));
}

#[test]
fn a_candidate_that_exits_is_not_restarted() {
    let f = fixture();
    f.prepare("stt").unwrap();
    exit(&f.launcher.handle(1));

    for _ in 0..3 {
        assert_eq!(step(&f, secs(10)), []);
    }
    assert_eq!(f.launcher.count(), 1);
}

#[test]
fn promotion_gives_the_member_its_three_restarts_back() {
    let f = fixture();
    f.serve("stt");
    for generation in 1..=3 {
        crash_and_recover(&f, generation);
    }

    f.serve("stt");

    assert_eq!(crash_and_recover(&f, 5), secs(1));
}

#[test]
fn a_failed_member_is_reported_until_a_deployment_is_promoted() {
    let f = fixture();
    f.serve("stt");
    for generation in 1..=3 {
        crash_and_recover(&f, generation);
    }
    exit(&f.launcher.handle(4));
    f.registry.supervise();
    assert!(f.registry.member_failure().is_some());

    // A deploy that does not get as far as promotion changes nothing.
    f.prepare("tts").unwrap();
    assert!(f.registry.member_failure().is_some());
    f.serve("tts");

    assert_eq!(f.registry.member_failure(), None);
}

/// The failed state lives in the registry only: nothing durable carries it
/// to the next agent process, which starts the recorded deployment again.
#[test]
fn a_failed_member_is_not_remembered_by_the_next_registry() {
    let f = fixture();
    f.serve("stt");
    for generation in 1..=3 {
        crash_and_recover(&f, generation);
    }
    exit(&f.launcher.handle(4));
    assert!(failed(&f.registry.supervise()));
    let Fixture {
        td,
        launcher,
        probe,
        next_generation,
        clock,
        registry,
    } = f;
    drop(registry);

    let counter = next_generation.clone();
    let next = Fixture {
        registry: MemberRegistry::new(
            settings(td.path()),
            launcher.clone(),
            probe.clone(),
            Arc::new(move || Ok(counter.fetch_add(1, Ordering::SeqCst))),
        )
        .with_clock(clock.clone()),
        td,
        launcher,
        probe,
        next_generation,
        clock,
    };

    assert_eq!(next.registry.member_failure(), None);
    next.serve("stt");
    assert_eq!(crash_and_recover(&next, 5), secs(1));
}

#[test]
fn shutdown_stops_every_member_and_nothing_starts_after_it() {
    let f = fixture();
    let serving = f.serve("stt");
    f.prepare("tts").unwrap();
    let candidate_handle = f.launcher.handle(2);

    f.registry.shutdown();

    assert_eq!(serving.calls(), ["terminate"]);
    assert_eq!(candidate_handle.calls(), ["terminate"]);
    assert_eq!(f.configs(), 0);
    assert!(matches!(
        f.prepare("stt"),
        Err(AgentError::WorkerControl(message)) if message.contains("stopping")
    ));
    assert!(matches!(
        f.warm("tts"),
        Err(AgentError::WorkerControl(message)) if message.contains("stopping")
    ));
    assert_eq!(step(&f, secs(60)), []);
    assert_eq!(f.launcher.count(), 2);
    assert_eq!(f.next_generation.load(Ordering::SeqCst), 3);
}

#[test]
fn shutdown_drops_a_restart_that_was_owed() {
    let f = fixture();
    exit(&f.serve("stt"));
    assert_eq!(owed(&f.registry.supervise()), Some(secs(1)));

    f.registry.shutdown();

    assert_eq!(step(&f, secs(60)), []);
    assert_eq!(f.launcher.count(), 1);
}

#[test]
fn a_member_that_ignores_shutdown_is_killed_at_the_stop_deadline() {
    let f = fixture();
    let serving = f.serve("stt");
    serving.obeys_terminate.store(false, Ordering::SeqCst);

    let started = Instant::now();
    f.registry.shutdown();

    assert_eq!(serving.calls(), ["terminate", "kill"]);
    assert!(started.elapsed() >= DRAIN);
}

#[test]
fn a_launch_that_finishes_after_shutdown_is_stopped() {
    let f = fixture();
    let (started, has_started) = mpsc::channel();
    let (release, released) = mpsc::channel();
    *f.launcher.hold_first.lock().unwrap() = Some((started, released));

    std::thread::scope(|scope| {
        let release = release;
        let held = scope.spawn(|| f.prepare("stt"));
        has_started
            .recv_timeout(HELD_LAUNCH)
            .expect("the launch starts");
        f.registry.shutdown();
        release.send(()).unwrap();
        assert!(matches!(
            held.join().unwrap(),
            Err(AgentError::WorkerControl(message)) if message.contains("stopping")
        ));
    });

    assert_eq!(f.launcher.handle(1).calls(), ["terminate"]);
    assert_eq!(f.configs(), 0);
}
