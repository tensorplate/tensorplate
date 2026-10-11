// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::error::ControlChannelError;
use crate::worker::{LiveMember, MemberRole, ServingMember};
use tensorplate_protocol::worker_control::LedgerStatus;

fn exit(handle: &Handle) {
    handle.exited.store(true, Ordering::SeqCst);
}

/// One supervision step, `after` later on the registry's clock.
fn step(f: &Fixture, after: Duration) -> Vec<MemberEvent> {
    f.clock.advance(after);
    f.registry.supervise()
}

fn view(f: &Fixture) -> Vec<LiveMember> {
    f.registry
        .live_members()
        .expect("the registry reports its members")
}

/// A member that runs, is ready and in contact on a single port.
fn live(id: &str, generation: u64, role: MemberRole, http_port: u16) -> LiveMember {
    LiveMember {
        member: MemberRef::new(id, generation),
        role,
        running: true,
        ready: true,
        in_contact: Some(true),
        http_port,
        stream_port: None,
    }
}

/// Holds `handle`'s stop; returns what tells that it began and what lets
/// it finish.
fn hold_stop(handle: &Handle) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (started, has_started) = mpsc::channel();
    let (release, released) = mpsc::channel();
    *handle.hold_terminate.lock().unwrap() = Some((started, released));
    (has_started, release)
}

#[test]
fn nothing_is_listed_before_a_member_is_started() {
    let f = fixture();
    assert_eq!(view(&f), []);
    assert_eq!(f.registry.serving_member().unwrap(), None);
}

#[test]
fn a_warm_that_does_not_find_the_candidate_ready_judges_nothing() {
    let f = fixture();
    f.prepare("stt").unwrap();
    let loading = LiveMember {
        ready: false,
        ..live("stt", 1, MemberRole::Candidate, ACTIVE_PORT)
    };
    assert_eq!(view(&f), [loading.clone()]);

    assert!(!f.warm("stt").unwrap().ready);

    assert_eq!(view(&f), [loading]);
    assert_eq!(f.registry.serving_member().unwrap(), None);
}

#[test]
fn a_candidate_is_ready_once_a_warm_judged_it_and_serves_once_promoted() {
    let f = fixture();
    f.prepare("stt").unwrap();
    f.health(ACTIVE_PORT, "stt");

    assert!(f.warm("stt").unwrap().ready);
    assert_eq!(
        view(&f),
        [live("stt", 1, MemberRole::Candidate, ACTIVE_PORT)]
    );
    assert_eq!(f.registry.serving_member().unwrap(), None);

    f.registry.promote("tx", &candidate("stt")).unwrap();
    assert_eq!(view(&f), [live("stt", 1, MemberRole::Serving, ACTIVE_PORT)]);
    assert_eq!(
        f.registry.serving_member().unwrap(),
        Some(ServingMember {
            member: MemberRef::new("stt", 1),
            serving_url: format!("http://127.0.0.1:{ACTIVE_PORT}/infer"),
        })
    );
}

#[test]
fn the_serving_member_is_listed_before_the_candidate_each_with_its_own_generation_and_port() {
    let f = fixture();
    f.serve("stt");
    f.prepare("tts").unwrap();

    assert_eq!(
        view(&f),
        [
            live("stt", 1, MemberRole::Serving, ACTIVE_PORT),
            LiveMember {
                ready: false,
                ..live("tts", 2, MemberRole::Candidate, CANDIDATE_PORT)
            },
        ]
    );
}

#[test]
fn a_member_on_a_port_pair_lists_the_stream_port_it_holds() {
    let f = fixture_with(
        |settings| RegistrySettings {
            ports: vec![MemberPorts {
                http: 41_100,
                stream: Some(41_101),
            }],
            ..settings
        },
        |registry| registry,
    );
    f.serve("stt");

    assert_eq!(
        view(&f),
        [LiveMember {
            stream_port: Some(41_101),
            ..live("stt", 1, MemberRole::Serving, 41_100)
        }]
    );
}

