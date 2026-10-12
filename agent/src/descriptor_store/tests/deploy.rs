// SPDX-License-Identifier: Apache-2.0

use super::start_again::{host, refuse_state_writes, shown, Agent, Host, Worker};
use super::*;

use tensorplate_protocol::agent_control::RecoveryAction;
use tensorplate_protocol::agent_state::TransactionKind;
use tensorplate_protocol::backend_descriptor::ComputeType;
use tensorplate_protocol::deploy_transaction::DeployState;

use crate::recovery::apply_startup;

const STT: &str = "test/models/bundles/v0_2/speech_stt_streaming";
const TTS: &str = "test/models/bundles/v0_2/speech_tts_streaming";
const FORMAT_0_1: &str = "test/models/bundles/v0_1/x86_fixture_smoke";

impl Agent {
    fn deploy(&self, id: &str, bundle: &str) -> AgentResult<()> {
        let labels = BTreeMap::new();
        let deployed = self
            .coordinator
            .deploy(id, &repo(bundle), labels, None, None);
        deployed.map(|_| ())
    }
}

impl Host {
    /// Where a deploy stages `id`.
    fn staged_at(&self, id: &str) -> String {
        let staged = self.td.path().join("staging").join(id);
        staged.display().to_string()
    }
    /// What the candidate record names, as the view shows a record.
    fn candidate(&self) -> String {
        let read = StateStore::open(self.state_dir()).unwrap();
        shown(&self.state_dir(), &read.snapshot().unwrap().candidate)
    }
    /// The descriptor a deploy of the speech-to-text fixture as `stt` derives
    /// for `generation`.
    fn derived(&self, generation: u64) -> DeploymentDescriptor {
        derive("stt", generation, &self.staged_at("stt"))
    }
}

/// The worker control of a start that finds `serving` being served.
fn reporting(serving: Option<&str>) -> Worker {
    serving.map_or_else(Worker::default, Worker::serving)
}

#[test]
fn a_deploy_a_start_of_the_agent_and_a_rollback_each_start_from_one_descriptor_of_one_configuration(
) {
    let host = host();
    let (agent, _) = host.start(Worker::default());

    agent.deploy("stt", STT).unwrap();

    // Generation 1 was taken under a hold that began before it, and its
    // descriptor named and written before a worker was asked for.
    let first = host.derived(1);
    let configuration = &first.configuration;
    let profile = configuration.runner_profile.as_ref().map(|p| &*p.id);
    assert_eq!(profile, Some("faster_whisper"));
    assert_eq!(configuration.compute_type, Some(ComputeType::Float16));
    let asked = agent.asked();
    let unnamed = ["-", "-", "stt", "-"].map(String::from);
    assert_eq!(asked.held_since, Some(unnamed));
    assert_eq!(asked.view, ["-", "-", "stt@1", "2"]);
    assert_eq!(asked.candidate, "stt@1");
    // The agent is configured with another runtime version than it runs.
    assert_ne!(CONFIGURED_RUNTIME, configuration.runtime_version);
    assert_eq!(asked.reference, reference(&first));
    assert_eq!(asked.resolved.as_ref(), Some(&first));
    assert_eq!(host.view(), ["stt@1", "-", "-", "2"]);
    assert_eq!(host.documents(), "stt-1");

    // A start that finds nothing serving restores it at generation 2.
    let (agent, pruned) = host.start(Worker::default());
    assert_eq!(pruned, 0);
    apply_startup(&agent.coordinator).unwrap();
    let second = first.restamp(2).unwrap();
    assert_eq!(agent.asked().resolved.as_ref(), Some(&second));

    // Another deployment replaces it; a rollback puts it back at generation 4.
    agent.deploy("tts", TTS).unwrap();
    assert_eq!(agent.asked().view, ["stt@2", "-", "tts@3", "4"]);
    agent.coordinator.rollback(None).unwrap();
    let fourth = second.restamp(4).unwrap();
    assert_eq!(agent.asked().resolved.as_ref(), Some(&fourth));
    assert_eq!(host.view(), ["stt@4", "tts@3", "-", "5"]);

    let started = [&first, &second, &fourth];
    let configured =
        |d: &&DeploymentDescriptor| d.configuration_digest == first.configuration_digest;
    assert!(started.iter().all(configured));
    let described: BTreeSet<_> = started.iter().map(|d| &d.descriptor_digest).collect();
    assert_eq!(described.len(), 3);
}

