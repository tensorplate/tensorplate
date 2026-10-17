// SPDX-License-Identifier: Apache-2.0

//! Qualification deploys: the test count each generation of the deployment
//! is started with, what records it, and the refusals before staging; and
//! the input formats a described speech bundle's stream block carries.

use super::*;
use tensorplate_protocol::agent_control::{
    ControlRequest, DeployRequest, ResponseStatus, StatusRequest,
};
use tensorplate_protocol::{AdmissionMode, DeploymentDescriptor, DomainQuotaBytes};

/// A copy of the STT fixture bundle under `root` declaring `sessions`.
fn declaring(root: &TempDir, sessions: u32) -> PathBuf {
    edited(root, &format!("stt-declaring-{sessions}"), |manifest| {
        manifest["max_concurrent_sessions"] = sessions.into();
    })
}

/// A copy named `name` under `root` of the STT fixture bundle, whose manifest
/// `edit` changes.
fn edited(root: &TempDir, name: &str, edit: impl FnOnce(&mut serde_json::Value)) -> PathBuf {
    let source = PathBuf::from(bundle_fixture(STT));
    let copy = root.path().join(name);
    fs::create_dir_all(&copy).unwrap();
    for entry in fs::read_dir(&source).unwrap() {
        let name = entry.unwrap().file_name();
        fs::copy(source.join(&name), copy.join(&name)).unwrap();
    }
    let path = copy.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    edit(&mut manifest);
    fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
    copy
}

/// Where `config` breaks the serving worker's configuration schema.
fn worker_config_errors(config: &serde_json::Value) -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../config/schemas/serving_worker.json");
    let schema = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let compiled = jsonschema::JSONSchema::compile(&schema).unwrap();
    let errors = compiled.validate(config).err();
    errors.map_or_else(Vec::new, |errors| errors.map(|e| e.to_string()).collect())
}

fn pcm_16k() -> serde_json::Value {
    serde_json::json!({"encoding": "pcm_s16le", "sample_rate_hz": 16000, "channels": 1})
}

fn mulaw_8k() -> serde_json::Value {
    serde_json::json!({"encoding": "mulaw", "sample_rate_hz": 8000, "channels": 1})
}

fn unmeasured(session_count: u32) -> MemberQuota {
    MemberQuota {
        session_count,
        domain_bytes: DomainQuotaBytes::default(),
    }
}

impl Wired {
    fn qualify(&self, id: &str, bundle: &Path, test_count: u32) -> AgentResult<()> {
        let serving = Serving::Qualification { test_count };
        let labels = BTreeMap::new();
        let deployed = self
            .coordinator
            .deploy_with(id, bundle, labels, None, None, serving);
        deployed.map(|_| ())
    }
    fn stream(&self, id: &str, bundle: &Path) -> AgentResult<()> {
        let labels = BTreeMap::new();
        let deployed =
            self.coordinator
                .deploy_with(id, bundle, labels, None, None, Serving::Streaming);
        deployed.map(|_| ())
    }
    /// The `streaming` block generation `generation` was started with.
    fn rendered(&self, generation: u64) -> serde_json::Value {
        let config = &self.f.launcher.handle(generation).config;
        assert_eq!(worker_config_errors(config), Vec::<String>::new());
        config["streaming"].clone()
    }
    /// The descriptor `active` names.
    fn active_descriptor(&self) -> DeploymentDescriptor {
        let record = self.store.snapshot().unwrap().active.unwrap();
        let identity = record.descriptor_identity().unwrap().unwrap();
        let descriptors = DescriptorStore::new(self.store.state_dir());
        descriptors.read(&record.deployment_id, identity).unwrap()
    }
    fn staged(&self, id: &str) -> bool {
        self.root.path().join("staging").join(id).exists()
    }
    /// The admission mode and test count status reports on `active`.
    fn reported_admission(&self) -> (Option<AdmissionMode>, Option<u32>) {
        let (reported, _) = active_of(self);
        (reported.admission_mode, reported.test_count)
    }
}