#[test]
fn a_channel_out_of_contact_is_unanswered_and_one_in_contact_reports_only_with_a_clean_ledger() {
    let snapshot = |contact, ledger: bool, error: bool| ChannelSnapshot {
        member: MemberRef::new("stt", 1),
        contact,
        missed_polls: 0,
        ledger: ledger.then(|| LedgerStatus {
            reserved: 0,
            active: 0,
            closing: 0,
            ceiling: 1,
            admission_monotonic_ns: Vec::new(),
        }),
        ledger_received_at: None,
        last_error: error.then_some(ControlChannelError::Deadline),
        event_revision: 0,
        last_event: None,
    };
    for (contact, ledger, error, expected) in [
        (
            ContactState::InContact,
            true,
            false,
            ControlContact::Reporting,
        ),
        (
            ContactState::InContact,
            false,
            false,
            ControlContact::Pending,
        ),
        (ContactState::InContact, true, true, ControlContact::Pending),
        (
            ContactState::OutOfContact,
            true,
            false,
            ControlContact::Unanswered,
        ),
        (
            ContactState::OutOfContact,
            false,
            true,
            ControlContact::Unanswered,
        ),
        (ContactState::Failed, true, false, ControlContact::Failed),
    ] {
        assert_eq!(
            contact_of(&snapshot(contact, ledger, error)),
            expected,
            "{contact:?}, ledger {ledger}, error {error}"
        );
    }
}

#[test]
fn a_member_is_out_of_contact_in_the_view_when_its_polls_go_unanswered_or_contact_has_failed() {
    let f = fixture();
    let serving = f.serve("stt");
    for (contact, in_contact) in [
        (ControlContact::Reporting, true),
        (ControlContact::Pending, true),
        (ControlContact::Unanswered, false),
        (ControlContact::Failed, false),
    ] {
        serving.set_contact(contact);
        assert_eq!(
            view(&f),
            [LiveMember {
                in_contact: Some(in_contact),
                ..live("stt", 1, MemberRole::Serving, ACTIVE_PORT)
            }],
            "{contact:?}"
        );
    }
}

#[test]
fn unanswered_polls_do_not_take_a_member_out_of_service() {
    let f = fixture();
    let serving = f.serve("stt");
    serving.set_contact(ControlContact::Unanswered);

    assert_eq!(f.registry.supervise(), []);
    assert_eq!(serving.calls(), Vec::<&str>::new());
    assert_eq!(
        f.registry.active_deployment_id().unwrap().as_deref(),
        Some("stt")
    );
}

#[test]
fn a_candidate_whose_polls_go_unanswered_is_not_judged_ready() {
    let f = fixture();
    f.prepare("stt").unwrap();
    f.health(ACTIVE_PORT, "stt");
    f.launcher.handle(1).set_contact(ControlContact::Unanswered);

    assert!(!f.warm("stt").unwrap().ready);
    assert!(!view(&f)[0].ready);
}

#[test]
fn a_member_that_exited_is_not_running_then_gone_and_its_successor_is_ready_only_once_judged() {
    let f = fixture();
    exit(&f.serve("stt"));

    assert_eq!(
        view(&f),
        [LiveMember {
            running: false,
            ..live("stt", 1, MemberRole::Serving, ACTIVE_PORT)
        }]
    );
    assert_eq!(f.registry.serving_member().unwrap(), None);

    // Taken out of service: it was not asked to stop, so it is not draining.
    f.probe.silent.store(true, Ordering::SeqCst);
    f.registry.supervise();
    assert_eq!(view(&f), []);

    step(&f, Duration::from_secs(1));
    let successor = live("stt", 2, MemberRole::Serving, ACTIVE_PORT);
    assert_eq!(
        view(&f),
        [LiveMember {
            ready: false,
            ..successor.clone()
        }]
    );
    assert_eq!(f.registry.serving_member().unwrap(), None);

    f.probe.silent.store(false, Ordering::SeqCst);
    step(&f, Duration::from_secs(1));
    assert_eq!(view(&f), [successor]);
    assert_eq!(
        f.registry.serving_member().unwrap().map(|s| s.member),
        Some(MemberRef::new("stt", 2))
    );
}

#[test]
fn a_member_promoted_over_is_draining_until_it_has_been_reaped() {
    let f = fixture();
    let old = f.serve("stt");
    f.prepare("tts").unwrap();
    f.health(CANDIDATE_PORT, "tts");
    assert!(f.warm("tts").unwrap().ready);
    let (is_stopping, release) = hold_stop(&old);

    std::thread::scope(|scope| {
        let promoting = scope.spawn(|| f.registry.promote("tx", &candidate("tts")));
        is_stopping
            .recv_timeout(GRACE)
            .expect("the old member is asked to stop");

        assert_eq!(
            view(&f),
            [
                live("tts", 2, MemberRole::Serving, CANDIDATE_PORT),
                LiveMember {
                    in_contact: None,
                    ..live("stt", 1, MemberRole::Draining, ACTIVE_PORT)
                },
            ]
        );
        assert_eq!(
            f.registry.serving_member().unwrap().map(|s| s.member),
            Some(MemberRef::new("tts", 2))
        );

        release.send(()).unwrap();
        promoting.join().unwrap().unwrap();
    });

    assert_eq!(old.calls(), ["terminate"]);
    assert_eq!(
        view(&f),
        [live("tts", 2, MemberRole::Serving, CANDIDATE_PORT)]
    );
}