#[test]
fn a_format_0_1_deploy_names_nothing_writes_no_document_and_takes_no_hold() {
    let host = host();
    let (agent, _) = host.start(Worker::default());

    agent.deploy("smoke", FORMAT_0_1).unwrap();

    // The reference a deploy handed over before any descriptor existed.
    let reference = CandidateRef {
        deployment_id: "smoke".into(),
        staged_path: host.staged_at("smoke"),
        bundle_digest: parse_bundle(&repo(FORMAT_0_1)).unwrap().manifest_digest,
        generation: None,
        descriptor_digest: None,
        configuration_digest: None,
        backend_hint: "python_pytorch".into(),
        model_class: "custom".into(),
        bundle_name: Some("x86-fixture-smoke".into()),
        bundle_version: Some("0.1.0".into()),
        artifact_relative_path: Some("x86-smoke.json".into()),
    };
    let asked = agent.asked();
    assert_eq!(asked.reference, reference);
    assert_eq!((asked.held_since, asked.resolved), (None, None));
    assert_eq!(asked.view, ["-", "-", "smoke", "-"]);
    assert_eq!(asked.candidate, "smoke");
    assert_eq!(host.view(), ["smoke", "-", "-", "-"]);

    // The state directory holds the state pair alone, at state version 0.1.
    let state = fs::read_to_string(host.state_dir().join("state.json")).unwrap();
    assert!(state.contains(r#""schema_version": "0.1""#), "{state}");
    for key in ["generation", "descriptor_digest", "configuration_digest"] {
        assert!(!state.contains(key), "{key}: {state}");
    }
    let entries = fs::read_dir(host.state_dir()).unwrap();
    let mut names: Vec<_> = entries
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["state.json", "state.json.bak"]);
}

#[test]
fn a_deploy_naming_a_runner_profile_is_refused_by_an_agent_that_holds_no_probe_of_its_backend() {
    let host = host();
    let (agent, _) = host.start_probed(Worker::default(), BTreeMap::new());

    let error = agent.deploy("stt", STT).unwrap_err().to_record();

    let unsupported = error.code == ErrorCode::Unsupported && !error.recoverable;
    let says = "of `stt` is not derived: runner profile `faster_whisper` is not installed";
    assert!(unsupported && error.message.ends_with(says), "{error:?}");
    // Nothing is named, no generation is taken and no worker is asked for.
    assert_eq!(host.view(), ["-", "-", "-", "-"]);
    let (quarantined, is_last_error) = host.quarantined();
    assert!(quarantined.ends_with("@CapacityChecked") && is_last_error);
    assert!(agent.never_asked() && host.documents().is_empty());
}

#[test]
fn a_deploy_takes_no_generation_when_its_document_cannot_be_written() {
    let host = host();
    let (agent, _) = host.start(Worker::default());
    let unfinished = host.state_dir().join("descriptor-stt-1.json.tmp");
    fs::create_dir(&unfinished).unwrap();

    let error = agent.deploy("stt", STT).unwrap_err();

    assert!(matches!(error, AgentError::Io(_)), "{error}");
    assert_eq!(host.view(), ["-", "-", "-", "-"]);
    assert!(agent.never_asked());
    fs::remove_dir(unfinished).unwrap();
    assert!(host.documents().is_empty());
    agent.deploy("stt", STT).unwrap();
    assert_eq!(host.view(), ["stt@1", "-", "-", "2"]);
}

/// An agent started over what a same-id update that did not complete left:
/// `stt` recorded active as the speech-to-text bundle, and the other bundle
/// staged under the id. The update was refused after staging.
fn started_after_an_update_that_did_not_complete(host: &Host) -> Agent {
    let (agent, _) = host.start(Worker::default());
    agent.deploy("stt", STT).unwrap();
    drop(agent.asked());
    let unfinished = host.state_dir().join("descriptor-stt-2.json.tmp");
    fs::create_dir(&unfinished).unwrap();
    agent.deploy("stt", TTS).unwrap_err();
    fs::remove_dir(unfinished).unwrap();
    assert_eq!(host.view(), ["stt@1", "-", "-", "2"]);

    let (next, pruned) = host.start(Worker::default());
    assert_eq!(pruned, 0);
    let summary = apply_startup(&next.coordinator).unwrap();
    assert_eq!(summary.action, RecoveryAction::RestoreActive);
    next
}

/// Whether the last error is that of a start that did not restore `stt`
/// because another bundle is staged under its id.
fn says_another_bundle_is_staged(host: &Host) -> bool {
    let Some(error) = host.last_error() else {
        return false;
    };
    let load_failed = error.code == ErrorCode::LoadFailed && !error.recoverable;
    let says = "`stt` is not started again: the bundle staged at";
    let remedy = "names; `stt` is not serving until it is deployed again";
    load_failed && error.message.contains(says) && error.message.ends_with(remedy)
}

#[test]
fn a_start_after_a_same_id_update_that_did_not_complete_leaves_the_agent_up_and_the_deployment_not_serving(
) {
    let host = host();

    // Staging is keyed by the id alone: the record's bundle is gone from it.
    let next = started_after_an_update_that_did_not_complete(&host);

    // No worker is asked for, no generation is taken and nothing is written.
    assert!(next.never_asked() && says_another_bundle_is_staged(&host));
    assert_eq!(host.view(), ["stt@1", "-", "-", "2"]);
    assert_eq!(host.documents(), "stt-1");
}

#[test]
fn a_deploy_of_the_id_through_an_agent_that_did_not_restore_it_succeeds_and_the_next_start_restores_it(
) {
    // As the bundle the update staged, and as the one the record names.
    for bundle in [TTS, STT] {
        let host = host();
        let next = started_after_an_update_that_did_not_complete(&host);

        next.deploy("stt", bundle).unwrap();

        assert_eq!(next.asked().view, ["stt@1", "-", "stt@2", "3"]);
        assert_eq!(host.view(), ["stt@2", "stt@1", "-", "3"]);
        assert_eq!(host.last_error(), None);

        let (after, pruned) = host.start(Worker::default());
        assert_eq!(pruned, 0);
        let summary = apply_startup(&after.coordinator).unwrap();
        assert_eq!(summary.action, RecoveryAction::RestoreActive);
        let restored = after.asked().resolved.unwrap();
        let deployed = parse_bundle(&repo(bundle)).unwrap().manifest_digest;
        assert_eq!(restored.configuration.bundle.bundle_digest, deployed);
        assert_eq!(host.view(), ["stt@3", "stt@1", "-", "4"], "{bundle}");
        assert_eq!(host.last_error(), None);
    }
}

#[test]
fn a_same_id_update_killed_after_prepared_is_quarantined_by_the_first_start_and_later_starts_come_up_with_the_error_recorded(
) {
    let host = host();
    let (agent, _) = host.start(Worker::killed_in("warm", 2));
    agent.deploy("stt", STT).unwrap();
    drop(agent.asked());

    let error = agent.deploy("stt", TTS).unwrap_err();

    assert!(matches!(error, AgentError::Stopping), "{error}");
    refuse_state_writes(&host.state_dir(), false);
    drop(agent.asked());
    assert_eq!(host.view(), ["stt@1", "-", "stt@2", "3"]);

    // The first start quarantines the candidate and restores nothing.
    let (first, pruned) = host.start(Worker::default());
    assert_eq!(pruned, 0);
    let summary = apply_startup(&first.coordinator).unwrap();
    assert_eq!(summary.action, RecoveryAction::QuarantineCandidate);
    let (quarantined, is_last_error) = host.quarantined();
    assert!(quarantined.ends_with("@Prepared") && is_last_error);
    assert!(first.never_asked() && host.view() == ["stt@1", "-", "-", "3"]);

    // Each later start finds the candidate's bundle staged under the id.
    for removed in [1, 0] {
        let (later, pruned) = host.start(Worker::default());
        assert_eq!(pruned, removed);
        let summary = apply_startup(&later.coordinator).unwrap();
        assert_eq!(summary.action, RecoveryAction::RestoreActive);
        assert!(later.never_asked() && says_another_bundle_is_staged(&host));
        assert_eq!(host.view(), ["stt@1", "-", "-", "3"]);
        assert_eq!(host.documents(), "stt-1");
    }
}

#[test]
fn a_same_id_update_that_a_start_resumes_and_then_refuses_fails_that_start_and_the_next_comes_up() {
    let host = host();
    let (agent, _) = host.start(Worker::killed_in("prepare", 2));
    agent.deploy("stt", STT).unwrap();
    drop(agent.asked());
    // The update is deployed from a copy that is gone by the next start.
    let source = host.td.path().join("update");
    fs::create_dir_all(source.join("warmup")).unwrap();
    for file in ["manifest.json", "entry.json", "warmup/segment.txt"] {
        fs::copy(repo(TTS).join(file), source.join(file)).unwrap();
    }
    let labels = BTreeMap::new();
    let update = agent.coordinator.deploy("stt", &source, labels, None, None);
    assert!(matches!(update, Err(AgentError::Stopping)), "{update:?}");
    refuse_state_writes(&host.state_dir(), false);
    drop(agent.asked());
    assert_eq!(host.view(), ["stt@1", "-", "stt@2", "3"]);
    fs::remove_dir_all(&source).unwrap();

    // The resumed deploy is refused, which fails that start as before.
    let (resuming, _) = host.start(Worker::default());
    let error = apply_startup(&resuming.coordinator).unwrap_err();
    assert!(matches!(error, AgentError::BundleMissing(_)), "{error}");
    assert!(resuming.never_asked() && host.view() == ["stt@1", "-", "-", "3"]);

    let (next, _) = host.start(Worker::default());
    let summary = apply_startup(&next.coordinator).unwrap();
    assert_eq!(summary.action, RecoveryAction::RestoreActive);
    assert!(next.never_asked() && says_another_bundle_is_staged(&host));
    assert_eq!(host.view(), ["stt@1", "-", "-", "3"]);
}

#[test]
fn a_rollback_after_a_deploy_of_another_bundle_under_the_same_id_is_refused() {
    let host = host();
    let (agent, _) = host.start(Worker::default());
    agent.deploy("stt", STT).unwrap();
    drop(agent.asked());
    agent.deploy("stt", TTS).unwrap();
    assert_eq!(agent.asked().view, ["stt@1", "-", "stt@2", "3"]);

    // The earlier record's bundle is no longer staged under the id.
    let error = agent.coordinator.rollback(None).unwrap_err().to_record();

    let load_failed = error.code == ErrorCode::LoadFailed && !error.recoverable;
    let says = "`stt` is not started again: the bundle staged at";
    assert!(load_failed && error.message.contains(says), "{error:?}");
    assert_eq!(host.view(), ["stt@2", "stt@1", "-", "3"]);
    assert!(agent.never_asked() && host.last_error() == Some(error));
}

/// What a deploy of `stt` hands [`start_deploy`].
struct Handed {
    record: DeploymentRecord,
    transaction_id: &'static str,
    probes: BTreeMap<String, BackendProbeReport>,
}

impl Handed {
    fn start<'w>(&self, agent: &'w Agent) -> AgentResult<InHand<'w>> {
        let deploy = Deploy {
            transaction_id: self.transaction_id,
            record: &self.record,
            artifact_relative_path: Some("entry.json".into()),
            format_version: "0.2",
            probes: &self.probes,
        };
        start_deploy(&agent.store, agent.worker.as_ref(), &deploy)
    }
}

