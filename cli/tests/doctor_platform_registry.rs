// SPDX-License-Identifier: Apache-2.0
//
// `doctor` reports the installed platform support registry.
//
// The registry is read-only package data that the agent, this CLI, and
// the observability service all read from one location, so the operator
// needs to be told which of three states a device is in: no registry
// installed, a registry that loads, or a registry that is present but
// unusable. Collapsing the first and the third would send an operator
// looking for a missing package when the package is installed and
// corrupt.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use tensorplate_cli::commands::doctor::finding::{Finding, FindingId, FindingStatus, Severity};
use tensorplate_cli::commands::doctor::install::{run, InstallProbeOptions};
use tensorplate_protocol::install_paths::PLATFORM_REGISTRY_DIR;

fn committed_registry_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("config/platform")
}

/// Options that read a staged install tree instead of the real one, with
/// every probe that shells out or touches the network switched off.
fn staged_options(prefix: &Path) -> InstallProbeOptions {
    InstallProbeOptions {
        prefix: Some(prefix.to_path_buf()),
        probe_backends: false,
        skip_systemd: true,
    }
}

/// Copy the committed registry into a staged install tree at the exact
/// path the packages install it to.
fn stage_registry(prefix: &Path) -> PathBuf {
    let staged = prefix.join(PLATFORM_REGISTRY_DIR.trim_start_matches('/'));
    for sub in ["rows", "roadmap_targets"] {
        let target = staged.join(sub);
        fs::create_dir_all(&target).expect("create staged registry dir");
        for entry in fs::read_dir(committed_registry_dir().join(sub)).expect("read committed dir") {
            let path = entry.expect("dir entry").path();
            let name = path.file_name().expect("file name");
            fs::copy(&path, target.join(name)).expect("stage document");
        }
    }
    staged
}

fn platform_finding(prefix: &Path) -> Finding {
    run(&staged_options(prefix))
        .into_iter()
        .find(|f| f.id == FindingId::PlatformRegistry)
        .expect("the platform registry probe runs as part of the install probes")
}

#[test]
fn the_production_probe_reads_the_installed_registry_location() {
    // With no prefix, `doctor` reads exactly what
    // `PlatformRegistry::load_installed` reads in the agent and the
    // observability service. Asserted on the message rather than assumed,
    // because a second path constant here would be invisible until the
    // three disagreed on a real device.
    let opts = InstallProbeOptions {
        prefix: None,
        probe_backends: false,
        skip_systemd: true,
    };
    let finding = run(&opts)
        .into_iter()
        .find(|f| f.id == FindingId::PlatformRegistry)
        .expect("the platform registry probe runs");
    assert!(
        finding.message.contains(PLATFORM_REGISTRY_DIR),
        "every outcome names the directory read: {}",
        finding.message
    );
}

#[test]
fn an_uninstalled_registry_is_missing_not_a_failure() {
    // Dev hosts and CI runners have no install layout. `doctor` passes
    // there by contract, so an absent registry is `missing`/Info.
    let td = TempDir::new().expect("td");
    let finding = platform_finding(td.path());
    assert_eq!(finding.status, FindingStatus::Missing);
    assert_eq!(finding.severity, Severity::Info);
    assert!(
        finding
            .hint
            .as_deref()
            .unwrap_or_default()
            .contains("tensorplate-common"),
        "the hint names the package that ships it: {finding:?}"
    );
}

#[test]
fn an_installed_registry_loads_and_reports_its_contents() {
    let td = TempDir::new().expect("td");
    stage_registry(td.path());
    let finding = platform_finding(td.path());
    assert_eq!(
        finding.status,
        FindingStatus::Pass,
        "the committed registry must load: {finding:?}"
    );
    // The counts come from the same query API the agent uses, so a row
    // that stops being a supported combination changes what `doctor`
    // prints rather than passing silently.
    assert!(
        finding.message.contains("12 rows") && finding.message.contains("7 supported"),
        "counts come from the query API: {}",
        finding.message
    );
}

#[test]
fn a_corrupt_row_fails_the_whole_registry() {
    // One bad row means no registry. Reporting "11 of 12 rows" would let
    // a supported machine be told it is unsupported.
    let td = TempDir::new().expect("td");
    let staged = stage_registry(td.path());
    let row = staged.join("rows/macos26-m1pro-16gb.json");
    let good = fs::read_to_string(&row).expect("read staged row");
    let broken = good.replace(
        "\"support_level\": \"Production\"",
        "\"support_level\": \"Rumoured\"",
    );
    assert_ne!(broken, good, "the mutation applied");
    fs::write(&row, broken).expect("write broken row");

    let finding = platform_finding(td.path());
    assert_eq!(finding.status, FindingStatus::Fail);
    assert_eq!(finding.severity, Severity::Critical);
    assert!(
        finding.message.contains("macos26-m1pro-16gb"),
        "the message names the offending document: {}",
        finding.message
    );
}

#[test]
fn an_empty_registry_directory_fails_rather_than_reporting_zero_rows() {
    // A registry with no rows answers "unsupported" for every machine on
    // earth. That must read as broken, not as a clean pass with 0 rows.
    let td = TempDir::new().expect("td");
    let staged = td
        .path()
        .join(PLATFORM_REGISTRY_DIR.trim_start_matches('/'));
    fs::create_dir_all(staged.join("rows")).expect("create rows");
    fs::create_dir_all(staged.join("roadmap_targets")).expect("create targets");

    let finding = platform_finding(td.path());
    assert_eq!(finding.status, FindingStatus::Fail);
    assert_eq!(finding.severity, Severity::Critical);
}
