// SPDX-License-Identifier: Apache-2.0

use super::*;
use tensorplate_protocol::agent_state::{AgentState, DeploymentRecord};
use tensorplate_protocol::{DeploymentDescriptor, ErrorCode};

use crate::descriptor_store::tests::{deploy, derive, naming, reference};
use crate::descriptor_store::{identity_of, resolve, restart_stamp, start_again, Start};

/// A registry wired as the agent's is, over a state whose `active` record
/// names the descriptor of generation 1 of `stt`.
struct Described {
    f: Fixture,
    state: TempDir,
    store: Arc<StateStore>,
    descriptors: Arc<DescriptorStore>,
    first: DeploymentDescriptor,
    recipe: CandidateRef,
}

impl std::ops::Deref for Described {
    type Target = Fixture;
    fn deref(&self) -> &Fixture {
        &self.f
    }
}

/// A staged root holding the manifest of `bundle`, which is all a restart
/// reads of it.
fn staged_copy(under: &TempDir, name: &str, bundle: &str) -> String {
    let staged = under.path().join(name);
    std::fs::create_dir_all(&staged).unwrap();
    let manifest = std::path::Path::new(&bundle_fixture(bundle)).join("manifest.json");
    std::fs::copy(manifest, staged.join("manifest.json")).unwrap();
    staged.display().to_string()
}

fn described() -> Described {
    described_with(|stamp| stamp)
}

fn described_with(stamp_of: impl FnOnce(RestartStamp) -> RestartStamp) -> Described {
    described_on(|settings| settings, stamp_of)
}

fn described_on(
    settings_of: impl FnOnce(RegistrySettings) -> RegistrySettings,
    stamp_of: impl FnOnce(RestartStamp) -> RestartStamp,
) -> Described {
    let state = TempDir::new().unwrap();
    let staged = staged_copy(&state, "staged", "v0_2/speech_stt_streaming");
    let store = Arc::new(StateStore::open(state.path().join("state")).unwrap());
    let descriptors = Arc::new(DescriptorStore::new(store.state_dir()));
    let first = deploy(&store, &descriptors, "stt", &staged).unwrap();
    let stamp = stamp_of(restart_stamp(store.clone(), descriptors.clone()));
    let wired = |mut registry: MemberRegistry| {
        registry.generations = durable_generations(store.clone());
        let stamped = registry.with_restart_stamp(stamp);
        stamped.with_descriptor_store(descriptors.clone())
    };
    let f = fixture_with(settings_of, wired);
    let recipe = reference(&first);
    Described {
        f,
        state,
        store,
        descriptors,
        first,
        recipe,
    }
}

impl Described {
    /// Take `recipe` through to serving on the first port.
    fn serve(&self, recipe: &CandidateRef) {
        self.registry.prepare("tx", recipe, WARM).unwrap();
        self.health(ACTIVE_PORT, &recipe.deployment_id);
        assert!(self.registry.warm("tx", recipe, WARM).unwrap().ready);
        self.registry.promote("tx", recipe).unwrap();
    }
    /// The serving generation exits and its restart becomes owed.
    fn stop(&self, generation: u64) {
        let serving = self.launcher.handle(generation);
        serving.exited.store(true, Ordering::SeqCst);
        self.registry.supervise();
    }
    /// One supervision step, `after` seconds later on the registry's clock.
    fn step(&self, after: u64) -> Vec<MemberEvent> {
        self.clock.advance(Duration::from_secs(after));
        self.registry.supervise()
    }
    /// `active` as the next agent start reads it.
    fn active(&self) -> DeploymentRecord {
        let read = StateStore::open(self.store.state_dir()).unwrap();
        read.snapshot().unwrap().active.unwrap()
    }
    /// Whether `active` names `descriptor` and its document is that one.
    fn records(&self, descriptor: &DeploymentDescriptor) -> bool {
        let identity = descriptor.identity();
        let read = self.descriptors.read("stt", identity).ok();
        self.active().descriptor_identity() == Ok(Some(identity))
            && read.as_ref() == Some(descriptor)
    }
    fn document(&self, generation: u64) -> std::path::PathBuf {
        let name = format!("descriptor-stt-{generation}.json");
        self.store.state_dir().join(name)
    }
    /// Every file of the state directory, the documents included, by name.
    fn state_files(&self) -> BTreeMap<String, Vec<u8>> {
        let entries = std::fs::read_dir(self.store.state_dir()).unwrap();
        let file = |entry: std::fs::DirEntry| {
            let name = entry.file_name().into_string().unwrap();
            (name, std::fs::read(entry.path()).unwrap())
        };
        entries.map(|entry| file(entry.unwrap())).collect()
    }
    fn documents(&self) -> usize {
        let entries = std::fs::read_dir(self.store.state_dir()).unwrap();
        let names = entries.map(|e| e.unwrap().file_name().into_string().unwrap());
        names.filter(|name| name.starts_with("descriptor-")).count()
    }
    fn edit_active(&self, edit: fn(&mut DeploymentRecord)) {
        let edit = |state: &mut AgentState| {
            edit(state.active.as_mut().unwrap());
            Ok(())
        };
        self.store.update(edit).unwrap();
    }
    fn last_config(&self) -> serde_json::Value {
        let launched = self.launcher.launched.lock().unwrap();
        launched.last().unwrap().config["deployment"].clone()
    }
}