#[test]
fn a_bundle_of_another_format_is_referenced_without_its_staged_copy_being_read() {
    let host = host();
    let (agent, mut handed) = about_to_prepare(&host);
    handed.record.staged_path = host.staged_at("gone");
    let deploy = Deploy {
        transaction_id: "tx",
        record: &handed.record,
        artifact_relative_path: None,
        format_version: "0.1",
        probes: &handed.probes,
    };
    let before = (host.view(), host.candidate());

    let in_hand = start_deploy(&agent.store, agent.worker.as_ref(), &deploy).unwrap();

    assert_eq!(identity_of(in_hand.reference()).unwrap(), None);
    drop(in_hand);
    assert_eq!((host.view(), host.candidate()), before);
    assert!(agent.never_asked() && host.documents().is_empty());
}

/// Put `candidate` in the candidate slot.
fn recorded(agent: &Agent, candidate: &DeploymentRecord) {
    let record = |state: &mut AgentState| {
        state.candidate = Some(candidate.clone());
        Ok(())
    };
    agent.store.update(record).unwrap();
}

/// A started agent whose deploy of `stt`, transaction `tx`, has recorded
/// its candidate and is about to ask for its worker, and what it hands over.
fn about_to_prepare(host: &Host) -> (Agent, Handed) {
    let (agent, _) = host.start(Worker::default());
    let staged = host.staged("stt").display().to_string();
    let candidate = DeploymentRecord {
        generation: None,
        descriptor_digest: None,
        configuration_digest: None,
        promoted_monotonic_ns: None,
        ..record(&derive("stt", 1, &staged))
    };
    agent.in_flight(TransactionKind::Deploy, "tx", "stt");
    recorded(&agent, &candidate);
    let handed = Handed {
        record: candidate,
        transaction_id: "tx",
        probes: probes(),
    };
    (agent, handed)
}

