// SPDX-License-Identifier: Apache-2.0
//
// The agent answers platform questions from the shared registry.
//
// What matters here is not that the coordinator can store a registry but
// that it distinguishes "no registry loaded" from "a registry that
// contains nothing". The second means no platform is supported; the
// first means the agent has no basis to say either way, and a deploy
// admission check that confuses them would reject every deploy on a
// device whose package data merely failed to install.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::path::PathBuf;

use common::Harness;
use tensorplate_agent::coordinator::Coordinator;
use tensorplate_platform::{PlatformRegistry, SupportLevel};

fn committed_registry() -> PlatformRegistry {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("config/platform");
    PlatformRegistry::load(&dir).expect("the committed registry loads")
}

fn coordinator_with_registry(harness: &Harness, registry: PlatformRegistry) -> Coordinator {
    Coordinator::new(
        harness.config.clone(),
        harness.store.clone(),
        harness.worker.clone(),
    )
    .with_platform_registry(registry)
}

#[test]
fn a_coordinator_without_a_registry_reports_absence_not_emptiness() {
    let harness = Harness::new();
    assert!(
        harness.coord.platform_registry().is_none(),
        "an unattached registry must be `None`, never an empty registry"
    );
}

#[test]
fn the_attached_registry_answers_the_shared_query_api() {
    let harness = Harness::new();
    let coordinator = coordinator_with_registry(&harness, committed_registry());
    let registry = coordinator
        .platform_registry()
        .expect("the registry is attached");

    // Row lookup and support-level posture come from `tensorplate-platform`,
    // so the agent cannot form a second opinion about what is supported.
    let row = registry
        .row("macos26-m1pro-16gb")
        .expect("a committed Production row resolves by id");
    assert_eq!(row.support_level(), SupportLevel::Production);
    assert!(row.is_supported_combination());

    let planned = registry
        .row("jetson-agx-orin-64gb")
        .expect("a committed Planned row resolves by id");
    assert!(
        !planned.is_supported_combination(),
        "Planned rows are defined but carry no support claim"
    );
    assert!(
        !registry
            .supported_rows()
            .any(|r| r.row_id() == planned.row_id()),
        "a Planned row must never appear among supported combinations"
    );
}

#[test]
fn roadmap_targets_are_not_reachable_as_support_rows() {
    // The agent must not be able to admit a deploy against a roadmap
    // target: those are non-row future intentions with no evidence.
    let harness = Harness::new();
    let coordinator = coordinator_with_registry(&harness, committed_registry());
    let registry = coordinator
        .platform_registry()
        .expect("the registry is attached");

    for target in registry.roadmap_targets() {
        assert!(
            registry.row(target.target_id()).is_none(),
            "roadmap target `{}` must not resolve as a support row",
            target.target_id()
        );
    }
}
