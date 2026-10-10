// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::config::{PortRange, WorkerConfig};

fn http_port(handle: &Handle) -> u16 {
    u16::try_from(handle.config["bind"]["port"].as_u64().unwrap()).unwrap()
}

fn ranged(first: u16, last: u16) -> Vec<MemberPorts> {
    configured_ports(&WorkerConfig {
        serving_port_range: Some(PortRange { first, last }),
        ..WorkerConfig::default()
    })
}

fn exit(handle: &Handle) {
    handle.exited.store(true, Ordering::SeqCst);
}

#[test]
fn a_range_is_handed_out_as_pairs_and_the_two_single_ports_without_a_stream_port() {
    let pair = |http, stream| MemberPorts {
        http,
        stream: Some(stream),
    };
    assert_eq!(
        ranged(41_100, 41_104),
        [pair(41_100, 41_101), pair(41_102, 41_103)]
    );
    assert_eq!(ranged(65_532, 65_535).last(), Some(&pair(65_534, 65_535)));
    assert_eq!(ranged(65_535, 65_535), []);

    let single = WorkerConfig {
        serving_bind_port: 41_081,
        serving_candidate_bind_port: 41_080,
        ..WorkerConfig::default()
    };
    assert_eq!(
        configured_ports(&single),
        [41_081, 41_080].map(|http| MemberPorts { http, stream: None })
    );
}

#[test]
fn each_member_binds_the_http_port_of_the_first_free_pair_and_nothing_names_its_stream_port() {
    let f = fixture_with(
        |settings| RegistrySettings {
            ports: ranged(41_100, 41_105),
            ..settings
        },
        |registry| registry,
    );

    let first = f.serve("stt");
    f.prepare("tts").unwrap();
    let second = f.launcher.handle(2);

    assert_eq!(http_port(&first), 41_100);
    assert_eq!(http_port(&second), 41_102);
    assert!(!second.config.to_string().contains("41103"));
    assert!(second.config.get("streaming").is_none());
    assert_eq!(
        f.registry.active_serving_url().unwrap().as_deref(),
        Some("http://127.0.0.1:41100/infer")
    );
}

#[test]
fn the_ports_of_a_reaped_member_are_given_to_the_next() {
    let f = fixture();
    let first = f.serve("stt");
    let second = f.serve("tts");
    assert_eq!(http_port(&first), ACTIVE_PORT);
    assert_eq!(http_port(&second), CANDIDATE_PORT);

    f.prepare("third").unwrap();

    assert_eq!(http_port(&f.launcher.handle(3)), ACTIVE_PORT);
}

/// A member that is draining is out of the registry's map and still bound
/// to its ports: nothing else is given them until it has been reaped.
#[test]
fn a_draining_members_ports_are_not_given_out_until_it_is_reaped() {
    let f = fixture();
    let old = f.serve("stt");
    f.prepare("tts").unwrap();
    f.health(CANDIDATE_PORT, "tts");
    assert!(f.warm("tts").unwrap().ready);
    let (started, is_draining) = mpsc::channel();
    let (release, released) = mpsc::channel();
    *old.hold_terminate.lock().unwrap() = Some((started, released));

    std::thread::scope(|scope| {
        let release = release;
        let promoting = scope.spawn(|| f.registry.promote("tx", &candidate("tts")));
        is_draining
            .recv_timeout(GRACE)
            .expect("the old member is asked to stop");

        let refused = f.prepare("third").unwrap_err();
        assert!(matches!(refused, AgentError::NoFreePort(_)), "{refused}");
        assert_eq!(
            refused.code(),
            tensorplate_protocol::ErrorCode::ResourceExhausted
        );
        assert!(refused.to_record().recoverable);
        assert_eq!(f.launcher.count(), 2);
        assert_eq!(f.next_generation.load(Ordering::SeqCst), 3);

        let waiting = scope.spawn(|| f.registry.prepare("tx", &candidate("third"), GRACE));
        std::thread::sleep(DRAIN);
        assert_eq!(f.launcher.count(), 2);
        release.send(()).unwrap();
        promoting.join().unwrap().unwrap();
        waiting.join().unwrap().unwrap();
    });

    assert_eq!(http_port(&f.launcher.handle(3)), ACTIVE_PORT);
}