/// The state of a deploy of `stt` killed after it recorded `phase`.
fn killed_after(host: &Host, phase: DeployState) {
    let (agent, handed) = about_to_prepare(host);
    drop(handed.start(&agent).unwrap());
    agent.store.record_phase("tx", phase, 1).unwrap();
    assert_eq!(host.view(), ["-", "-", "stt@1", "2"]);
    assert_eq!(host.candidate(), "stt@1");
    assert!(agent.never_asked());
}

#[test]
fn a_deploy_is_refused_unless_its_staged_copy_its_records_and_the_installation_all_stand() {
    type Broken = fn(&Host, &Agent, &mut Handed);
    let load_failed = (ErrorCode::LoadFailed, false);
    let config_invalid = (ErrorCode::ConfigInvalid, false);
    let not_in_flight = "of `stt` is not derived: deploy transaction `tx` is not in flight";
    let cases: [(Broken, (ErrorCode, bool), &str); 9] = [
        (
            |host, _, _| drop(host.restage()),
            load_failed,
            "`stt` is not started: the bundle staged at",
        ),
        (
            |host, _, _| fs::write(host.staged_at("stt") + "/entry.json", "{}").unwrap(),
            load_failed,
            "bundle artifact `entry.json` failed integrity check",
        ),
        (
            |_, _, handed| handed.record.bundle_version = "0.2.1".into(),
            load_failed,
            "of `stt` is not derived: the candidate record is not its deploy's",
        ),
        (
            |_, _, handed| handed.transaction_id = "another",
            load_failed,
            "of `stt` is not derived: deploy transaction `another` is not in flight",
        ),
        (
            |_, agent, _| agent.in_flight(TransactionKind::Rollback, "tx", "stt"),
            load_failed,
            not_in_flight,
        ),
        (
            |_, agent, _| agent.in_flight(TransactionKind::Deploy, "tx", "tts"),
            load_failed,
            not_in_flight,
        ),
        (
            |_, _, handed| {
                let report = handed.probes.get_mut("python_pytorch").unwrap();
                report.runner_profiles[0].profile.compute_types = vec![ComputeType::Float32];
            },
            (ErrorCode::Unsupported, false),
            "runner profile `faster_whisper` cannot load compute type `float16`",
        ),
        // The profile is installed, under another backend than the bundle's.
        (
            |_, _, handed| {
                let report = handed.probes.remove("python_pytorch").unwrap();
                handed.probes.insert("tensorrt".into(), report);
            },
            (ErrorCode::Unsupported, false),
            "of `stt` is not derived: runner profile `faster_whisper` is not installed",
        ),
        (
            |_, agent, handed| {
                handed.record.staged_path = handed.record.staged_path.replace("/stt", "/./stt");
                recorded(agent, &handed.record);
            },
            config_invalid,
            "of `stt` is not derived: deployment descriptor `staged_path`",
        ),
    ];
    for (case, (broken, refused_as, says)) in cases.into_iter().enumerate() {
        let host = host();
        let (agent, mut handed) = about_to_prepare(&host);
        broken(&host, &agent, &mut handed);
        let before = (host.view(), host.candidate());

        let refused = handed.start(&agent).map(|_| ());

        let error = refused.unwrap_err().to_record();
        assert_eq!((error.code, error.recoverable), refused_as, "case {case}");
        assert!(error.message.contains(says), "case {case}: {error:?}");
        // Nothing is named, no generation is taken and nothing is written.
        assert_eq!((host.view(), host.candidate()), before, "case {case}");
        assert!(agent.never_asked() && host.documents().is_empty());
    }
}

