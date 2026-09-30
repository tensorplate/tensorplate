// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use serde_json::{json, Value};
use tensorplate_protocol::{decode_with_version_check, MemoryObservation, SCHEMA_VERSION};

const FIXTURE: &str = include_str!("fixtures/memory_observation_l4_idle.json");

fn schema() -> jsonschema::JSONSchema {
    jsonschema::JSONSchema::compile(
        &serde_json::from_str::<Value>(include_str!("../../schemas/memory_observation.json"))
            .unwrap(),
    )
    .unwrap()
}

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).unwrap()
}

#[test]
fn memory_observation_round_trip_and_schema() {
    let first: MemoryObservation = decode_with_version_check(FIXTURE).unwrap();
    assert_eq!(first.schema_version, SCHEMA_VERSION);
    let value = serde_json::to_value(&first).unwrap();
    assert_eq!(value, fixture());
    assert!(schema().is_valid(&value));
    assert_eq!(
        serde_json::from_value::<MemoryObservation>(value).unwrap(),
        first
    );
}

#[test]
fn schema_and_reader_reject_malformed_fields_independently() {
    let validator = schema();
    let replacements = [
        ("/domain", json!("gpu")),
        ("/source", json!("torch")),
        ("/source", json!({"nvidia_smi_xml":null})),
        ("/availability", json!({"available":null})),
        ("/sampled_monotonic_ns", json!(-1)),
        ("/sampled_monotonic_ns", json!(9_007_199_254_740_992_u64)),
        ("/process_aggregate/processes/0/pid", json!(0)),
        (
            "/process_aggregate/processes/0/bytes/bytes",
            json!(9_007_199_254_740_992_u64),
        ),
        (
            "/process_aggregate/processes/0/role",
            json!({"python_sidecar":null}),
        ),
        (
            "/allocated",
            json!({"availability":"unavailable","reason":{"unsupported":null}}),
        ),
        ("/reserved", json!({"availability":"available"})),
        (
            "/device_aggregate/used",
            json!({"availability":"available","bytes":-1}),
        ),
        (
            "/device_aggregate/used",
            json!({"availability":"available","bytes":1.5}),
        ),
        (
            "/device_aggregate/used",
            json!({"availability":"available","bytes":9_007_199_254_740_992_u64}),
        ),
        (
            "/device_aggregate/used",
            json!({"availability":"unavailable","bytes":0,"reason":"missing_field"}),
        ),
        (
            "/process_aggregate",
            json!({"availability":"available","processes":[[5819,"external",{"availability":"available","bytes":1}]]}),
        ),
        ("/device_aggregate", json!([])),
        ("/schema_version", json!("0.2")),
    ];
    for (path, value) in replacements {
        let mut candidate = fixture();
        *candidate.pointer_mut(path).unwrap() = value;
        assert!(
            !validator.is_valid(&candidate),
            "schema {path}: {candidate}"
        );
        assert!(
            serde_json::from_value::<MemoryObservation>(candidate.clone()).is_err(),
            "reader {path}: {candidate}"
        );
    }
    for key in fixture().as_object().unwrap().keys() {
        let mut missing = fixture();
        missing.as_object_mut().unwrap().remove(key);
        assert!(!validator.is_valid(&missing), "missing {key}");
        assert!(
            serde_json::from_value::<MemoryObservation>(missing).is_err(),
            "missing {key}"
        );
        let mut null = fixture();
        null[key] = Value::Null;
        assert!(!validator.is_valid(&null), "null {key}");
        assert!(
            serde_json::from_value::<MemoryObservation>(null).is_err(),
            "null {key}"
        );
    }
    let mut unknown = fixture();
    unknown["extra"] = json!(true);
    assert!(!validator.is_valid(&unknown));
    assert!(serde_json::from_value::<MemoryObservation>(unknown).is_err());
    let mut unavailable_processes = fixture();
    unavailable_processes["availability"] = json!("partial");
    unavailable_processes["process_aggregate"] =
        json!({"availability":"unavailable","reason":"unsupported"});
    assert!(validator.is_valid(&unavailable_processes));
    assert!(serde_json::from_value::<MemoryObservation>(unavailable_processes.clone()).is_ok());
    unavailable_processes["process_aggregate"]["reason"] = json!({"unsupported":null});
    assert!(!validator.is_valid(&unavailable_processes));
    assert!(serde_json::from_value::<MemoryObservation>(unavailable_processes).is_err());
}

#[test]
fn semantic_guards_reject_available_lies_and_misclassified_measurements() {
    for (path, value) in [
        ("/domain", json!("guest_ram")),
        ("/availability", json!("unavailable")),
        ("/device_aggregate/capacity/bytes", json!(0)),
        (
            "/device_aggregate/available/bytes",
            json!(24_152_899_585_u64),
        ),
        ("/device_aggregate/used/bytes", json!(24_152_899_585_u64)),
        (
            "/device_aggregate/driver_reserved/bytes",
            json!(24_152_899_585_u64),
        ),
        ("/allocated", json!({"availability":"available","bytes":1})),
        ("/reserved", json!({"availability":"available","bytes":1})),
    ] {
        let mut candidate = fixture();
        *candidate.pointer_mut(path).unwrap() = value;
        assert!(
            serde_json::from_value::<MemoryObservation>(candidate).is_err(),
            "{path}"
        );
    }
    let mut zero_capacity = fixture();
    for reading in zero_capacity["device_aggregate"]
        .as_object_mut()
        .unwrap()
        .values_mut()
    {
        reading["bytes"] = json!(0);
    }
    zero_capacity["process_aggregate"]["processes"] = json!([]);
    assert!(serde_json::from_value::<MemoryObservation>(zero_capacity).is_err());
    let mut duplicated = fixture();
    let item = duplicated["process_aggregate"]["processes"][0].clone();
    duplicated["process_aggregate"]["processes"]
        .as_array_mut()
        .unwrap()
        .push(item);
    assert!(serde_json::from_value::<MemoryObservation>(duplicated).is_err());
    let mut partial = fixture();
    partial["device_aggregate"]["available"] =
        json!({"availability":"unavailable","reason":"missing_field"});
    assert!(serde_json::from_value::<MemoryObservation>(partial.clone()).is_err());
    partial["availability"] = json!("partial");
    assert!(schema().is_valid(&partial));
    assert!(serde_json::from_value::<MemoryObservation>(partial).is_ok());
    let mut unavailable = fixture();
    for value in unavailable["device_aggregate"]
        .as_object_mut()
        .unwrap()
        .values_mut()
    {
        *value = json!({"availability":"unavailable","reason":"source_unavailable"});
    }
    unavailable["process_aggregate"] =
        json!({"availability":"unavailable","reason":"source_unavailable"});
    unavailable["availability"] = json!("unavailable");
    assert!(schema().is_valid(&unavailable));
    assert!(serde_json::from_value::<MemoryObservation>(unavailable).is_ok());
}