#[test]
fn the_restart_owed_to_a_member_keeps_its_ports_until_the_member_is_unloaded() {
    let f = fixture();
    exit(&f.serve("stt"));
    f.registry.supervise();

    // Each candidate displaces the one before it and takes its ports,
    // never the ports kept for the restart.
    f.prepare("tts").unwrap();
    f.prepare("third").unwrap();
    assert_eq!(http_port(&f.launcher.handle(2)), CANDIDATE_PORT);
    assert_eq!(http_port(&f.launcher.handle(3)), CANDIDATE_PORT);

    f.registry.unload("stt");
    f.prepare("fourth").unwrap();
    assert_eq!(http_port(&f.launcher.handle(4)), ACTIVE_PORT);
}

#[test]
fn a_member_that_is_failed_gives_its_ports_back() {
    let f = fixture();
    let mut serving = f.serve("stt");
    for restart in 0..3 {
        exit(&serving);
        let after = f
            .registry
            .supervise()
            .iter()
            .find_map(|event| match event {
                MemberEvent::RestartDue { after, .. } => Some(*after),
                _ => None,
            })
            .expect("a restart is owed");
        f.clock.advance(after);
        f.registry.supervise();
        serving = f.launcher.handle(restart + 2);
        assert_eq!(http_port(&serving), ACTIVE_PORT);
        f.registry.supervise();
    }
    exit(&serving);
    f.registry.supervise();
    assert!(f.registry.member_failure().is_some());

    f.prepare("tts").unwrap();

    assert_eq!(http_port(&f.launcher.handle(5)), ACTIVE_PORT);
}

type Stamps = Arc<Mutex<Vec<(String, u64, u64, String)>>>;