#[test]
fn a_qualification_deploy_starts_its_worker_with_the_test_count_and_records_it_unqualified() {
    let w = wired_with(Step::BeforePrepare, streams);
    let request = DeployRequest {
        bundle_path: declaring(&w.root, 4).display().to_string(),
        deployment_id: "stt".into(),
        streaming: true,
        admission_mode: Some(AdmissionMode::Qualification),
        test_count: Some(3),
        ..DeployRequest::default()
    };
    let deploy = ControlRequest::deploy(None, request);
    let answered = crate::control::dispatch(&w.coordinator, deploy).unwrap();
    assert_eq!(answered.status, ResponseStatus::Ok, "{answered:?}");

    let port = ACTIVE_PORT + 10;
    let expected = serde_json::json!({
        "enabled": true,
        "port": port,
        "max_sessions": 3,
        "input_audio_formats": [pcm_16k(), mulaw_8k()]
    });
    assert_eq!(w.rendered(1), expected);
    let described = w.active_descriptor();
    assert_eq!(described.admission_mode, AdmissionMode::Qualification);
    assert_eq!(described.configuration.quota, unmeasured(3));

    let status = ControlRequest::status(None, StatusRequest::default());
    let answered = crate::control::dispatch(&w.coordinator, status).unwrap();
    let active = answered.agent_status.unwrap().active.unwrap();
    assert_eq!(active.admission_mode, Some(AdmissionMode::Qualification));
    assert_eq!(active.test_count, Some(3));
}

#[test]
fn status_reports_no_admission_for_a_descriptor_that_does_not_read_back() {
    let w = wired_with(Step::BeforePrepare, streams);
    w.qualify("stt", &declaring(&w.root, 4), 3).unwrap();
    let qualified = (Some(AdmissionMode::Qualification), Some(3));
    assert_eq!(w.reported_admission(), qualified);

    let path = w.store.state_dir().join("descriptor-stt-1.json");
    let text = fs::read_to_string(&path).unwrap();
    let altered = text.replace("\"qualification\"", "\"production\"");
    assert_ne!(altered, text);
    fs::write(&path, altered).unwrap();
    assert_eq!(w.reported_admission(), (None, None));
}

#[test]
fn a_test_count_above_the_declared_sessions_is_refused_before_staging_and_one_at_them_is_not() {
    let w = wired_with(Step::BeforePrepare, streams);
    let bundle = declaring(&w.root, 4);

    let refused = w.qualify("stt", &bundle, 5).unwrap_err();
    assert!(matches!(refused, AgentError::Config(_)), "{refused}");
    let message = refused.to_string();
    assert!(
        message.contains("`test_count` 5 exceeds the 4 sessions"),
        "{message}"
    );
    assert!(!w.staged("stt"));
    assert_eq!(w.f.launcher.count(), 0);

    w.qualify("stt", &bundle, 4).unwrap();
    assert_eq!(w.rendered(1)["max_sessions"], 4);
}

#[test]
fn a_rollback_and_the_next_start_start_a_qualification_deployment_with_its_test_count_again() {
    let w = wired_with(Step::BeforePrepare, streams);
    w.qualify("stt", &declaring(&w.root, 4), 3).unwrap();
    let qualified = w.active_descriptor();
    w.deploy("tts", TTS).unwrap();

    assert_eq!(
        w.reported_admission(),
        (Some(AdmissionMode::Production), None)
    );

    w.coordinator.rollback(None).unwrap();
    let admitted = (Some(AdmissionMode::Qualification), Some(3));
    assert_eq!(w.reported_admission(), admitted);
    let back = w.active_descriptor();
    assert_eq!(back.generation, 3);
    assert_eq!(back.admission_mode, AdmissionMode::Qualification);
    assert_eq!(back.configuration_digest, qualified.configuration_digest);
    assert_eq!(w.rendered(3)["max_sessions"], 3);
    let declared = serde_json::json!([pcm_16k(), mulaw_8k()]);
    assert_eq!(w.rendered(3)["input_audio_formats"], declared);

    let next = wired_at(w.root.clone(), Step::BeforePrepare, streams);
    apply_startup(&next.coordinator).unwrap();
    let restored = next.active_descriptor();
    assert_eq!(restored.generation, 4);
    assert_eq!(restored.admission_mode, AdmissionMode::Qualification);
    assert_eq!(
        restored.configuration_digest,
        qualified.configuration_digest
    );
    assert_eq!(next.rendered(4)["max_sessions"], 3);
    assert_eq!(next.rendered(4)["input_audio_formats"], declared);
    assert_eq!(next.reported_admission(), admitted);
}

