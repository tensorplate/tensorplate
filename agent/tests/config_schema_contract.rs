// SPDX-License-Identifier: Apache-2.0
//
// The agent config schema and the running agent must accept the same set
// of admission postures.
//
// `config/schemas/agent.json` sets `additionalProperties: false`, so a
// field the runtime honours but the schema omits is refused by every
// schema-aware validator while the agent itself starts happily on it. The
// operator override then works in one place and not the other, and which
// one an operator trusts decides whether their machine deploys.
//
// Asserted in BOTH directions deliberately. Checking only that the schema's
// values parse would pass while the runtime accepted a third posture the
// schema rejects -- which is the exact shape of the bug this file exists
// to stop.

#![allow(clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use tensorplate_agent::config::AgentConfig;
use tensorplate_platform::AdmissionPosture;

fn repo_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(relative)
}

/// The postures `config/schemas/agent.json` allows.
fn schema_postures() -> Vec<String> {
    let body = std::fs::read_to_string(repo_path("config/schemas/agent.json"))
        .expect("read the agent config schema");
    let schema: serde_json::Value = serde_json::from_str(&body).expect("schema parses");
    schema["properties"]["admission_posture"]["enum"]
        .as_array()
        .expect("admission_posture declares an enum")
        .iter()
        .map(|value| value.as_str().expect("enum values are strings").to_string())
        .collect()
}

/// A config the runtime accepts, with `admission_posture` set to `posture`
/// when given. Mirrors the shipped `packaging/conf/agent.json` shape.
fn config_json(posture: Option<&str>) -> String {
    let line = posture.map_or(String::new(), |value| {
        format!("  \"admission_posture\": \"{value}\",\n")
    });
    format!(
        "{{\n\
         {line}\
           \"schema_version\": \"0.1\",\n\
           \"transport\": \"unix_socket\",\n\
           \"socket_path\": \"/run/tensorplate/agent.sock\",\n\
           \"state_dir\": \"/var/lib/tensorplate/state\",\n\
           \"staging_dir\": \"/var/lib/tensorplate/bundles/staging\",\n\
           \"available_backends\": [\"mock\"]\n\
         }}"
    )
}

#[test]
fn the_schema_declares_every_posture_the_runtime_accepts() {
    let mut declared = schema_postures();
    declared.sort();
    let mut runtime: Vec<String> = AdmissionPosture::ALL
        .iter()
        .map(|posture| posture.as_str().to_string())
        .collect();
    runtime.sort();
    assert_eq!(
        declared, runtime,
        "the agent config schema and AdmissionPosture disagree; a posture the \
         runtime accepts but the schema omits is refused by schema-aware \
         validators while the agent starts on it"
    );
}

#[test]
fn every_posture_the_schema_declares_starts_a_real_agent() {
    // Parsing the string is not the claim. The claim is that a config file
    // carrying it survives `AgentConfig::validate`, which is what an
    // operator actually does with the value.
    for posture in schema_postures() {
        let cfg = AgentConfig::parse_json(&config_json(Some(&posture)))
            .unwrap_or_else(|err| panic!("`{posture}` is declared but rejected at startup: {err}"));
        assert_eq!(cfg.admission_posture.as_deref(), Some(posture.as_str()));
    }
}

#[test]
fn a_posture_neither_side_knows_is_refused_at_startup() {
    // Fail closed. An unrecognised posture silently becoming the row floor
    // would run the machine at a strictness nobody chose.
    let err = AgentConfig::parse_json(&config_json(Some("whatever_is_convenient")))
        .expect_err("an unknown posture must not start the agent");
    let message = err.to_string();
    assert!(
        message.contains("whatever_is_convenient"),
        "the error should name the offending value, got `{message}`"
    );
}

#[test]
fn omitting_the_posture_stays_valid_and_leaves_the_row_to_decide() {
    // The field is optional; every config written before it existed must
    // still load, with no operator preference recorded.
    let cfg =
        AgentConfig::parse_json(&config_json(None)).expect("a config without a posture loads");
    assert_eq!(cfg.admission_posture, None);
}

