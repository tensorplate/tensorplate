// SPDX-License-Identifier: Apache-2.0
//
// Format 0.2 fixtures under `test/models/bundles/v0_2/`. Both bundles parse
// through the shared parser and validate against the manifest schema; every
// change in `cases.json` gets its recorded verdict from both.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::{json, Value};
use tensorplate_protocol::backend_descriptor::ComputeType;
use tensorplate_protocol::bundle::{
    parse_bundle, parse_bundle_with, BundleDescriptor, ParseError, ParseOptions,
};
use tensorplate_protocol::{
    AudioEncoding, BudgetDomainName, DegradedProfile, SpeechChunking, SpeechServingMode,
    SpeechTask, StageOwnership, SupportLevel,
};

const STT: &str = "speech_stt_streaming";
const TTS: &str = "speech_tts_streaming";

fn repo_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

fn read_json(path: &Path) -> Value {
    let raw =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn fixture_dir(name: &str) -> PathBuf {
    repo_path("test/models/bundles/v0_2").join(name)
}

fn manifest(name: &str) -> Value {
    read_json(&fixture_dir(name).join("manifest.json"))
}

fn validator() -> &'static jsonschema::JSONSchema {
    static VALIDATOR: OnceLock<jsonschema::JSONSchema> = OnceLock::new();
    VALIDATOR.get_or_init(|| {
        let budget = read_json(&repo_path("config/schemas/memory_budget_breakdown.json"));
        let id = budget["$id"].as_str().expect("$id").to_owned();
        let mut options = jsonschema::JSONSchema::options();
        options.with_document(id, budget);
        options
            .compile(&read_json(&repo_path(
                "protocol/schemas/bundle_manifest.json",
            )))
            .expect("schema compiles as Draft-07")
    })
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).expect("read fixture dir") {
        let entry = entry.expect("dir entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            std::fs::create_dir_all(&target).expect("mkdir");
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

/// Parses `text` as the manifest of a copy of the bundle at `dir`.
fn parse_text(dir: &Path, text: &str) -> Result<BundleDescriptor, ParseError> {
    let copy = tempfile::tempdir().expect("temp dir");
    copy_tree(dir, copy.path());
    std::fs::write(copy.path().join("manifest.json"), text).expect("write manifest");
    parse_bundle_with(
        copy.path(),
        ParseOptions {
            verify_artifact_digests: false,
        },
    )
}

fn parent_and_key<'a>(value: &'a mut Value, pointer: &str) -> (&'a mut Value, String) {
    let (parent, key) = pointer
        .rsplit_once('/')
        .unwrap_or_else(|| panic!("`{pointer}` is not a JSON pointer"));
    let parent = value
        .pointer_mut(parent)
        .unwrap_or_else(|| panic!("no `{parent}` in the manifest"));
    (parent, key.to_string())
}

/// Applies a case's `remove`, then `set`, then `append` operations.
fn apply(manifest: &mut Value, case: &Value) {
    for pointer in case["remove"].as_array().into_iter().flatten() {
        let pointer = pointer.as_str().expect("pointer");
        let (parent, key) = parent_and_key(manifest, pointer);
        parent
            .as_object_mut()
            .and_then(|o| o.remove(&key))
            .unwrap_or_else(|| panic!("nothing to remove at `{pointer}`"));
    }
    for (pointer, new) in case["set"].as_object().into_iter().flatten() {
        let (parent, key) = parent_and_key(manifest, pointer);
        match parent {
            Value::Object(object) => {
                object.insert(key, new.clone());
            }
            Value::Array(array) => {
                let index: usize = key.parse().expect("array index");
                array[index] = new.clone();
            }
            _ => panic!("`{pointer}` has no container"),
        }
    }
    for (pointer, items) in case["append"].as_object().into_iter().flatten() {
        manifest
            .pointer_mut(pointer)
            .and_then(Value::as_array_mut)
            .unwrap_or_else(|| panic!("`{pointer}` is not an array"))
            .extend(items.as_array().expect("items").iter().cloned());
    }
}

#[test]
fn every_case_gets_its_verdict_from_schema_and_parser() {
    let cases = read_json(&repo_path("test/models/bundles/v0_2/cases.json"));
    let cases = cases["cases"].as_array().expect("cases");
    assert!(!cases.is_empty());
    for case in cases {
        let name = case["name"].as_str().expect("name");
        let fixture = case["fixture"].as_str().expect("fixture");
        let mut changed = manifest(fixture);
        apply(&mut changed, case);
        assert_ne!(
            changed,
            manifest(fixture),
            "{name}: the case changes nothing"
        );
        let (schema_expected, parser_expected) = match case["verdict"].as_str() {
            Some("accept") => (true, true),
            Some("reject") => (false, false),
            Some("reject_beyond_schema") => (true, false),
            other => panic!("{name}: unknown verdict {other:?}"),
        };
        let text = serde_json::to_string_pretty(&changed).expect("serialize");
        let parsed = parse_text(&fixture_dir(fixture), &text);
        assert_eq!(
            validator().is_valid(&changed),
            schema_expected,
            "{name}: schema verdict (parser: {parsed:?})"
        );
        assert_eq!(
            parsed.is_ok(),
            parser_expected,
            "{name}: parser verdict {parsed:?}"
        );
    }
}

#[test]
fn both_fixtures_validate_against_the_schema() {
    for name in [STT, TTS] {
        let manifest = manifest(name);
        let errors: Vec<String> = match validator().validate(&manifest) {
            Ok(()) => Vec::new(),
            Err(errors) => errors
                .map(|e| format!("{e} at {}", e.instance_path))
                .collect(),
        };
        assert!(errors.is_empty(), "{name}: {errors:#?}");
    }
}

#[test]
fn stt_fixture_parses_into_its_speech_contract() {
    let descriptor = parse_bundle(&fixture_dir(STT)).expect("STT fixture parses");
    let profile = descriptor.manifest.profile.expect("format 0.2 profile");
    assert_eq!(profile.runner_profile.as_deref(), Some("faster_whisper"));
    assert_eq!(profile.compute_type, Some(ComputeType::Float16));
    assert_eq!(profile.support_level, Some(SupportLevel::Experimental));
    assert_eq!(profile.degraded_profile, Some(DegradedProfile::Disabled));
    assert_eq!(profile.hardware_compatibility, ["ubuntu2404-x86-l4-g2s8"]);
    assert!(profile.warmup.is_none());
    let preprocessing = &profile.pipeline_stages[2];
    assert_eq!(preprocessing.stage, "preprocessing");
    assert_eq!(preprocessing.ownership, StageOwnership::RuntimeOwned);
    assert!(
        !preprocessing.observable,
        "the fused stage reports not_observable"
    );

    let speech = profile.speech.expect("speech contract");
    assert_eq!(speech.task, SpeechTask::Stt);
    assert_eq!(speech.serving_mode, SpeechServingMode::Streaming);
    assert_eq!(speech.wire_serving_mode(), Some("stt_streaming"));
    assert_eq!(speech.languages, ["en", "ar"]);
    assert!(speech.voices.is_empty());
    let formats: Vec<_> = speech
        .input_audio_formats
        .iter()
        .map(|f| (f.encoding, f.sample_rate_hz, f.channels))
        .collect();
    assert_eq!(
        formats,
        [
            (AudioEncoding::PcmS16le, 16_000, 1),
            (AudioEncoding::Mulaw, 8_000, 1)
        ]
    );
    match speech.chunking {
        SpeechChunking::Stt(c) => assert_eq!(
            (c.frame_ms_min, c.frame_ms_max, c.max_utterance_ms),
            (20, 320, 30_000)
        ),
        other @ SpeechChunking::Tts(_) => panic!("STT chunking expected, got {other:?}"),
    }
}

#[test]
fn tts_fixture_parses_into_its_speech_contract() {
    let descriptor = parse_bundle(&fixture_dir(TTS)).expect("TTS fixture parses");
    let profile = descriptor.manifest.profile.expect("format 0.2 profile");
    assert_eq!(profile.runner_profile.as_deref(), Some("kokoro"));
    assert_eq!(profile.compute_type, Some(ComputeType::Float32));
    let warmup = profile.warmup.expect("warmup");
    assert_eq!(warmup.fixtures, ["warmup/segment.txt"]);
    assert_eq!((warmup.repetitions, warmup.timeout_ms), (3, 30_000));

    let speech = profile.speech.expect("speech contract");
    assert_eq!(speech.task, SpeechTask::Tts);
    assert_eq!(speech.wire_serving_mode(), Some("tts_streaming"));
    assert_eq!(speech.languages, ["en-US"]);
    assert_eq!(speech.voices, ["af_heart"]);
    assert!(speech.input_audio_formats.is_empty());
    let formats: Vec<_> = speech
        .output_audio_formats
        .iter()
        .map(|f| (f.encoding, f.sample_rate_hz, f.channels))
        .collect();
    assert_eq!(formats, [(AudioEncoding::PcmS16le, 24_000, 1)]);
    assert!(matches!(speech.chunking, SpeechChunking::Tts(_)));
}

#[test]
fn both_fixtures_budget_both_l4_domains_with_no_os_reserve() {
    for name in [STT, TTS] {
        let descriptor = parse_bundle(&fixture_dir(name)).expect("fixture parses");
        let profile = descriptor.manifest.profile.expect("format 0.2 profile");
        let by_domain = profile.memory_budget_by_domain.expect("per-domain budgets");
        assert!(
            by_domain.get(BudgetDomainName::SharedPool).is_none(),
            "{name}"
        );
        for domain in [BudgetDomainName::GuestRam, BudgetDomainName::DeviceVram] {
            let budget = by_domain.get(domain).expect("L4 domain");
            assert_eq!(
                budget.os_reserve_bytes, 0,
                "{name}: the row holds the OS reserve"
            );
            assert!(budget.per_session_state_bytes > 0, "{name}: session state");
        }
        let guest = by_domain
            .get(BudgetDomainName::GuestRam)
            .expect("guest_ram");
        assert!(guest.sidecar_process_bytes > 0, "{name}: sidecar residual");
        assert_eq!(
            by_domain.line_sum(),
            profile.memory_budget_breakdown_bytes,
            "{name}: the breakdown is the per-line domain sum"
        );
    }
}

#[test]
fn a_parsed_manifest_serializes_back_to_its_source() {
    for name in [STT, TTS] {
        let descriptor = parse_bundle(&fixture_dir(name)).expect("fixture parses");
        let written = serde_json::to_value(&descriptor.manifest).expect("serialize");
        assert_eq!(written, manifest(name), "{name}");
    }
}

#[test]
fn duplicate_keys_and_inexact_numbers_reject_in_the_manifest_text() {
    let text = std::fs::read_to_string(fixture_dir(STT).join("manifest.json")).expect("read");
    let duplicated = text.replacen(
        "\"runner_profile\": \"faster_whisper\",",
        "\"runner_profile\": \"faster_whisper\",\n  \"runner_profile\": \"kokoro\",",
        1,
    );
    assert_ne!(duplicated, text);
    parse_text(&fixture_dir(STT), &duplicated).expect_err("a repeated top-level key rejects");

    let inexact = text.replacen(
        "\"max_concurrent_sessions\": 1,",
        "\"max_concurrent_sessions\": 1.0000000000000001,",
        1,
    );
    assert_ne!(inexact, text);
    let changed: Value = serde_json::from_str(&inexact).expect("valid JSON");
    assert!(
        validator().is_valid(&changed),
        "an f64 validator sees 1.0; only the lexeme shows the fraction"
    );
    parse_text(&fixture_dir(STT), &inexact).expect_err("an inexact lexeme rejects");
}

fn format_0_1_bundles() -> Vec<PathBuf> {
    let root = repo_path("test/models/bundles/v0_1");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .expect("v0_1 fixtures")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| {
            p.is_dir()
                && !p
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("invalid_"))
        })
        .collect();
    dirs.sort();
    dirs
}