fn restarted(generation: u64) -> [MemberEvent; 1] {
    let member = MemberRef::new("stt", generation);
    [MemberEvent::Restarted { member }]
}

/// The message of a restart refused as a launch that failed and counted:
/// `load_failed`, not recoverable, the next restart owed two seconds later.
fn refused_restart(events: &[MemberEvent]) -> &str {
    let [MemberEvent::LaunchFailed { error, .. }, MemberEvent::RestartDue { after, .. }] = events
    else {
        panic!("{events:?}");
    };
    assert_eq!(
        (error.code, error.recoverable),
        (ErrorCode::LoadFailed, false)
    );
    assert_eq!(*after, Duration::from_secs(2));
    &error.message
}

#[test]
fn a_deploys_launch_is_the_generation_its_reference_names_and_takes_none() {
    let d = described();
    assert_eq!(durable_generations(d.store.clone())().unwrap(), 2);

    d.serve(&d.recipe);

    let launched = d.launcher.handle(1);
    assert_eq!(launched.spec.member, MemberRef::new("stt", 1));
    assert_eq!(launched.config["deployment"]["generation"], 1);
    assert_eq!(d.store.snapshot().unwrap().next_generation, Some(3));

    // The generation is running: it is not handed to a second launch.
    let error = d.registry.prepare("tx", &d.recipe, WARM).unwrap_err();
    let says = "generation 1 is not started: the registry holds generation 1";
    assert!(error.to_string().contains(says), "{error}");
    assert!(d.launcher.count() == 1 && launched.calls().is_empty());
}

#[test]
fn a_handed_generation_is_refused_unless_it_is_above_every_generation_the_registry_holds() {
    // Generation 2 is held as the serving member, then as the restart owed for it.
    for owed in [false, true] {
        let d = described();
        let second = d.first.restamp(2).unwrap();
        let third = d.first.restamp(3).unwrap();
        d.descriptors.write(&second).unwrap();
        d.descriptors.write(&third).unwrap();
        d.serve(&reference(&second));
        if owed {
            d.stop(2);
        }

        for stale in [&d.first, &second] {
            let handed = reference(stale);
            let error = d.registry.prepare("tx", &handed, WARM).unwrap_err();
            let record = error.to_record();
            // Another attempt takes a new generation: recoverable.
            assert!(matches!(error, AgentError::Internal(_)), "{error}");
            assert!(record.code == ErrorCode::Internal && record.recoverable);
            let says = format!(
                "`stt` generation {} is not started: the registry holds generation 2",
                stale.generation
            );
            assert!(record.message.contains(&says), "owed {owed}: {record:?}");
        }
        assert_eq!(d.launcher.count(), 1, "owed {owed}");

        d.registry.prepare("tx", &reference(&third), WARM).unwrap();
        let member = MemberRef::new("stt", 3);
        assert_eq!(d.launcher.handle(3).spec.member, member, "owed {owed}");
    }
}

/// The refusal of a generation handed while the registry holds `held`.
fn refused_as_held(d: &Described, handed: &CandidateRef, held: u64) {
    let error = d.registry.prepare("tx", handed, WARM).unwrap_err();
    let record = error.to_record();
    assert!(matches!(error, AgentError::Internal(_)), "{error}");
    assert!(record.code == ErrorCode::Internal && record.recoverable);
    let says = format!("is not started: the registry holds generation {held}");
    assert!(record.message.contains(&says), "{record:?}");
}