// ---------------------------------------------------------------------------
// The whole config surface, not one field.
//
// `admission_posture` was added to the struct and not to the schema, and
// nothing in the build noticed. The same was already true of four control
// fields and `runtime_version`, and of a `control` object the schema
// declared that no config has ever carried. A per-field test would have
// caught none of those, because each was written after the field it would
// have guarded.

/// Every top-level key a fully-populated config serializes to.
fn runtime_keys() -> BTreeSet<String> {
    // Every optional field is Some on purpose: two of them are
    // `skip_serializing_if = "Option::is_none"`, so a None here would hide
    // exactly the field this is meant to check.
    let populated = serde_json::json!({
        "schema_version": "0.1",
        "transport": "unix_socket",
        "socket_path": "/run/tensorplate/agent.sock",
        "tcp_bind_host": "127.0.0.1",
        "tcp_bind_port": 18080_u16,
        "state_dir": "/var/lib/tensorplate/state",
        "staging_dir": "/var/lib/tensorplate/bundles/staging",
        "available_backends": ["mock"],
        "device_memory_bytes": 8_589_934_592_u64,
        "admission_posture": "validated_row_required",
        "runtime_version": "0.2.1",
        "supervision": {
            "binary_path": "/usr/lib/tensorplate/tensorplate-serving",
            "working_dir": "/var/lib/tensorplate",
            "serving_config_path": "/var/lib/tensorplate/serving.json",
            "control_port": 18080
        },
    });
    let config =
        AgentConfig::parse_json(&populated.to_string()).expect("the populated config is valid");
    let rendered = serde_json::to_value(config).expect("a config serializes");
    rendered
        .as_object()
        .expect("a config is an object")
        .keys()
        .cloned()
        .collect()
}

fn schema_properties() -> BTreeSet<String> {
    let body = std::fs::read_to_string(repo_path("config/schemas/agent.json"))
        .expect("read the agent config schema");
    let schema: serde_json::Value = serde_json::from_str(&body).expect("schema parses");
    schema["properties"]
        .as_object()
        .expect("the schema declares properties")
        .keys()
        .cloned()
        .collect()
}

#[test]
fn the_schema_declares_every_field_the_runtime_accepts() {
    // `additionalProperties: false` means a field the schema omits is
    // REFUSED by a schema-aware validator while the agent starts on it.
    let (runtime, declared) = (runtime_keys(), schema_properties());
    let missing: Vec<&String> = runtime.difference(&declared).collect();
    assert!(
        missing.is_empty(),
        "the runtime accepts fields the schema omits, so a config using them is refused by \
         every validator while the agent honours it: {missing:?}"
    );
}

#[test]
fn the_schema_declares_no_field_the_runtime_would_refuse() {
    // The other direction, which is not symmetric: a schema-only field is
    // an invitation to write a config the agent silently ignores. The
    // `control` object was exactly that -- an object no config has ever
    // carried, describing a shape the runtime never had.
    let (runtime, declared) = (runtime_keys(), schema_properties());
    let invented: Vec<&String> = declared.difference(&runtime).collect();
    assert!(
        invented.is_empty(),
        "the schema declares fields the runtime does not accept, so a config written to the \
         schema would be ignored or rejected: {invented:?}"
    );
}

/// The compiled schema, so a test validates documents rather than
/// inspecting key names.
fn compiled_schema() -> jsonschema::JSONSchema {
    let body = std::fs::read_to_string(repo_path("config/schemas/agent.json"))
        .expect("read the agent config schema");
    let document: serde_json::Value = serde_json::from_str(&body).expect("schema parses");
    // Leaked so the compiled schema can borrow it for the test's lifetime.
    let document: &'static serde_json::Value = Box::leak(Box::new(document));
    jsonschema::JSONSchema::compile(document).expect("the schema itself is valid")
}