#[test]
fn a_deploy_killed_after_its_document_is_resumed_under_the_same_generation() {
    for serving in [None, Some("stt")] {
        let host = host();
        let (agent, _) = host.start(Worker::killed_at_its_hold());

        agent.deploy("stt", STT).unwrap_err();

        // The document is written, nothing names it and the counter stands.
        refuse_state_writes(&host.state_dir(), false);
        assert_eq!(host.view(), ["-", "-", "stt", "-"]);
        assert_eq!(host.candidate(), "stt");
        assert!(agent.never_asked() && host.documents() == "stt-1");

        let (next, pruned) = host.start(reporting(serving));
        assert_eq!((pruned, host.documents().as_str()), (1, ""));
        let summary = apply_startup(&next.coordinator).unwrap();
        assert_eq!(summary.action, RecoveryAction::ResumePrepare);
        assert_eq!(next.asked().resolved, Some(host.derived(1)));
        assert_eq!(host.view(), ["stt@1", "-", "-", "2"], "{serving:?}");
    }
}

#[test]
fn a_deploy_killed_after_it_named_its_generation_is_resumed_under_a_new_one() {
    for serving in [None, Some("stt")] {
        let host = host();
        let (agent, _) = host.start(Worker::killed());

        agent.deploy("stt", STT).unwrap_err();

        refuse_state_writes(&host.state_dir(), false);
        assert_eq!(host.view(), ["-", "-", "stt@1", "2"]);
        assert_eq!(host.candidate(), "stt@1");

        // Every document is named, so the next start removes none; the
        // resumed deploy takes another generation.
        let (next, pruned) = host.start(reporting(serving));
        assert_eq!(pruned, 0);
        let summary = apply_startup(&next.coordinator).unwrap();
        assert_eq!(summary.action, RecoveryAction::ResumePrepare);
        assert_eq!(next.asked().resolved, Some(host.derived(2)));
        assert_eq!(host.view(), ["stt@2", "-", "-", "3"], "{serving:?}");
        assert_eq!(host.documents(), "stt-1 stt-2");
        assert_eq!(host.start(Worker::default()).1, 1);
        assert_eq!(host.documents(), "stt-2");
    }
}

