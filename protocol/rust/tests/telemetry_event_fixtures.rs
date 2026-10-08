// SPDX-License-Identifier: Apache-2.0
//
// The log and metric events, their delivery class and the registered metric
// label values, against the schema files and the names the values repeat.

#![allow(clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use tensorplate_protocol::bundle::parse_bundle;
use tensorplate_protocol::metric_event::{
    registered_metric_label_values, ALLOWED_METRIC_LABEL_KEYS, METRIC_MODE_VALUES,
    METRIC_OUTCOME_VALUES, METRIC_ROW_VALUES, METRIC_STAGE_VALUES,
};
use tensorplate_protocol::{
    decode_with_version_check, LogComponent, LogEvent, LogLevel, MetricEvent, MetricKind,
    MetricLabels, MetricSample, MetricUnit, SpeechServingMode, SpeechTask, TelemetryPriority,
    ValidatePayload, RUNTIME_PIPELINE_STAGES,
};

const REGISTERED_KEYS: [&str; 4] = ["row", "mode", "outcome", "stage"];

fn repo_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

fn read(relative: &str) -> String {
    let path = repo_path(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn schema(name: &str) -> Value {
    serde_json::from_str(&read(&format!("protocol/schemas/{name}"))).expect("schema parses")
}

fn validator(name: &str) -> jsonschema::JSONSchema {
    jsonschema::JSONSchema::compile(&schema(name)).expect("schema compiles")
}

/// The documents of a fixture: one per line for `.jsonl`, else the file.
fn documents(fixture: &str) -> Vec<Value> {
    let raw = read(&format!("protocol/rust/tests/fixtures/{fixture}"));
    if Path::new(fixture)
        .extension()
        .is_some_and(|ext| ext == "jsonl")
    {
        raw.lines()
            .map(|line| serde_json::from_str(line).expect("fixture line parses"))
            .collect()
    } else {
        vec![serde_json::from_str(&raw).expect("fixture parses")]
    }
}

fn names(values: &[&str]) -> Vec<Value> {
    values.iter().map(|v| json!(v)).collect()
}

fn priority_names() -> Vec<Value> {
    TelemetryPriority::ALL
        .iter()
        .map(|p| json!(p.as_str()))
        .collect()
}

/// Decodes every document through the mirror and requires the re-emitted
/// event to equal the fixture, so a field the mirror drops fails here.
fn assert_lossless<T>(schema_name: &str, fixture: &str)
where
    T: serde::de::DeserializeOwned + serde::Serialize + ValidatePayload,
{
    let validator = validator(schema_name);
    let documents = documents(fixture);
    assert!(!documents.is_empty(), "{fixture} is empty");
    for document in documents {
        assert!(
            validator.is_valid(&document),
            "{fixture}: violates {schema_name}: {document}"
        );
        let event: T = decode_with_version_check(&document.to_string())
            .unwrap_or_else(|e| panic!("{fixture}: decode failed: {e}\n{document}"));
        assert_eq!(
            serde_json::to_value(&event).expect("serialize"),
            document,
            "{fixture}: the mirror is not lossless"
        );
    }
}

#[test]
fn log_event_fixtures_satisfy_the_schema_and_round_trip() {
    assert_lossless::<LogEvent>("log_event.json", "log_event_0_1_unclassified.json");
    assert_lossless::<LogEvent>("log_event.json", "log_event_priority.jsonl");
}

#[test]
fn metric_event_fixtures_satisfy_the_schema_and_round_trip() {
    assert_lossless::<MetricEvent>("metric_event.json", "metric_event_0_1_unclassified.json");
    assert_lossless::<MetricEvent>("metric_event.json", "metric_event_priority.jsonl");
    assert_lossless::<MetricEvent>("metric_event.json", "metric_event_speech_labels.jsonl");
}

#[test]
fn priority_enum_matches_both_schemas_and_every_class_has_a_fixture() {
    for (schema_name, fixture) in [
        ("log_event.json", "log_event_priority.jsonl"),
        ("metric_event.json", "metric_event_priority.jsonl"),
    ] {
        let listed = schema(schema_name)["properties"]["priority"]["enum"].clone();
        assert_eq!(listed, Value::Array(priority_names()), "{schema_name}");
        let recorded: Vec<Value> = documents(fixture)
            .iter()
            .map(|d| d["priority"].clone())
            .collect();
        assert_eq!(recorded, priority_names(), "{fixture}");
    }
}

#[test]
fn the_builders_emit_the_priority_fixtures() {
    let levels = [
        LogLevel::Error,
        LogLevel::Warn,
        LogLevel::Info,
        LogLevel::Debug,
    ];
    let recorded = documents("log_event_priority.jsonl");
    for (index, (priority, level)) in TelemetryPriority::ALL.into_iter().zip(levels).enumerate() {
        let name = recorded[index]["event"].as_str().expect("event name");
        let event = LogEvent::new(
            LogComponent::ServingWorker,
            name,
            level,
            1000 + index as u64,
        )
        .with_priority(priority)
        .with_deployment("synthetic-deployment");
        assert_eq!(
            serde_json::to_value(&event).expect("serialize"),
            recorded[index]
        );
    }

    let mut labels = MetricLabels::new();
    assert!(labels.insert("backend", "python_pytorch"));
    let mut event = MetricEvent::new(
        "tp_scheduler_missed_deadlines",
        MetricKind::Counter,
        MetricUnit::Count,
        1001,
        MetricSample::scalar(3.0),
    )
    .with_priority(TelemetryPriority::Safety);
    event.labels = labels;
    assert_eq!(
        serde_json::to_value(&event).expect("serialize"),
        documents("metric_event_priority.jsonl")[1]
    );
}

#[test]
fn an_event_without_a_priority_stays_unclassified() {
    let log: LogEvent =
        decode_with_version_check(&documents("log_event_0_1_unclassified.json")[0].to_string())
            .expect("log event decodes");
    assert_eq!(log.priority, None);
    let metric: MetricEvent =
        decode_with_version_check(&documents("metric_event_0_1_unclassified.json")[0].to_string())
            .expect("metric event decodes");
    assert_eq!(metric.priority, None);
}

#[test]
fn a_null_or_unknown_priority_is_rejected_by_mirror_and_schema() {
    for bad in [Value::Null, json!("urgent"), json!("Fatal"), json!(1)] {
        let mut log = documents("log_event_priority.jsonl").remove(0);
        log["priority"] = bad.clone();
        assert!(!validator("log_event.json").is_valid(&log), "{bad}");
        assert!(
            decode_with_version_check::<LogEvent>(&log.to_string()).is_err(),
            "{bad}"
        );

        let mut metric = documents("metric_event_priority.jsonl").remove(0);
        metric["priority"] = bad.clone();
        assert!(!validator("metric_event.json").is_valid(&metric), "{bad}");
        assert!(
            decode_with_version_check::<MetricEvent>(&metric.to_string()).is_err(),
            "{bad}"
        );
    }
}

// A reader built before a field existed accepts an event that carries it
// only because the mirrors do not deny unknown fields.
#[test]
fn the_mirrors_ignore_a_field_they_do_not_know() {
    let mut log = documents("log_event_priority.jsonl").remove(0);
    log["added_later"] = json!("value");
    decode_with_version_check::<LogEvent>(&log.to_string()).expect("log event decodes");

    let mut metric = documents("metric_event_priority.jsonl").remove(0);
    metric["added_later"] = json!("value");
    decode_with_version_check::<MetricEvent>(&metric.to_string()).expect("metric event decodes");
}

#[test]
fn registered_label_values_match_both_schemas() {
    let lists = [
        ("row", &METRIC_ROW_VALUES[..]),
        ("mode", &METRIC_MODE_VALUES[..]),
        ("outcome", &METRIC_OUTCOME_VALUES[..]),
        ("stage", &METRIC_STAGE_VALUES[..]),
    ];
    assert_eq!(lists.map(|(key, _)| key), REGISTERED_KEYS);
    let event = schema("metric_event.json");
    let worker = schema("serving_metrics.json");
    let constrained: BTreeSet<&str> = event["properties"]["labels"]["properties"]
        .as_object()
        .expect("label properties")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(constrained, BTreeSet::from(REGISTERED_KEYS));
    for (key, values) in lists {
        assert_eq!(registered_metric_label_values(key), Some(values), "{key}");
        let pointer = match key {
            "outcome" => "/definitions/OutcomeLabel/enum".to_owned(),
            "stage" => "/definitions/StageLabel/enum".to_owned(),
            _ => format!("/properties/labels/properties/{key}/enum"),
        };
        for (name, listed) in [
            (
                "metric_event.json",
                event.pointer(&format!("/properties/labels/properties/{key}/enum")),
            ),
            ("serving_metrics.json", worker.pointer(&pointer)),
        ] {
            assert_eq!(listed, Some(&Value::Array(names(values))), "{name}: {key}");
        }
    }
    for key in ALLOWED_METRIC_LABEL_KEYS {
        assert_eq!(
            registered_metric_label_values(key).is_some(),
            REGISTERED_KEYS.contains(key),
            "{key}"
        );
    }
    assert_eq!(registered_metric_label_values("session"), None);
}

#[test]
fn the_speech_label_fixture_carries_every_registered_value() {
    let documents = documents("metric_event_speech_labels.jsonl");
    for key in REGISTERED_KEYS {
        let recorded: BTreeSet<&str> = documents
            .iter()
            .filter_map(|d| d["labels"][key].as_str())
            .collect();
        let registered: BTreeSet<&str> = registered_metric_label_values(key)
            .expect("registered key")
            .iter()
            .copied()
            .collect();
        assert_eq!(recorded, registered, "{key}");
    }
}

#[test]
fn an_unregistered_label_value_is_rejected_by_mirror_and_schema() {
    let validator = validator("metric_event.json");
    for key in REGISTERED_KEYS {
        for bad in ["session-7", "", "Backend"] {
            let mut event = documents("metric_event_0_1_unclassified.json").remove(0);
            event["labels"][key] = json!(bad);
            assert!(!validator.is_valid(&event), "{key}={bad}");
            assert!(
                decode_with_version_check::<MetricEvent>(&event.to_string()).is_err(),
                "{key}={bad}"
            );
        }
    }
}

#[test]
fn serving_metrics_labels_take_row_and_mode_and_no_per_series_key() {
    let validator = validator("serving_metrics.json");
    let body = |labels: Value| {
        json!({
            "schema_version": "0.1",
            "labels": labels,
            "counters": {},
            "gauges": {},
            "latency_ms": {}
        })
    };
    let base = json!({
        "endpoint": "synthetic-deployment",
        "model_class": "speech",
        "model_name": "synthetic-model",
        "backend": "python_pytorch"
    });
    assert!(validator.is_valid(&body(base.clone())));
    for key in ["row", "mode"] {
        for value in registered_metric_label_values(key).expect("registered key") {
            let mut labels = base.clone();
            labels[key] = json!(value);
            assert!(validator.is_valid(&body(labels)), "{key}={value}");
        }
        let mut labels = base.clone();
        labels[key] = json!("session-7");
        assert!(!validator.is_valid(&body(labels)), "{key}");
    }
    for (key, value) in [
        ("outcome", "failed"),
        ("stage", "backend"),
        ("session_id", "s"),
    ] {
        let mut labels = base.clone();
        labels[key] = json!(value);
        assert!(!validator.is_valid(&body(labels)), "{key}");
    }
}

#[test]
fn serving_metrics_per_series_definitions_take_registered_values_only() {
    let schema = schema("serving_metrics.json");
    for (definition, key) in [("OutcomeLabel", "outcome"), ("StageLabel", "stage")] {
        let validator = jsonschema::JSONSchema::compile(&schema["definitions"][definition])
            .expect("definition compiles");
        for value in registered_metric_label_values(key).expect("registered key") {
            assert!(validator.is_valid(&json!(value)), "{definition}: {value}");
        }
        for bad in [json!("session-7"), json!(""), Value::Null] {
            assert!(!validator.is_valid(&bad), "{definition}: {bad}");
        }
    }
}

// A key is added on purpose, here and in the schema's description; a
// session, turn, request, utterance or voice identifier is never one.
#[test]
fn the_allowed_label_keys_are_exactly_these_ten() {
    let expected = [
        "endpoint",
        "model_class",
        "model_name",
        "backend",
        "component",
        "status",
        "row",
        "mode",
        "outcome",
        "stage",
    ];
    assert_eq!(ALLOWED_METRIC_LABEL_KEYS, expected);
    let description = schema("metric_event.json")["properties"]["labels"]["description"]
        .as_str()
        .expect("description")
        .to_owned();
    let listed = expected.map(|key| format!("`{key}`")).join(", ");
    assert!(
        description.contains(&format!("allows the keys {listed}.")),
        "{description}"
    );
}

#[test]
fn mode_values_are_what_the_speech_block_resolves_to() {
    let dir = repo_path("test/models/bundles/v0_2/speech_stt_streaming");
    let bundle = parse_bundle(&dir).expect("fixture parses");
    let profile = bundle.manifest.profile.expect("format 0.2 profile");
    let contract = profile.speech.expect("speech block");
    // Exhaustive on purpose: a new serving mode or task stops this compiling.
    let registered = |mode, task| match (mode, task) {
        (SpeechServingMode::Streaming, SpeechTask::Stt) => Some(METRIC_MODE_VALUES[0]),
        (SpeechServingMode::Streaming, SpeechTask::Tts) => Some(METRIC_MODE_VALUES[1]),
        (SpeechServingMode::Batch, SpeechTask::Stt | SpeechTask::Tts) => None,
    };
    let mut resolved = Vec::new();
    for mode in [SpeechServingMode::Streaming, SpeechServingMode::Batch] {
        for task in [SpeechTask::Stt, SpeechTask::Tts] {
            let mut contract = contract.clone();
            contract.serving_mode = mode;
            contract.task = task;
            assert_eq!(contract.wire_serving_mode(), registered(mode, task));
            resolved.extend(contract.wire_serving_mode());
        }
    }
    assert_eq!(resolved, METRIC_MODE_VALUES);
}

#[test]
fn mode_values_are_the_stream_schema_session_modes() {
    let proto = read("protocol/proto/tensorplate/stream/v1/session.proto");
    let declared: Vec<String> = proto
        .lines()
        .filter_map(|line| line.trim().strip_prefix("SESSION_MODE_"))
        .filter_map(|rest| rest.split_once(" = "))
        .map(|(name, _)| name.to_ascii_lowercase())
        .filter(|name| name != "unspecified")
        .collect();
    assert_eq!(declared, METRIC_MODE_VALUES);
}

#[test]
fn stage_values_are_the_pipeline_stages_with_the_queue_after_ingress() {
    let without_queue: Vec<&str> = METRIC_STAGE_VALUES
        .iter()
        .copied()
        .filter(|stage| *stage != "queue")
        .collect();
    assert_eq!(without_queue, RUNTIME_PIPELINE_STAGES);
    assert_eq!(METRIC_STAGE_VALUES[..2], ["ingress", "queue"]);
}