#[test]
fn a_production_deploy_of_a_bundle_qualified_before_renders_no_count_and_records_production() {
    let w = wired_with(Step::BeforePrepare, streams);
    let bundle = declaring(&w.root, 4);
    w.qualify("stt", &bundle, 3).unwrap();
    let qualified = w.active_descriptor();
    w.f.health(CANDIDATE_PORT, "stt");

    w.stream("stt", &bundle).unwrap();
    let production = w.active_descriptor();
    assert_eq!(
        w.reported_admission(),
        (Some(AdmissionMode::Production), None)
    );
    assert_eq!(production.generation, 2);
    assert_eq!(production.admission_mode, AdmissionMode::Production);
    assert_eq!(production.configuration.quota, MemberQuota::default());
    assert_ne!(
        production.configuration_digest,
        qualified.configuration_digest
    );
    let rendered = w.rendered(2);
    assert_eq!(rendered["enabled"], true);
    assert_eq!(rendered.get("max_sessions"), None);
}

#[test]
fn a_test_count_the_worker_refuses_fails_the_deploy_with_its_reason_and_the_serving_member_stays() {
    let w = wired_with(Step::BeforePrepare, streams);
    w.deploy_streaming("stt", STT).unwrap();

    let refused = w.qualify("tts", &declaring(&w.root, 100), 65).unwrap_err();
    match &refused {
        AgentError::WorkerStartupFailed { code, message } => {
            assert_eq!(*code, ErrorCode::ConfigInvalid);
            assert!(message.contains("from 1 to 64"), "{message}");
        }
        other => panic!("expected the worker's startup failure, got {other:?}"),
    }
    assert_eq!(refused.to_record().code, ErrorCode::ConfigInvalid);
    assert_eq!(w.rendered(2)["max_sessions"], 65);
    let (reported, recorded) = active_of(&w);
    assert_eq!(recorded.deployment_id, "stt");
    assert_eq!(reported.generation, Some(1));
}

#[test]
fn a_stream_deploy_renders_the_input_formats_its_descriptor_declares_beyond_16_khz_pcm() {
    let w = wired_with(Step::BeforePrepare, streams);
    w.stream("stt", Path::new(&bundle_fixture(STT))).unwrap();

    let declared = w.active_descriptor().configuration.speech.unwrap();
    assert_eq!(declared.input_audio_formats.len(), 2);
    let rendered = w.rendered(1);
    assert_eq!(
        rendered["input_audio_formats"],
        serde_json::json!([pcm_16k(), mulaw_8k()])
    );
    assert_eq!(rendered.get("max_sessions"), None);
}

// A worker that predates the key refuses it, so a descriptor that declares
// no more than the worker's default renders none.
#[test]
fn a_stream_deploy_whose_descriptor_declares_only_16_khz_pcm_renders_no_input_formats() {
    let w = wired_with(Step::BeforePrepare, streams);
    let bundle = edited(&w.root, "stt-pcm-only", |manifest| {
        manifest["model_blocks"]["speech"]["input_audio_formats"] = serde_json::json!([pcm_16k()]);
    });
    w.stream("stt", &bundle).unwrap();

    let declared = w.active_descriptor().configuration.speech.unwrap();
    assert_eq!(declared.input_audio_formats.len(), 1);
    let port = ACTIVE_PORT + 10;
    assert_eq!(
        w.rendered(1),
        serde_json::json!({"enabled": true, "port": port})
    );
}
