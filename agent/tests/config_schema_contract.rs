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