#[test]
fn every_shipped_config_is_accepted_by_the_schema_it_names() {
    // The bug that started this: `packaging/conf/agent.json` -- the config
    // installed on every Debian host -- did not validate against the schema
    // `docs/architecture/agent.md` names as its wire format.
    //
    // Validated with the real compiler, not by comparing top-level key
    // names: a nested property, a wrong type, a bad enum value or an
    // out-of-range number all pass a membership check while failing the
    // schema this is named after.
    let schema = compiled_schema();
    for relative in [
        "packaging/conf/agent.json",
        "packaging/homebrew/conf/agent.json.in",
    ] {
        let body = std::fs::read_to_string(repo_path(relative))
            .unwrap_or_else(|err| panic!("read {relative}: {err}"));
        // The Homebrew file is a template. Its @HOMEBREW_PREFIX@ tokens sit
        // inside string values, so it parses as JSON untouched -- but both
        // the schema and the runtime require absolute paths, and an
        // unsubstituted token is not one.
        let body = body.replace("@HOMEBREW_PREFIX@", "/opt/homebrew");
        let document: serde_json::Value =
            serde_json::from_str(&body).unwrap_or_else(|err| panic!("{relative} parses: {err}"));

        if let Err(errors) = schema.validate(&document) {
            let rendered: Vec<String> = errors
                .map(|e| format!("{} at {}", e, e.instance_path))
                .collect();
            panic!(
                "`{relative}` does not satisfy its own schema:\n  {}",
                rendered.join("\n  ")
            );
        }

        AgentConfig::parse_json(&body)
            .unwrap_or_else(|err| panic!("{relative} must also satisfy the runtime: {err}"));
    }
}

#[test]
fn a_config_the_schema_accepts_is_one_the_runtime_can_start() {
    // The two must agree on what is REQUIRED, not only on which fields
    // exist. `{"schema_version":"0.1"}` used to validate cleanly and then
    // fail at startup for want of `state_dir` -- an operator following the
    // published schema produced a config the agent refused.
    let schema = compiled_schema();
    let cases = [
        r#"{"schema_version":"0.1"}"#,
        // Isolates the REQUIRED fields specifically: socket_path is present,
        // so the transport conditional is satisfied and nothing else can do
        // the rejecting. Without this case, dropping `required` from the
        // schema left every other case still rejected for another reason and
        // this test passed while the guarantee was gone.
        r#"{"schema_version":"0.1","socket_path":"/run/tensorplate/agent.sock"}"#,
        r#"{"schema_version":"0.1","state_dir":"/var/lib/tp","staging_dir":"/var/lib/tp/staging"}"#,
        r#"{"schema_version":"0.1","state_dir":"relative","staging_dir":"/b","socket_path":"/s","available_backends":["mock"]}"#,
    ];
    for case in cases {
        let document: serde_json::Value = serde_json::from_str(case).expect("case parses");
        let schema_ok = schema.validate(&document).is_ok();
        let runtime_ok = AgentConfig::parse_json(case).is_ok();
        assert!(
            !schema_ok || runtime_ok,
            "the schema accepts a config the runtime refuses, so following the published \
             schema produces something that will not start: {case}"
        );
    }
}

#[test]
fn a_mistyped_field_is_refused_rather_than_silently_defaulted() {
    // `worker.mod` instead of `worker.mode` used to parse, leaving
    // `mode = Mock` -- so an operator asking for the real serving binary
    // got the in-process mock and served nothing real, with no error. The
    // schema said additionalProperties:false all along; only the runtime
    // disagreed.
    let mistyped = r#"{
        "schema_version": "0.1",
        "transport": "unix_socket",
        "socket_path": "/run/tensorplate/agent.sock",
        "state_dir": "/var/lib/tensorplate/state",
        "staging_dir": "/var/lib/tensorplate/bundles/staging",
        "available_backends": ["mock"],
        "worker": {"mod": "process"}
    }"#;
    let err = AgentConfig::parse_json(mistyped)
        .expect_err("an unknown field must not be accepted and defaulted");
    assert!(
        err.to_string().contains("mod"),
        "the error should name the offending field, got `{err}`"
    );
    // And the schema agrees, which is the point of the pair.
    let document: serde_json::Value = serde_json::from_str(mistyped).expect("parses");
    assert!(
        compiled_schema().validate(&document).is_err(),
        "the schema must reject it too"
    );
}