#[test]
fn a_deploy_killed_before_its_promotion_is_recorded_is_quarantined_by_a_start_that_finds_nothing_serving(
) {
    use DeployState::{Prepared, Promoted, Warmed};
    for phase in [Prepared, Warmed, Promoted] {
        let host = host();
        killed_after(&host, phase);

        let (next, pruned) = host.start(Worker::default());
        assert_eq!(pruned, 0, "{phase:?}");
        let summary = apply_startup(&next.coordinator).unwrap();
        assert_eq!(summary.action, RecoveryAction::QuarantineCandidate);
        assert_eq!(host.view(), ["-", "-", "-", "2"]);
        assert_eq!(host.candidate(), "-");
        assert_eq!(host.quarantined(), (format!("tx@{phase:?}"), true));
        assert!(next.never_asked() && host.documents() == "stt-1");

        // No record names the candidate's document now: the start after
        // that removes it.
        assert_eq!(host.start(Worker::default()).1, 1, "{phase:?}");
    }
}

#[test]
fn a_deploy_killed_before_its_promotion_is_recorded_is_finalized_when_its_deployment_is_serving() {
    use DeployState::{Prepared, Promoted, Warmed};
    for phase in [Prepared, Warmed, Promoted] {
        let host = host();
        killed_after(&host, phase);

        let (next, pruned) = host.start(Worker::serving("stt"));
        assert_eq!(pruned, 0, "{phase:?}");
        let summary = apply_startup(&next.coordinator).unwrap();
        assert_eq!(summary.action, RecoveryAction::FinalizePromotion);
        assert_eq!(host.view(), ["stt@1", "-", "-", "2"]);
        assert_eq!(host.candidate(), "-");
        assert!(next.never_asked() && host.documents() == "stt-1");
    }
}

