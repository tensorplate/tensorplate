// SPDX-License-Identifier: Apache-2.0
//
// Per-class memory budget fixtures. The committed JSON files under
// `tests/fixtures/` are the canonical demonstration of how each model class
// maps onto the shared `memory_budget_breakdown_bytes` vocabulary; this
// suite proves each fixture decodes fail-closed, round-trips losslessly,
// and exercises the class-specific lines it is meant to exercise.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use tensorplate_protocol::{
    decode_with_version_check, MemoryBudgetDeclaration, MEMORY_BUDGET_LINE_NAMES,
};

fn fixtures_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests");
    p.push("fixtures");
    p
}

fn load(name: &str) -> String {
    let mut p = fixtures_dir();
    p.push(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read fixture {}: {e}", p.display()))
}

fn decode_fixture(name: &str) -> MemoryBudgetDeclaration {
    let raw = load(name);
    let first: MemoryBudgetDeclaration =
        decode_with_version_check(&raw).unwrap_or_else(|e| panic!("decode failed for {name}: {e}"));
    let re_emitted = serde_json::to_string(&first).expect("serialize");
    let second: MemoryBudgetDeclaration =
        decode_with_version_check(&re_emitted).expect("re-decode");
    assert_eq!(first, second, "round-trip mismatch for {name}");
    first
}

#[test]
fn vla_fixture_exercises_step_scratch_action_queue_and_session_state() {
    let b = decode_fixture("memory_budget_vla.json").memory_budget_breakdown_bytes;
    assert!(
        b.cache_bytes > 0,
        "VLA declares KV/token + action-decoder cache"
    );
    assert!(
        b.step_scratch_bytes > 0,
        "VLA declares flow/denoising step scratch"
    );
    assert!(b.output_queue_bytes > 0, "VLA declares an action queue");
    assert!(b.io_buffer_bytes > 0, "VLA declares observation buffers");
    assert!(
        b.per_session_state_bytes > 0,
        "VLA declares policy-session state"
    );
}

#[test]
fn speech_stt_fixture_declares_streaming_session_state() {
    let b = decode_fixture("memory_budget_speech_stt.json").memory_budget_breakdown_bytes;
    assert!(
        b.per_session_state_bytes > 0,
        "streaming STT must declare non-zero per-session state"
    );
    assert!(
        b.cache_bytes > 0,
        "STT declares shared acoustic/decoder caches"
    );
    assert!(
        b.output_queue_bytes > 0,
        "STT declares undelivered transcript events"
    );
    assert!(b.io_buffer_bytes > 0, "STT declares the audio chunk ring");
    assert_eq!(b.step_scratch_bytes, 0, "STT maps no step scratch");
}

#[test]
fn speech_tts_fixture_declares_streaming_session_state() {
    let b = decode_fixture("memory_budget_speech_tts.json").memory_budget_breakdown_bytes;
    assert!(
        b.per_session_state_bytes > 0,
        "streaming TTS must declare non-zero per-session state"
    );
    assert!(b.cache_bytes > 0, "TTS declares shared vocoder state");
    assert!(
        b.output_queue_bytes > 0,
        "TTS declares undelivered audio chunks"
    );
    assert!(
        b.io_buffer_bytes > 0,
        "TTS declares text-in/audio-out buffers"
    );
    assert_eq!(b.step_scratch_bytes, 0, "TTS maps no step scratch");
}

#[test]
fn vision_fixture_is_request_scoped() {
    let b = decode_fixture("memory_budget_vision.json").memory_budget_breakdown_bytes;
    assert_eq!(
        b.per_session_state_bytes, 0,
        "vision is request-scoped: no per-session state"
    );
    assert_eq!(b.cache_bytes, 0, "vision maps no reusable cache");
    assert_eq!(b.step_scratch_bytes, 0, "vision maps no step scratch");
    assert!(b.output_queue_bytes > 0, "vision declares output buffers");
    assert!(b.io_buffer_bytes > 0, "vision declares frame buffers");
    assert!(
        b.sidecar_process_bytes > 0,
        "the vision fixture models the sidecar backend path"
    );
}

#[test]
fn language_readiness_fixture_declares_engine_kv_pool() {
    let b = decode_fixture("memory_budget_language_readiness.json").memory_budget_breakdown_bytes;
    assert!(
        b.cache_bytes > 0,
        "language readiness declares the engine KV pool as cache"
    );
    assert!(
        b.backend_reserve_bytes > 0,
        "language declares engine-internal reserve"
    );
    assert!(
        b.output_queue_bytes > 0,
        "language declares undelivered token events"
    );
    assert!(b.io_buffer_bytes > 0, "language declares prompt buffers");
    assert!(
        b.per_session_state_bytes > 0,
        "language declares per-request sequence state"
    );
    assert_eq!(b.step_scratch_bytes, 0, "language maps no step scratch");
}

#[test]
fn all_class_fixtures_declare_universal_lines() {
    // Walk the directory rather than hardcoding names so a future class
    // fixture cannot silently skip the universal-lines check.
    let mut checked = 0;
    for entry in std::fs::read_dir(fixtures_dir()).expect("fixtures dir") {
        let name = entry
            .expect("dir entry")
            .file_name()
            .into_string()
            .expect("utf-8 fixture name");
        let is_json = std::path::Path::new(&name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
        if !name.starts_with("memory_budget_") || !is_json {
            continue;
        }
        let b = decode_fixture(&name).memory_budget_breakdown_bytes;
        assert!(b.model_weights_bytes > 0, "{name}: weights line");
        assert!(
            b.runtime_overhead_bytes > 0,
            "{name}: runtime overhead line"
        );
        assert!(b.session_scratch_bytes > 0, "{name}: session scratch line");
        assert!(b.os_reserve_bytes > 0, "{name}: OS reserve line");
        assert!(b.backend_reserve_bytes > 0, "{name}: backend reserve line");
        checked += 1;
    }
    assert_eq!(checked, 5, "expected exactly five per-class fixtures");
}

#[test]
fn rust_vocabulary_matches_schema_document() {
    // The schema document is the language-neutral source of truth; this
    // test keeps the Rust mirror in lockstep with it.
    let schema_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../config/schemas/memory_budget_breakdown.json");
    let raw = std::fs::read_to_string(&schema_path)
        .unwrap_or_else(|e| panic!("read schema {}: {e}", schema_path.display()));
    let schema: serde_json::Value = serde_json::from_str(&raw).expect("schema document parses");
    let breakdown = &schema["definitions"]["memory_budget_breakdown_bytes"];

    let mut schema_lines: Vec<&str> = breakdown["properties"]
        .as_object()
        .expect("breakdown properties object")
        .keys()
        .map(String::as_str)
        .collect();
    schema_lines.sort_unstable();
    let mut expected = MEMORY_BUDGET_LINE_NAMES.to_vec();
    expected.sort_unstable();
    assert_eq!(
        schema_lines, expected,
        "schema line names must match the Rust vocabulary"
    );

    let required: Vec<&str> = breakdown["required"]
        .as_array()
        .expect("breakdown required array")
        .iter()
        .map(|v| v.as_str().expect("required entry is a string"))
        .collect();
    assert_eq!(required, ["model_weights_bytes"]);
    assert_eq!(
        breakdown["additionalProperties"],
        serde_json::Value::Bool(false),
        "unknown line names must stay fail-closed in the schema"
    );
}