/// The reference of `d`'s descriptor re-stamped for `generation`, its
/// document written.
fn restamped(d: &Described, generation: u64) -> CandidateRef {
    let next = d.first.restamp(generation).unwrap();
    d.descriptors.write(&next).unwrap();
    reference(&next)
}

#[test]
fn a_handed_generation_at_or_below_one_that_is_draining_is_refused() {
    let d = described();
    let second = restamped(&d, 2);
    d.serve(&second);
    let (is_stopping, release) = hold_stop(&d.launcher.handle(2));

    std::thread::scope(|scope| {
        // Dropped if this closure panics, which lets what is held end.
        let release = release;
        let unloading = scope.spawn(|| d.registry.unload("stt"));
        is_stopping
            .recv_timeout(GRACE)
            .expect("the member is asked to stop");

        // Generation 2 has left the table and has not been reaped.
        for handed in [&d.recipe, &second] {
            refused_as_held(&d, handed, 2);
        }
        assert_eq!(d.launcher.count(), 1);

        release.send(()).unwrap();
        unloading.join().unwrap();
    });
}

#[test]
fn with_two_generations_draining_a_handed_one_must_be_above_the_newer() {
    // A third port: a launch finds one while two workers hold theirs.
    let d = described_on(
        |mut settings| {
            let http = CANDIDATE_PORT + 1;
            settings.ports.push(MemberPorts { http, stream: None });
            settings
        },
        |stamp| stamp,
    );
    let [second, third, fourth] = [2, 3, 4].map(|generation| restamped(&d, generation));
    d.serve(&second);
    d.registry.prepare("tx", &third, WARM).unwrap();
    let (is_stopping, release) = hold_stop(&d.launcher.handle(2));

    std::thread::scope(|scope| {
        // Dropped if this closure panics, which lets what is held end.
        let release = release;
        let unloading = scope.spawn(|| d.registry.unload("stt"));
        is_stopping
            .recv_timeout(GRACE)
            .expect("the serving member is asked to stop");

        // Generations 2 and 3 left the table together; neither is reaped.
        for handed in [&d.recipe, &second, &third] {
            refused_as_held(&d, handed, 3);
        }
        assert_eq!(d.launcher.count(), 2);

        d.registry.prepare("tx", &fourth, WARM).unwrap();
        let member = MemberRef::new("stt", 4);
        assert_eq!(d.launcher.handle(4).spec.member, member);

        release.send(()).unwrap();
        unloading.join().unwrap();
    });
}

#[test]
fn the_serving_member_carries_the_digests_of_the_descriptor_it_was_started_from() {
    let d = described();
    let started_from = || {
        let serving = d.registry.serving_member().unwrap().unwrap();
        (
            serving.member.generation,
            serving.descriptor_digest,
            serving.configuration_digest,
        )
    };
    let of = |descriptor: &DeploymentDescriptor| {
        (
            descriptor.generation,
            Some(descriptor.descriptor_digest.clone()),
            Some(descriptor.configuration_digest.clone()),
        )
    };
    d.serve(&d.recipe);
    assert_eq!(started_from(), of(&d.first));

    // A restarted generation serves from what the stamp wrote on `active`.
    d.stop(1);
    assert_eq!(d.step(1), restarted(2));
    d.step(0);
    let second = d.first.restamp(2).unwrap();
    assert!(d.records(&second));
    assert_eq!(started_from(), of(&second));
}

#[test]
fn a_record_started_again_is_launched_as_the_generation_its_reference_names() {
    let d = described();
    let recorded = d.active();

    let again = start_again(
        &d.store,
        &d.registry,
        &recorded,
        "entry.json".into(),
        Start::Restore,
    );
    let again = again.unwrap().reference().clone();
    d.serve(&again);

    let second = d.first.restamp(2).unwrap();
    assert_eq!(d.launcher.handle(2).spec.member, MemberRef::new("stt", 2));
    assert_eq!(d.last_config()["generation"], 2);
    assert_eq!(d.last_config()["model"]["runner_profile"], "faster_whisper");
    assert!(d.records(&second) && d.launcher.count() == 1);

    // Its restart is stamped from the generation the record now names.
    d.stop(2);
    assert_eq!(d.step(1), restarted(3));
    assert!(d.records(&second.restamp(3).unwrap()));
}