/// A fixture whose registry records every restart stamp and answers it
/// with `answer`.
fn stamped_fixture(
    answer: impl Fn(&Restamp<'_>) -> AgentResult<CandidateRef> + Send + Sync + 'static,
) -> (Fixture, Stamps) {
    let stamps = Stamps::default();
    let seen = stamps.clone();
    let f = fixture_with(
        |settings| settings,
        |registry| {
            registry.with_restart_stamp(Arc::new(move |restamp| {
                seen.lock().unwrap().push((
                    restamp.recipe.deployment_id.clone(),
                    restamp.stopped_generation,
                    restamp.generation,
                    restamp.recipe.backend_hint.clone(),
                ));
                answer(restamp)
            }))
        },
    );
    (f, stamps)
}

/// The recipe as the stamp of `generation` returns it.
fn stamped_for(restamp: &Restamp<'_>) -> CandidateRef {
    CandidateRef {
        backend_hint: format!("stamped-{}", restamp.generation),
        ..restamp.recipe.clone()
    }
}

fn restart(f: &Fixture, after: u64) -> Vec<MemberEvent> {
    f.clock.advance(Duration::from_secs(after));
    f.registry.supervise()
}

#[test]
fn a_restart_is_stamped_before_its_config_is_rendered_and_started_from_what_the_stamp_returns() {
    let (f, stamps) = stamped_fixture(|restamp| Ok(stamped_for(restamp)));
    exit(&f.serve("stt"));
    assert_eq!(
        *stamps.lock().unwrap(),
        [],
        "a deploy's launch is not stamped"
    );
    f.registry.supervise();

    restart(&f, 1);

    assert_eq!(
        *stamps.lock().unwrap(),
        [("stt".to_owned(), 1, 2, "mock".to_owned())]
    );
    let second = f.launcher.handle(2);
    assert_eq!(second.config["deployment"]["backend"], "stamped-2");
    f.registry.supervise();

    // The next restart replaces the generation that one was stamped for,
    // from what its stamp returned.
    exit(&second);
    f.registry.supervise();
    restart(&f, 2);
    assert_eq!(
        stamps.lock().unwrap().last(),
        Some(&("stt".to_owned(), 2, 3, "stamped-2".to_owned()))
    );
}

#[test]
fn a_stamp_that_fails_is_a_restart_that_could_not_be_launched() {
    let fail = Arc::new(AtomicBool::new(true));
    let failing = fail.clone();
    let (f, stamps) = stamped_fixture(move |restamp| {
        if failing.load(Ordering::SeqCst) {
            return Err(AgentError::Internal(
                "the descriptor was not written".into(),
            ));
        }
        Ok(stamped_for(restamp))
    });
    exit(&f.serve("stt"));
    f.registry.supervise();

    let events = restart(&f, 1);

    assert!(
        matches!(&events[..], [
            MemberEvent::LaunchFailed { error, .. },
            MemberEvent::RestartDue { after, .. },
        ] if error.message.contains("the descriptor was not written")
            && *after == Duration::from_secs(2)),
        "{events:?}"
    );
    assert_eq!(f.launcher.count(), 1);
    assert_eq!(f.configs(), 0);

    // Nothing was stamped, so the next attempt still replaces generation 1.
    fail.store(false, Ordering::SeqCst);
    restart(&f, 2);
    assert_eq!(
        stamps.lock().unwrap().last(),
        Some(&("stt".to_owned(), 1, 3, "mock".to_owned()))
    );
}

#[test]
fn a_stamp_written_for_a_launch_that_then_failed_is_what_the_next_restart_replaces() {
    let (f, stamps) = stamped_fixture(|restamp| Ok(stamped_for(restamp)));
    exit(&f.serve("stt"));
    f.registry.supervise();
    f.launcher.refuse.store(true, Ordering::SeqCst);

    let events = restart(&f, 1);

    assert!(
        matches!(
            &events[..],
            [
                MemberEvent::LaunchFailed { .. },
                MemberEvent::RestartDue { .. }
            ]
        ),
        "{events:?}"
    );
    f.launcher.refuse.store(false, Ordering::SeqCst);
    restart(&f, 2);
    assert_eq!(
        stamps.lock().unwrap()[1..],
        [("stt".to_owned(), 2, 3, "stamped-2".to_owned())]
    );
    assert_eq!(
        f.launcher.handle(3).config["deployment"]["backend"],
        "stamped-3"
    );
}

#[test]
fn a_stamp_that_names_another_deployment_fails_the_launch() {
    let (f, _stamps) = stamped_fixture(|restamp| {
        Ok(CandidateRef {
            deployment_id: "tts".into(),
            ..restamp.recipe.clone()
        })
    });
    exit(&f.serve("stt"));
    f.registry.supervise();

    let events = restart(&f, 1);

    assert!(
        matches!(&events[..], [MemberEvent::LaunchFailed { deployment_id, .. }, ..]
            if deployment_id == "stt"),
        "{events:?}"
    );
    assert_eq!(f.launcher.count(), 1);
}

#[test]
fn a_member_whose_staged_bundle_was_replaced_is_not_stamped() {
    let (f, stamps) = stamped_fixture(|restamp| Ok(stamped_for(restamp)));
    let staged = f.td.path().join("staged");
    let stage = |bundle: &str| {
        std::fs::create_dir_all(&staged).unwrap();
        for entry in std::fs::read_dir(bundle_fixture(bundle)).unwrap() {
            let entry = entry.unwrap();
            std::fs::copy(entry.path(), staged.join(entry.file_name())).unwrap();
        }
    };
    stage("v0_1/x86_fixture_smoke");
    let recorded = candidate_staged_at("stt", &staged.display().to_string());
    f.registry.prepare("tx", &recorded, WARM).unwrap();
    f.health(ACTIVE_PORT, "stt");
    assert!(f.registry.warm("tx", &recorded, WARM).unwrap().ready);
    f.registry.promote("tx", &recorded).unwrap();
    stage("v0_1/x86_cuda_smoke");
    exit(&f.launcher.handle(1));
    f.registry.supervise();

    let events = restart(&f, 1);

    assert!(
        matches!(&events[..], [MemberEvent::Failed { .. }]),
        "{events:?}"
    );
    assert_eq!(*stamps.lock().unwrap(), []);
}

#[test]
fn a_successor_that_yielded_to_a_deploy_is_the_generation_the_next_restart_replaces() {
    let (f, stamps) = stamped_fixture(|restamp| Ok(stamped_for(restamp)));
    exit(&f.serve("stt"));
    f.registry.supervise();
    f.clock.advance(Duration::from_secs(1));
    let (started, has_started) = mpsc::channel();
    let (release, released) = mpsc::channel();
    *f.launcher.hold_first.lock().unwrap() = Some((started, released));

    std::thread::scope(|scope| {
        let release = release;
        let restarting = scope.spawn(|| f.registry.supervise());
        has_started
            .recv_timeout(GRACE)
            .expect("the restart launches");
        f.prepare("tts").unwrap();
        release.send(()).unwrap();
        assert_eq!(restarting.join().unwrap(), []);
    });
    f.registry.unload("tts");

    restart(&f, 0);

    assert_eq!(
        stamps.lock().unwrap()[1..],
        [("stt".to_owned(), 2, 4, "stamped-2".to_owned())]
    );
}

/// What a packaged configuration can meet: a successor that yielded to a
/// deploy is still draining on the first port when the next deploy comes.
#[test]
fn a_candidate_waits_for_the_port_of_a_successor_that_is_still_draining() {
    let stop_deadline = Duration::from_secs(1);
    let f = fixture_with(
        |settings| RegistrySettings {
            stop_deadline,
            ..settings
        },
        |registry| registry,
    );
    exit(&f.serve("stt"));
    f.registry.supervise();
    f.clock.advance(Duration::from_secs(1));
    f.launcher.stubborn.store(true, Ordering::SeqCst);
    let (started, has_started) = mpsc::channel();
    let (release, released) = mpsc::channel();
    *f.launcher.hold_first.lock().unwrap() = Some((started, released));

    std::thread::scope(|scope| {
        let release = release;
        let restarting = scope.spawn(|| f.registry.supervise());
        has_started
            .recv_timeout(GRACE)
            .expect("the restart launches");
        f.prepare("tts").unwrap();
        release.send(()).unwrap();
        // The successor yields and drains on the first port; meanwhile the
        // deploy is promoted and the next one begins.
        f.health(CANDIDATE_PORT, "tts");
        assert!(f.warm("tts").unwrap().ready);
        f.registry.promote("tx", &candidate("tts")).unwrap();
        let began = Instant::now();
        let refused = f.prepare("third");
        assert!(
            matches!(refused, Err(AgentError::NoFreePort(_))),
            "{refused:?}"
        );

        f.registry
            .prepare("tx", &candidate("third"), GRACE)
            .unwrap();

        assert!(
            began.elapsed() >= stop_deadline / 2,
            "{:?}",
            began.elapsed()
        );
        assert_eq!(restarting.join().unwrap(), []);
    });
    assert_eq!(f.launcher.handle(2).calls(), ["terminate", "kill"]);
    assert_eq!(http_port(&f.launcher.handle(4)), ACTIVE_PORT);
}

#[test]
fn a_member_unloaded_while_its_restart_was_in_hand_is_neither_stamped_nor_started() {
    let (f, stamps) = stamped_fixture(|restamp| Ok(stamped_for(restamp)));
    exit(&f.serve("stt"));
    f.registry.supervise();
    f.clock.advance(Duration::from_secs(1));
    let (reading, is_reading) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    *f.registry.before_staged_read.lock().unwrap() = Some(Box::new(move || {
        reading.send(()).unwrap();
        let _ = released.recv();
    }));

    let events = std::thread::scope(|scope| {
        let release = release;
        let restarting = scope.spawn(|| f.registry.supervise());
        is_reading
            .recv_timeout(GRACE)
            .expect("the manifest read begins");
        f.registry.unload("stt");
        release.send(()).unwrap();
        restarting.join().unwrap()
    });

    assert_eq!(events, []);
    assert_eq!(*stamps.lock().unwrap(), []);
    assert_eq!(f.launcher.count(), 1);
}
