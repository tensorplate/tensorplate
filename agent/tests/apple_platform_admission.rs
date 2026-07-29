// SPDX-License-Identifier: Apache-2.0
//
// Agent admission consumes only the platform registry's resolved Apple
// capability and typed rejection outcome.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;

use common::{vision_bundle, Harness};
use serde_json::Value;
use tensorplate_agent::{AgentError, Coordinator, PlatformAdmission};
use tensorplate_platform::{identify_platform, HostSources, PlatformReason, PlatformRegistry};

fn repo_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(relative)
}

fn registry() -> PlatformRegistry {
    PlatformRegistry::load(&repo_path("config/platform")).expect("registry loads")
}

fn sources(name: &str) -> HostSources {
    let path = repo_path(&format!("test/platform/host_identity/{name}.json"));
    let body = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    let fixture: Value = serde_json::from_str(&body).expect("fixture parses");
    let raw = &fixture["sources"];
    let text = |key: &str| raw.get(key).and_then(Value::as_str).map(str::to_string);
    HostSources {
        uname_machine: text("uname_machine"),
        os_release: text("os_release"),
        cpuinfo: text("cpuinfo"),
        nv_tegra_release: text("nv_tegra_release"),
        nvidia_jetpack_version: text("nvidia_jetpack_version"),
        device_tree_model: text("device_tree_model"),
        sw_vers_product_name: text("sw_vers_product_name"),
        sw_vers_product_version: text("sw_vers_product_version"),
        sw_vers_build_version: text("sw_vers_build_version"),
        cpu_brand: text("cpu_brand"),
        hw_memsize: text("hw_memsize"),
        gce_machine_type: text("gce_machine_type"),
    }
}

fn admission(name: &str) -> PlatformAdmission {
    let report = identify_platform(&sources(name)).expect("platform detects");
    PlatformAdmission::evaluate(&registry(), &report)
}

#[test]
fn supported_memory_bounds_agent_capacity_without_raising_a_smaller_limit() {
    let harness = Harness::new();
    let supported = admission("macos26-m1pro-16gb");
    assert_eq!(supported.row_id(), Some("macos26-m1pro-16gb"));
    assert_eq!(supported.reason(), None);

    let mut larger = harness.config.clone();
    larger.device_memory_bytes = Some(32_u64 * 1024 * 1024 * 1024);
    supported.apply_memory_limit(&mut larger);
    assert_eq!(
        larger.device_memory_bytes,
        Some(16_u64 * 1024 * 1024 * 1024)
    );

    let mut smaller = harness.config.clone();
    smaller.device_memory_bytes = Some(4_u64 * 1024 * 1024 * 1024);
    supported.apply_memory_limit(&mut smaller);
    assert_eq!(
        smaller.device_memory_bytes,
        Some(4_u64 * 1024 * 1024 * 1024)
    );
}

#[test]
fn planned_and_unknown_chips_keep_the_registry_reason() {
    let planned = admission("macos26-m4pro-24gb");
    assert_eq!(
        planned.reason(),
        Some(PlatformReason::RowPlannedNotValidated)
    );
    assert_eq!(planned.row_id(), Some("macos26-m4pro-24gb"));

    let unknown = admission("macos26-m2pro-16gb-unsupported");
    assert_eq!(
        unknown.reason(),
        Some(PlatformReason::UnsupportedAcceleratorSku)
    );
    assert_eq!(unknown.row_id(), None);
}

#[test]
fn rejected_platform_fails_before_bundle_staging_or_worker_prepare() {
    let harness = Harness::new();
    let rejected = admission("macos26-m3max-36gb-unsupported");
    let coordinator = Coordinator::new(
        harness.config.clone(),
        harness.store.clone(),
        harness.worker.clone(),
    )
    .with_platform_admission(rejected);
    let bundle = vision_bundle(harness.td.path(), "unsupported-apple");

    let err = coordinator
        .deploy("unsupported-apple", &bundle, BTreeMap::new(), None, None)
        .expect_err("unsupported chip must fail before model load");
    assert!(
        matches!(err, AgentError::UnsupportedHardware(ref detail) if detail.contains("unsupported_accelerator_sku")),
        "unexpected error: {err}"
    );
    assert!(
        !harness
            .config
            .staging_dir
            .join("unsupported-apple")
            .exists(),
        "platform rejection must happen before staging"
    );
    assert!(
        harness.worker.calls().expect("worker calls").is_empty(),
        "platform rejection must happen before worker prepare"
    );
}