#[test]
fn a_deploy_killed_after_its_promotion_was_recorded_leaves_the_active_record_naming_its_descriptor()
{
    for serving in [None, Some("stt")] {
        let host = host();
        killed_after(&host, DeployState::Promoted);
        let store = StateStore::open(host.state_dir()).unwrap();
        store.promote_candidate(2).unwrap();
        assert_eq!(host.view(), ["stt@1", "-", "stt@1", "2"]);

        let (next, pruned) = host.start(reporting(serving));
        assert_eq!(pruned, 0);
        let summary = apply_startup(&next.coordinator).unwrap();
        assert_eq!(host.view(), ["stt@1", "-", "-", "2"], "{serving:?}");
        assert!(next.never_asked() && host.documents() == "stt-1");
        if serving.is_some() {
            assert_eq!(summary.action, RecoveryAction::FinalizePromotion);
            assert_eq!(host.last_error(), None);
            continue;
        }
        assert_eq!(summary.action, RecoveryAction::QuarantineCandidate);
        assert_eq!(host.quarantined(), ("tx@Promoted".to_owned(), true));

        // The start after it restores the record at the next generation.
        let (after, pruned) = host.start(Worker::default());
        assert_eq!(pruned, 0);
        let summary = apply_startup(&after.coordinator).unwrap();
        assert_eq!(summary.action, RecoveryAction::RestoreActive);
        let second = host.derived(1).restamp(2).unwrap();
        assert_eq!(after.asked().resolved, Some(second));
        assert_eq!(host.view(), ["stt@2", "-", "-", "3"]);
    }
}

#[test]
fn status_reports_the_generation_a_retained_record_was_last_started_as() {
    // A record that names no descriptor has none to report.
    let cases = [
        (["stt", "tts"], [STT, TTS], Some(1)),
        (["smoke", "smoke-2"], [FORMAT_0_1; 2], None),
    ];
    for (ids, bundles, retained) in cases {
        let host = host();
        let (agent, _) = host.start(Worker::default());
        agent.deploy(ids[0], bundles[0]).unwrap();
        agent.deploy(ids[1], bundles[1]).unwrap();

        let status = agent.coordinator.status().unwrap();

        let previous = status.previous_active.unwrap();
        assert_eq!(
            (&*previous.deployment_id, previous.generation),
            (ids[0], retained)
        );
        // `active` reports the generation serving, which this worker
        // control does not report.
        assert_eq!(status.active.unwrap().generation, None);
    }
}
