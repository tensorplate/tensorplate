// SPDX-License-Identifier: Apache-2.0
//
// Gate-semantic handling, per row, against the committed registry.
//
// The three postures a row can declare for a signal are genuinely
// different, and every pair of them is plausible to collapse:
//
//   load_bearing  a failed sensor is a machine that cannot be trusted
//   context_only  a failed sensor is a missing number
//   not_applicable the sensor was never there; nothing failed
//
// These assert the row's declaration decides which applies, and that a
// row that says a signal is absent carries its own explanation rather
// than borrowing a typed platform reason -- those say why a PLATFORM is
// unsupported, and a sensor macOS does not expose is not a support claim.

#![allow(clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use tensorplate_platform::row::GateValue;
use tensorplate_platform::{
    PlatformReason, PlatformRegistry, SignalName, SignalOutcome, SignalTelemetry,
};

fn registry() -> PlatformRegistry {
    PlatformRegistry::load(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("config/platform"),
    )
    .expect("registry loads")
}

fn everything_answered() -> BTreeMap<SignalName, SignalOutcome> {
    SignalName::all()
        .into_iter()
        .map(|name| (name, SignalOutcome::Collected))
        .collect()
}

fn one_failure(name: SignalName) -> BTreeMap<SignalName, SignalOutcome> {
    let mut outcomes = everything_answered();
    outcomes.insert(
        name,
        SignalOutcome::Unavailable {
            detail: "sensor read failed".into(),
        },
    );
    outcomes
}

fn telemetry(row_id: &str, outcomes: &BTreeMap<SignalName, SignalOutcome>) -> SignalTelemetry {
    let registry = registry();
    let row = registry
        .row(row_id)
        .unwrap_or_else(|| panic!("{row_id} is committed"));
    SignalTelemetry::resolve(row, outcomes)
}

#[test]
fn the_edge_and_datacenter_rows_declare_thermal_differently() {
    // The split the whole posture concept exists for: a Jetson's cooling
    // is the operator's problem, a managed datacenter chassis is not.
    let jetson = telemetry("jetson-orin-nano-8gb-jp62", &everything_answered());
    assert_eq!(
        jetson.signal(SignalName::Thermal).expect("declared").gate,
        GateValue::LoadBearing
    );
    for datacenter in [
        "ubuntu2404-x86-l4-g2s8",
        "ubuntu2404-x86-rtxpro6000se-g4s48",
        "macos26-m1pro-16gb",
    ] {
        assert_eq!(
            telemetry(datacenter, &everything_answered())
                .signal(SignalName::Thermal)
                .expect("declared")
                .gate,
            GateValue::ContextOnly,
            "{datacenter}: thermal informs rather than decides here"
        );
    }
}

#[test]
fn a_failed_load_bearing_sensor_degrades_deployment() {
    // A Jetson that cannot read its own thermal state cannot be trusted
    // to throttle itself, which is a deployment concern and not a
    // reporting one.
    let jetson = telemetry(
        "jetson-orin-nano-8gb-jp62",
        &one_failure(SignalName::Thermal),
    );
    assert_eq!(
        jetson.degraded_reason(),
        Some(PlatformReason::TelemetryDegraded)
    );
    assert!(jetson.degrades_deployment());
}

#[test]
fn an_empty_outcome_map_fails_applicable_load_bearing_signals_closed() {
    let jetson = telemetry("jetson-orin-nano-8gb-jp62", &BTreeMap::new());
    assert!(jetson.degrades_deployment());
    assert_eq!(
        jetson.degraded_reason(),
        Some(PlatformReason::TelemetryDegraded)
    );
    for name in [
        SignalName::Thermal,
        SignalName::Power,
        SignalName::Throttle,
        SignalName::Memory,
        SignalName::GpuUtilization,
    ] {
        assert!(
            matches!(
                jetson.signal(name).expect("declared").outcome,
                Some(SignalOutcome::Unavailable { .. })
            ),
            "{}: omission must be an unavailable result",
            name.as_str()
        );
    }
}

#[test]
fn a_partial_outcome_map_cannot_turn_an_applicable_signal_into_not_applicable() {
    let mut outcomes = everything_answered();
    outcomes.remove(&SignalName::Thermal);
    let jetson = telemetry("jetson-orin-nano-8gb-jp62", &outcomes);
    let thermal = jetson.signal(SignalName::Thermal).expect("declared");
    assert_eq!(thermal.gate, GateValue::LoadBearing);
    assert_eq!(
        thermal.outcome,
        Some(SignalOutcome::Unavailable {
            detail: "collector produced no outcome".into()
        })
    );
    assert!(jetson.degrades_deployment());
}

#[test]
fn a_failed_context_only_sensor_is_reported_without_blocking() {
    // The same failure on a datacenter row. It is recorded -- the reason
    // is still telemetry_degraded -- and it does not block, because the
    // row already said this signal informs rather than decides.
    // Refusing here would make a context signal load-bearing by the back
    // door, which is the row's decision to make and not this code's.
    let l4 = telemetry("ubuntu2404-x86-l4-g2s8", &one_failure(SignalName::Thermal));
    assert_eq!(
        l4.degraded_reason(),
        Some(PlatformReason::TelemetryDegraded),
        "a failure is still reported"
    );
    assert!(
        !l4.degrades_deployment(),
        "a context-only failure must not block a deploy"
    );
}

#[test]
fn a_signal_the_row_declares_absent_never_reads_as_a_failure() {
    // macOS exposes neither per-device power nor GPU utilization without
    // privileged access. Treating that as a collector failure would
    // report every Mac as degraded forever.
    let mac = telemetry("macos26-m1pro-16gb", &everything_answered());
    for absent in [SignalName::Power, SignalName::GpuUtilization] {
        let status = mac.signal(absent).expect("declared");
        assert_eq!(status.gate, GateValue::NotApplicable);
        assert!(
            status.outcome.is_none(),
            "{}: nothing was asked for, so nothing succeeded or failed",
            absent.as_str()
        );
    }
    assert_eq!(mac.degraded_reason(), None);
    assert!(!mac.degrades_deployment());
}

#[test]
fn an_absent_signal_carries_the_rows_own_words_not_a_typed_reason() {
    // The reason is a free-text row fact. The typed vocabulary says why a
    // PLATFORM is unsupported; a sensor macOS does not expose is not a
    // support claim about the machine, and borrowing a typed value would
    // make it read as one.
    let mac = telemetry("macos26-m1pro-16gb", &everything_answered());
    let absent = mac.not_applicable();
    assert!(!absent.is_empty(), "the macOS row declares absent signals");
    for (name, reason) in absent {
        let reason = reason.unwrap_or_else(|| panic!("{}: absent needs a reason", name.as_str()));
        assert!(
            !reason.is_empty(),
            "{}: the reason must say something",
            name.as_str()
        );
        assert!(
            PlatformReason::ALL
                .iter()
                .all(|typed| typed.as_str() != reason),
            "{}: `{reason}` is a typed platform reason, not a row fact",
            name.as_str()
        );
    }
}

#[test]
fn every_committed_row_declares_a_reason_wherever_it_declares_absence() {
    // The registry enforces this at load; asserting it here is what says
    // the telemetry can rely on it rather than defending against a row
    // that declares absence and explains nothing.
    let registry = registry();
    for row in registry.rows() {
        let telemetry = SignalTelemetry::resolve(row, &everything_answered());
        for (name, reason) in telemetry.not_applicable() {
            assert!(
                reason.is_some_and(|r| !r.is_empty()),
                "{}: {} is absent with no explanation",
                row.row_id(),
                name.as_str()
            );
        }
    }
}