#[test]
fn a_running_generation_the_source_gives_again_is_an_internal_error_and_starts_nothing() {
    let f = fixture();
    f.serve("stt");
    f.next_generation.store(1, Ordering::SeqCst);

    let error = f.prepare("stt").unwrap_err();

    let says = "the generation source gave `stt` generation 1, which is running";
    assert!(matches!(error, AgentError::Internal(_)), "{error}");
    assert!(error.to_string().contains(says), "{error}");
    assert_eq!(f.launcher.count(), 1);
}

#[test]
fn prepare_refuses_a_reference_that_names_part_of_a_descriptor_before_it_displaces_anything() {
    let d = described();
    d.registry.prepare("tx", &d.recipe, WARM).unwrap();
    let counter = d.store.snapshot().unwrap().next_generation;

    let partial = [
        [true, false, false],
        [false, true, true],
        [true, true, false],
    ];
    for kept in partial {
        let partial = naming(&d.recipe, kept);
        let error = d.registry.prepare("tx", &partial, WARM).unwrap_err();
        let record = error.to_record();
        assert!(
            matches!(error, AgentError::DeploymentDescriptor { .. }),
            "{error}"
        );
        assert!(record.code == ErrorCode::LoadFailed && !record.recoverable);
        assert!(record.message.contains("all three or none"), "{record:?}");
    }
    assert!(d.launcher.count() == 1 && d.launcher.handle(1).calls().is_empty());
    assert_eq!(d.store.snapshot().unwrap().next_generation, counter);
}

#[test]
fn a_candidate_that_names_a_descriptor_is_rendered_from_its_document_and_no_staged_manifest() {
    let d = described();
    let staged = std::path::Path::new(&d.recipe.staged_path);
    std::fs::remove_file(staged.join("manifest.json")).unwrap();

    d.registry.prepare("tx", &d.recipe, WARM).unwrap();

    assert_eq!(d.last_config()["model"]["runner_profile"], "faster_whisper");
}

#[test]
fn a_candidate_that_contradicts_the_descriptor_it_names_is_not_started() {
    let d = described();
    let mut elsewhere = d.recipe.clone();
    elsewhere.staged_path = staged_copy(&d.state, "another", "v0_2/speech_tts_streaming");
    let mut unrelated = d.recipe.clone();
    unrelated.bundle_digest = format!("sha256:{}", "0".repeat(64));
    unrelated.model_class = "vision".into();
    unrelated.backend_hint = "mock".into();

    for (field, contradicting) in [("staged_path", elsewhere), ("bundle_digest", unrelated)] {
        let error = d.registry.prepare("tx", &contradicting, WARM).unwrap_err();
        let record = error.to_record();
        assert!(
            matches!(error, AgentError::DeploymentDescriptor { .. }),
            "{error}"
        );
        assert!(record.code == ErrorCode::LoadFailed && !record.recoverable);
        let says = format!("of `stt` generation 1 is not the candidate's: its `{field}` is");
        assert!(record.message.contains(&says), "{record:?}");
        assert_eq!((d.launcher.count(), d.configs()), (0, 0), "{field}");
    }
}

#[test]
fn a_candidate_whose_document_cannot_be_read_is_not_started() {
    let d = described();
    std::fs::remove_file(d.document(1)).unwrap();

    let error = d.registry.prepare("tx", &d.recipe, WARM).unwrap_err();

    let says = "descriptor-stt-1.json` is missing";
    assert!(error.to_string().contains(says), "{error}");
    assert_eq!(error.code(), ErrorCode::LoadFailed);
    assert_eq!((d.launcher.count(), d.configs()), (0, 0));

    // A registry that was given no store cannot start it either.
    let plain = fixture();
    let error = plain.registry.prepare("tx", &d.recipe, WARM).unwrap_err();
    assert!(error.to_string().contains("no descriptor store"), "{error}");
    assert_eq!((plain.launcher.count(), plain.configs()), (0, 0));
}

