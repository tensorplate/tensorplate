// SPDX-License-Identifier: Apache-2.0
//
// The strictness floor a row implies, and the rules that keep the policy
// replaceable without touching the mechanism.
//
// The floor is DERIVED from gate semantics rather than stored, so these
// cases read the committed registry rather than a fixture: if a row author
// changes a gate, the floor moves with it and that is the intent.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use tensorplate_platform::{AdmissionPosture, GateValue, PlatformRegistry};

fn registry() -> PlatformRegistry {
    PlatformRegistry::load(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("config/platform"),
    )
    .expect("registry loads")
}

#[test]
fn the_floor_follows_the_chassis_gates_a_row_already_declares() {
    // The whole mechanism. A row that acts on thermal, power or throttle is
    // one whose cooling belongs to the operator; a row that merely reports
    // them is a managed machine. Asserted against every committed row, so
    // this cannot drift from what the rows actually say.
    let registry = registry();
    let mut strict = 0;
    let mut loose = 0;
    for row in registry.rows() {
        let gates = row.gate_semantics();
        let chassis_gated = [&gates.thermal, &gates.power, &gates.throttle]
            .into_iter()
            .any(|gate| gate.gate == GateValue::LoadBearing);
        let floor = AdmissionPosture::floor_for(row);
        if chassis_gated {
            assert_eq!(
                floor,
                AdmissionPosture::ValidatedRowRequired,
                "`{}` acts on a chassis signal, so its evidence does not transfer",
                row.row_id()
            );
            strict += 1;
        } else {
            assert_eq!(
                floor,
                AdmissionPosture::TechnicalPrerequisites,
                "`{}` only reports chassis signals",
                row.row_id()
            );
            loose += 1;
        }
    }
    assert!(strict > 0 && loose > 0, "both kinds must be represented");
}

#[test]
fn the_same_card_in_two_chassis_gets_two_floors() {
    // The case that makes "bare metal is risky" the wrong unit of analysis.
    // These are the same accelerator family; one is a workstation whose
    // cooling is somebody's desk and one is a datacenter server.
    let registry = registry();
    let workstation = registry
        .row("ubuntu2404-x86-rtxpro6000we-physical")
        .expect("the workstation row is committed");
    let server = registry
        .row("ubuntu2404-x86-rtxpro6000se-g4s48")
        .expect("the server row is committed");

    assert_eq!(
        AdmissionPosture::floor_for(workstation),
        AdmissionPosture::ValidatedRowRequired
    );
    assert_eq!(
        AdmissionPosture::floor_for(server),
        AdmissionPosture::TechnicalPrerequisites
    );
}

#[test]
fn an_operator_may_tighten_but_never_loosen_below_the_floor() {
    let strict = AdmissionPosture::ValidatedRowRequired;
    let loose = AdmissionPosture::TechnicalPrerequisites;

    // Tightening a loose row: honoured.
    assert_eq!(AdmissionPosture::resolve(loose, Some(strict)), strict);
    // Loosening a strict row: refused. This is what makes an edge row's
    // requirement a property of the hardware rather than a default someone
    // can switch off.
    assert_eq!(AdmissionPosture::resolve(strict, Some(loose)), strict);
    // No preference: the row decides, which is the v0.2.1 decision.
    assert_eq!(AdmissionPosture::resolve(loose, None), loose);
    assert_eq!(AdmissionPosture::resolve(strict, None), strict);
}

#[test]
fn the_resolved_posture_says_where_it_came_from() {
    // Reporting the value alone would not let an operator tell a default
    // they inherited from a choice they made -- which is the difference
    // between being able to pin it and being surprised by a future change.
    let strict = AdmissionPosture::ValidatedRowRequired;
    let loose = AdmissionPosture::TechnicalPrerequisites;

    assert_eq!(AdmissionPosture::provenance(loose, None), "row floor");
    assert_eq!(
        AdmissionPosture::provenance(loose, Some(strict)),
        "operator"
    );
    assert_eq!(
        AdmissionPosture::provenance(strict, Some(loose)),
        "row floor",
        "a request that changed nothing did not decide the outcome"
    );
}

#[test]
fn an_unknown_posture_is_an_error_not_a_fallback() {
    // A posture a future release adds must not be silently ignored by an
    // older agent, which would then run at a strictness nobody chose.
    assert!("technical_prerequisites"
        .parse::<AdmissionPosture>()
        .is_ok());
    assert!("validated_row_required".parse::<AdmissionPosture>().is_ok());
    for bad in ["", "strict", "TECHNICAL_PREREQUISITES", "prerequisites"] {
        let err = bad
            .parse::<AdmissionPosture>()
            .expect_err("an unrecognized posture must not resolve to a default");
        assert!(
            err.contains("unknown admission posture"),
            "the error names the problem: {err}"
        );
    }
}

#[test]
fn posture_stays_out_of_the_row_documents_and_the_schema() {
    // A reversibility gate, asserted rather than intended. Storing the
    // posture per row would make a future policy change a schema bump plus
    // a golden regeneration plus a migration of every committed row.
    // Deriving it keeps the rule in one function.
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("config/platform/rows");
    let schema = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("config/schemas/platform_support_row.json");
    let schema_body = std::fs::read_to_string(schema).expect("read schema");
    assert!(
        !schema_body.contains("admission_posture"),
        "the row schema declares an admission posture; storing it would make a \
         policy change a schema bump plus a migration of every row"
    );

    for entry in std::fs::read_dir(dir).expect("read rows") {
        let path = entry.expect("entry").path();
        let body = std::fs::read_to_string(&path).expect("read row");
        assert!(
            !body.contains("admission_posture"),
            "`{}` stores an admission posture; the floor is derived from gate \
             semantics so a policy change stays a one-function edit",
            path.display()
        );
    }
}