#[test]
fn a_draining_member_that_was_never_judged_ready_is_listed_not_ready() {
    let f = fixture();
    f.prepare("stt").unwrap();
    let (is_stopping, release) = hold_stop(&f.launcher.handle(1));

    std::thread::scope(|scope| {
        let unloading = scope.spawn(|| f.registry.unload("stt"));
        is_stopping
            .recv_timeout(GRACE)
            .expect("the candidate is asked to stop");

        assert_eq!(
            view(&f),
            [LiveMember {
                ready: false,
                in_contact: None,
                ..live("stt", 1, MemberRole::Draining, ACTIVE_PORT)
            }]
        );

        release.send(()).unwrap();
        unloading.join().unwrap();
    });
    assert_eq!(view(&f), []);
}

#[test]
fn a_member_killed_at_the_stop_deadline_leaves_the_view() {
    let f = fixture();
    f.launcher.stubborn.store(true, Ordering::SeqCst);
    let stubborn = f.serve("stt");

    f.registry.unload("stt");

    assert_eq!(stubborn.calls(), ["terminate", "kill"]);
    assert_eq!(view(&f), []);
}

#[test]
fn every_member_is_draining_and_none_serves_while_the_registry_stops() {
    let f = fixture();
    let serving = f.serve("stt");
    f.prepare("tts").unwrap();
    let (is_stopping, release) = hold_stop(&serving);

    std::thread::scope(|scope| {
        let stopping = scope.spawn(|| f.registry.shutdown());
        is_stopping
            .recv_timeout(GRACE)
            .expect("the serving member is asked to stop");

        assert_eq!(
            view(&f),
            [
                LiveMember {
                    in_contact: None,
                    ..live("stt", 1, MemberRole::Draining, ACTIVE_PORT)
                },
                LiveMember {
                    ready: false,
                    in_contact: None,
                    ..live("tts", 2, MemberRole::Draining, CANDIDATE_PORT)
                },
            ]
        );
        assert_eq!(f.registry.serving_member().unwrap(), None);

        release.send(()).unwrap();
        stopping.join().unwrap();
    });
    assert_eq!(view(&f), []);
}

/// Records, when it is dropped, whether its generation was still listed
/// as draining.
struct ListedAtDrop {
    set: Arc<DrainingSet>,
    listed: Arc<AtomicBool>,
}

impl MemberProcess for ListedAtDrop {
    fn exited(&mut self) -> AgentResult<bool> {
        Ok(false)
    }
    fn startup_failure(&mut self) -> AgentError {
        AgentError::WorkerNotReady
    }
    fn control(&self) -> ControlContact {
        ControlContact::Pending
    }
    fn terminate(&mut self) {}
    fn kill(&mut self) {}
}

impl Drop for ListedAtDrop {
    fn drop(&mut self) {
        self.listed
            .store(!self.set.listed().is_empty(), Ordering::SeqCst);
    }
}

/// A real process is killed and reaped when it is dropped: a member that
/// is dropped without having been stopped is draining until then, and not
/// after.
#[test]
fn a_draining_member_is_listed_until_its_process_has_been_dropped_and_not_after() {
    let set = Arc::new(DrainingSet::default());
    let listed = Arc::new(AtomicBool::new(false));
    let pool = PortPool::new(vec![MemberPorts {
        http: ACTIVE_PORT,
        stream: None,
    }]);
    let stopping = set.record(Member {
        deployment_id: "stt".into(),
        generation: 4,
        ready: true,
        config_path: PathBuf::new(),
        process: Box::new(ListedAtDrop {
            set: set.clone(),
            listed: listed.clone(),
        }),
        recipe: candidate("stt"),
        ports: pool.lease().unwrap(),
        outside: None,
    });
    let recorded = set.listed();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].0, 4);
    assert_eq!(recorded[0].1.deployment_id, "stt");
    assert!(recorded[0].1.ready);

    drop(stopping);

    assert!(listed.load(Ordering::SeqCst));
    assert!(set.listed().is_empty());
}