#[test]
fn a_restart_re_stamps_the_descriptor_and_the_restart_after_it_starts_from_that() {
    let d = described();
    d.serve(&d.recipe);
    d.stop(1);

    assert_eq!(d.step(1), restarted(2));

    let second = d.first.restamp(2).unwrap();
    assert!(d.records(&second));
    assert_eq!(second.configuration_digest, d.first.configuration_digest);
    assert_ne!(second.descriptor_digest, d.first.descriptor_digest);
    assert_eq!(d.last_config()["generation"], 2);
    assert_eq!(d.last_config()["model"]["runner_profile"], "faster_whisper");
    let member = MemberRef::new("stt", 2);
    assert_eq!(d.step(0), [MemberEvent::Serving { member }]);

    d.stop(2);
    assert_eq!(d.step(2), restarted(3));
    assert!(d.records(&second.restamp(3).unwrap()));
    assert_eq!(d.last_config()["generation"], 3);
}

#[test]
fn a_restart_whose_descriptor_cannot_be_re_stamped_is_a_launch_that_failed_and_writes_nothing() {
    const NOT_NAMED: &str = "the active deployment record does not name it";
    let other = serde_json::to_vec(&derive("stt", 1, "/staged/other")).unwrap();
    let says = [
        "is missing",
        "does not verify",
        ", not generation 1 of `stt`",
        NOT_NAMED,
        NOT_NAMED,
        NOT_NAMED,
    ];
    for (case, says) in says.into_iter().enumerate() {
        let d = described();
        d.serve(&d.recipe);
        durable_generations(d.store.clone())().unwrap();
        match case {
            0 => std::fs::remove_file(d.document(1)).unwrap(),
            1 => std::fs::write(d.document(1), "{}").unwrap(),
            2 => std::fs::write(d.document(1), &other).unwrap(),
            3 => d.edit_active(|active| active.deployment_id = "tts".into()),
            4 => d.edit_active(|active| active.generation = Some(2)),
            // The same generation, another descriptor.
            _ => d.edit_active(|active| {
                active.descriptor_digest = Some(format!("sha256:{}", "0".repeat(64)));
            }),
        }
        let before = (d.active(), d.documents());
        d.stop(1);

        let events = d.step(1);

        let message = refused_restart(&events);
        assert!(
            message.contains("descriptor-stt-1.json` "),
            "case {case}: {message}"
        );
        assert!(message.contains(says), "case {case}: {message}");
        assert_eq!((d.launcher.count(), d.configs()), (1, 0), "case {case}");
        assert_eq!((d.active(), d.documents()), before, "case {case}");
    }
}

#[test]
fn a_described_member_whose_staged_copy_was_replaced_is_failed_and_nothing_is_stamped() {
    let d = described();
    d.serve(&d.recipe);
    staged_copy(&d.state, "staged", "v0_2/speech_tts_streaming");
    let before = d.state_files();
    assert_eq!(before.len(), 3, "{:?}", before.keys());
    d.stop(1);

    let events = d.step(1);

    // Failed at once: no launch failed and no restart is owed.
    let [MemberEvent::Failed {
        deployment_id,
        error,
    }] = &events[..]
    else {
        panic!("{events:?}");
    };
    let says = "`stt` is not restarted: the bundle staged at";
    assert!(
        deployment_id == "stt" && error.message.contains(says),
        "{error:?}"
    );
    assert_eq!((d.launcher.count(), d.configs()), (1, 0));
    assert_eq!(d.state_files(), before);
    assert_eq!(d.step(60), []);
    assert_eq!(d.state_files(), before);
}

#[test]
fn a_stop_that_begins_while_the_descriptor_is_re_stamped_starts_nothing() {
    let (stamping, is_stamping) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let held = Mutex::new((stamping, released));
    let d = described_with(move |real| {
        Arc::new(move |restamp| {
            let held = held.lock().unwrap();
            held.0.send(()).unwrap();
            let _ = held.1.recv();
            real(restamp)
        })
    });
    d.serve(&d.recipe);
    d.stop(1);
    d.clock.advance(Duration::from_secs(1));

    std::thread::scope(|scope| {
        let release = release;
        let restarting = scope.spawn(|| d.registry.supervise());
        is_stamping
            .recv_timeout(GRACE)
            .expect("the restart is stamped");
        let stopping = scope.spawn(|| d.registry.shutdown());
        let deadline = Instant::now() + GRACE;
        while !d.registry.stopping() {
            assert!(Instant::now() < deadline, "shutdown never began");
            std::thread::sleep(Duration::from_millis(1));
        }
        release.send(()).unwrap();
        assert_eq!(restarting.join().unwrap(), []);
        stopping.join().unwrap();
    });

    assert_eq!((d.launcher.count(), d.configs()), (1, 0));
    // What the stamp wrote stands, for the next start to read.
    assert!(d.records(&d.first.restamp(2).unwrap()));
}