#[test]
fn format_0_1_fixtures_parse_without_a_profile_and_still_validate() {
    let dirs = format_0_1_bundles();
    assert!(!dirs.is_empty());
    for dir in dirs {
        let descriptor = parse_bundle(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        assert!(descriptor.manifest.profile.is_none(), "{}", dir.display());
        let manifest = read_json(&dir.join("manifest.json"));
        assert!(validator().is_valid(&manifest), "{}", dir.display());
    }
}

#[test]
fn format_0_1_keeps_format_0_2_keys_as_extras() {
    let dir = repo_path("test/models/bundles/v0_1/x86_fixture_smoke");
    let mut manifest = read_json(&dir.join("manifest.json"));
    apply(
        &mut manifest,
        &json!({"set": {
            "/runner_profile": 5,
            "/memory_budget_breakdown_bytes": {"not_a_line": true}
        }}),
    );
    let text = serde_json::to_string_pretty(&manifest).expect("serialize");
    let descriptor = parse_text(&dir, &text).expect("format 0.1 ignores format 0.2 keys");
    assert!(descriptor.manifest.profile.is_none());
    assert_eq!(
        descriptor.manifest.extra.get("runner_profile"),
        Some(&json!(5))
    );
    assert!(descriptor
        .manifest
        .extra
        .contains_key("memory_budget_breakdown_bytes"));
}

/// `{"$serde_json::private::RawValue": "<json>"}` is how serde_json's
/// `raw_value` feature spells an embedded document; with that feature off,
/// it is an ordinary key and must not smuggle a document past the checks.
fn wrapped(text: &str) -> String {
    json!({ "$serde_json::private::RawValue": text }).to_string()
}

#[test]
fn wrapped_documents_do_not_bypass_the_text_checks() {
    let text = std::fs::read_to_string(fixture_dir(STT).join("manifest.json")).expect("read");
    parse_text(&fixture_dir(STT), &wrapped(&text)).expect_err("a wrapped manifest rejects");

    let mut changed = manifest(STT);
    let blocks = changed["model_blocks"].to_string();
    changed["model_blocks"] = serde_json::from_str(&wrapped(&blocks)).expect("wrapper parses");
    let text = serde_json::to_string_pretty(&changed).expect("serialize");
    parse_text(&fixture_dir(STT), &text).expect_err("wrapped model blocks reject");

    let declaration = r#"{"schema_version":"0.1","memory_budget_breakdown_bytes":{"model_weights_bytes":1.0000000000000001}}"#;
    tensorplate_protocol::MemoryBudgetDeclaration::from_json(&wrapped(declaration))
        .expect_err("a wrapped budget declaration rejects");
}

#[test]
fn the_generic_payload_decoder_refuses_format_0_2() {
    let text = std::fs::read_to_string(fixture_dir(STT).join("manifest.json")).expect("read");
    let err =
        tensorplate_protocol::decode_with_version_check::<tensorplate_protocol::BundleManifest>(
            &text,
        )
        .expect_err("format 0.2 decodes only through the bundle parser");
    assert!(err.to_string().contains("bundle parser"), "{err}");

    let legacy = std::fs::read_to_string(repo_path(
        "test/models/bundles/v0_1/x86_fixture_smoke/manifest.json",
    ))
    .expect("read");
    let manifest = tensorplate_protocol::decode_with_version_check::<
        tensorplate_protocol::BundleManifest,
    >(&legacy)
    .expect("format 0.1 still decodes generically");
    assert!(manifest.profile.is_none());
}

#[test]
fn format_0_2_field_errors_name_the_field() {
    let mut changed = manifest(STT);
    changed["model_blocks"]["speech"]["task"] = json!(5);
    let text = serde_json::to_string_pretty(&changed).expect("serialize");
    let err = parse_text(&fixture_dir(STT), &text).expect_err("numeric task rejects");
    assert!(matches!(err, ParseError::ManifestSemantics(_)), "{err:?}");
    assert!(err.to_string().contains("model_blocks.speech"), "{err}");
}

#[test]
fn a_sequence_shaped_format_0_1_speech_block_rejects() {
    let dir = repo_path("test/models/bundles/v0_1/x86_fixture_smoke");
    let mut manifest = read_json(&dir.join("manifest.json"));
    manifest["model_blocks"] = json!({"speech": ["asr", 16_000, "log_mel"]});
    assert!(
        !validator().is_valid(&manifest),
        "the schema never allowed it"
    );
    let text = serde_json::to_string_pretty(&manifest).expect("serialize");
    parse_text(&dir, &text).expect_err("the parser now agrees with the schema");
}
