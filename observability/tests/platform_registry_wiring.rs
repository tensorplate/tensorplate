// SPDX-License-Identifier: Apache-2.0
//
// The observability service reads the same platform registry the agent
// and the CLI read.
//
// Telemetry posture is a per-row fact — which platform signals are
// load-bearing on a given row is recorded in that row's gate semantics,
// not decided by the observability service. Wiring the service to the
// shared registry is what keeps those two from drifting apart.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;

use tensorplate_observability::{ObservabilityConfig, Service};
use tensorplate_platform::{GateValue, PlatformRegistry};

fn committed_registry() -> PlatformRegistry {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("config/platform");
    PlatformRegistry::load(&dir).expect("the committed registry loads")
}

fn service() -> Service {
    Service::new(ObservabilityConfig::default()).expect("default config builds a service")
}

#[test]
fn a_service_without_a_registry_reports_absence_not_emptiness() {
    // The constructor never loads the registry: if it did, every test in
    // this crate would behave differently on a host that happens to have
    // the packages installed.
    assert!(
        service().platform_registry().is_none(),
        "an unattached registry must be `None`, never an empty registry"
    );
}

#[test]
fn the_attached_registry_answers_the_shared_query_api() {
    let service = service().with_platform_registry(committed_registry());
    let registry = service
        .platform_registry()
        .expect("the registry is attached");

    // Gate semantics are read from the row, never inferred here.
    let row = registry
        .row("macos26-m1pro-16gb")
        .expect("a committed Production row resolves by id");
    assert_eq!(
        row.gate_semantics().gpu_utilization.gate,
        GateValue::NotApplicable,
        "unified-memory Apple rows declare GPU utilization not applicable"
    );

    // Every row declares a posture for every signal, so a consumer never
    // has to guess what an omitted signal meant.
    for row in registry.rows() {
        let gates = row.gate_semantics();
        for gate in [
            &gates.thermal,
            &gates.power,
            &gates.throttle,
            &gates.memory,
            &gates.gpu_utilization,
        ] {
            if gate.gate == GateValue::NotApplicable {
                assert!(
                    gate.reason.is_some(),
                    "{}: a not-applicable signal must say why",
                    row.row_id()
                );
            }
        }
    }
}