/// A recipe with no identity, whatever its bundle's format: the configs the
/// wired registry writes are those of a registry with no stamp and no store.
#[test]
fn a_recipe_that_names_no_descriptor_is_started_and_restarted_as_without_a_store() {
    for bundle in ["v0_1/x86_fixture_smoke", "v0_2/speech_stt_streaming"] {
        let d = described();
        let plain = fixture();
        plain.next_generation.store(2, Ordering::SeqCst);
        let recipe = candidate_staged_at("stt", &bundle_fixture(bundle));
        let recorded = d.active();
        let config = |launcher: &FakeLauncher, generation| {
            std::fs::read(&launcher.handle(generation).spec.config_path).unwrap()
        };

        d.serve(&recipe);
        plain.registry.prepare("tx", &recipe, WARM).unwrap();
        plain.health(ACTIVE_PORT, "stt");
        assert!(plain.registry.warm("tx", &recipe, WARM).unwrap().ready);
        plain.registry.promote("tx", &recipe).unwrap();
        assert_eq!(
            config(&d.launcher, 2),
            config(&plain.launcher, 2),
            "{bundle}"
        );

        d.stop(2);
        plain
            .launcher
            .handle(2)
            .exited
            .store(true, Ordering::SeqCst);
        plain.registry.supervise();
        plain.clock.advance(Duration::from_secs(1));
        assert_eq!(d.step(1), restarted(3));
        assert_eq!(plain.registry.supervise(), restarted(3));
        assert_eq!(
            config(&d.launcher, 3),
            config(&plain.launcher, 3),
            "{bundle}"
        );
        assert_eq!((d.active(), d.documents()), (recorded, 1));
    }
    // Its staged manifest is still what it is rendered from.
    let d = described();
    let mut gone = naming(&d.recipe, [false; 3]);
    gone.staged_path = d.state.path().join("gone").display().to_string();
    let refused = d.registry.prepare("tx", &gone, WARM);
    assert!(
        matches!(refused, Err(AgentError::BundleMissing(_))),
        "{refused:?}"
    );
}

#[test]
fn the_agents_registry_reads_and_re_stamps_the_documents_beside_its_state() {
    let td = TempDir::new().unwrap();
    let config = serde_json::json!({
        "transport": "loopback_tcp",
        "state_dir": td.path().join("state"),
        "staging_dir": td.path().join("staging"),
        "worker": {
            "mode": "process",
            "serving_binary_path": "/usr/local/bin/tensorplate-serving",
            "serving_config_dir": td.path().join("worker-configs"),
        },
    });
    let agent = crate::config::AgentConfig::parse_json(&config.to_string()).unwrap();
    let store = Arc::new(StateStore::open(&agent.state_dir).unwrap());
    let beside = DescriptorStore::new(&agent.state_dir);
    let first = deploy(&store, &beside, "stt", "/staged/stt").unwrap();
    let recipe = reference(&first);

    let registry = crate::worker::process_registry(&agent, &store).unwrap();

    let read = resolve(registry.descriptors.as_deref(), &recipe, 1).unwrap();
    assert_eq!(read.as_ref(), Some(&first));
    let stamp = registry
        .restart_stamp
        .as_ref()
        .expect("a stamp is installed");
    let generation = (registry.generations)().unwrap();
    let restamp = Restamp {
        recipe: &recipe,
        stopped_generation: 1,
        generation,
    };
    let stamped = stamp(&restamp).unwrap();
    let second = first.restamp(2).unwrap();
    assert_eq!(identity_of(&stamped).unwrap(), Some(second.identity()));
    assert_eq!(beside.read("stt", second.identity()).unwrap(), second);
    let active = store.snapshot().unwrap().active.unwrap();
    assert_eq!(active.descriptor_identity(), Ok(Some(second.identity())));
}
