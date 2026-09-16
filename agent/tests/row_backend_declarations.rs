// SPDX-License-Identifier: Apache-2.0
//
// A platform support row must not declare a backend path that the build
// installed on that row's hosts does not contain.
//
// Two committed artifacts answer "which backends can this machine serve?"
// and nothing in the running system compares them. The agent config is
// per-architecture: `packaging/debian/tensorplate-agent.install` selects
// `packaging/conf/agent.amd64.json` on amd64 and `packaging/conf/agent.json`
// everywhere else, because the x86_64 serving worker is built with
// `TP_ENABLE_TENSORRT=OFF` (`tools/release/amd64-build-profile.sh`) and has
// no TensorRT adapter compiled in. The platform rows are the other answer,
// and until issue #204 the x86_64 GPU rows still named a `tensorrt` package
// set satisfied by `tensorplate-serving`.
//
// The two are gates in series. The config is read first, so on a stock
// install a TensorRT bundle was already refused at compatibility
// evaluation; the row mattered where that conffile does not, because
// `/etc/tensorplate/agent.json` is a dpkg conffile an operator can edit.
// There deploy admission (`check_backend_packages`) reads only the row, so
// it found the package installed and admitted a bundle the worker can only
// refuse at engine lookup. `agent/tests/platform_admission.rs` pins that
// ordering; what the row said was wrong either way.
//
// This test is that missing comparison. It derives the shipped config from
// the row's own package channel and CPU architecture, and fails closed: a
// combination no packaging path ships is an error to state, not a row to
// skip.

#![allow(clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use tensorplate_agent::config::AgentConfig;
use tensorplate_platform::{CpuArchitecture, PackageChannel, PlatformRegistry, PlatformSupportRow};

fn repo_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(relative)
}

/// The agent config the packaging installs on a host this row matches.
///
/// `packaging/debian/tensorplate-agent.install` is the source for the apt
/// rows; the Homebrew formula installs the single macOS template. A
/// channel and architecture pair with no shipped config returns `None`
/// rather than falling back to one of the others: guessing here would let
/// a new row be checked against a config no host of that row ever reads.
fn shipped_agent_config(channel: PackageChannel, arch: CpuArchitecture) -> Option<&'static str> {
    match (channel, arch) {
        (PackageChannel::Apt, CpuArchitecture::X86_64) => Some("packaging/conf/agent.amd64.json"),
        (PackageChannel::Apt, CpuArchitecture::Arm64) => Some("packaging/conf/agent.json"),
        (PackageChannel::Homebrew, CpuArchitecture::Arm64) => {
            Some("packaging/homebrew/conf/agent.json.in")
        }
        (PackageChannel::Homebrew, CpuArchitecture::X86_64) => None,
    }
}

/// `available_backends` from one shipped agent config, parsed through the
/// agent's own validator so a config this test accepts is one the agent
/// would start on.
fn available_backends(relative: &str) -> BTreeSet<String> {
    let path = repo_path(relative);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        // The Homebrew config is a template; the prefix it interpolates is
        // a path, and no backend name depends on it.
        .replace("@HOMEBREW_PREFIX@", "/opt/homebrew");
    let config = AgentConfig::parse_json(&raw)
        .unwrap_or_else(|e| panic!("{} must validate: {e}", path.display()));
    config.available_backends.into_iter().collect()
}

/// Backend paths `row` declares that the config shipped for its hosts does
/// not advertise, each rendered as the sentence the failure prints.
fn unbacked_declarations(row: &PlatformSupportRow) -> Vec<String> {
    let arch = row.cpu().architecture;
    let mut found = Vec::new();
    for set in row.backend_packages() {
        let Some(relative) = shipped_agent_config(set.channel, arch) else {
            panic!(
                "row `{}` installs backend path `{}` through the {} channel on {}, \
                 and no packaging path ships an agent config for that combination; \
                 add the mapping to shipped_agent_config before adding the row",
                row.row_id(),
                set.backend_path,
                set.channel.as_str(),
                arch.as_str()
            );
        };
        if !available_backends(relative).contains(&set.backend_path) {
            found.push(format!(
                "row `{}` declares backend path `{}`, which {relative} does not list in \
                 available_backends",
                row.row_id(),
                set.backend_path
            ));
        }
    }
    found
}

fn committed_registry() -> PlatformRegistry {
    PlatformRegistry::load(&repo_path("config/platform")).expect("committed registry loads")
}

#[test]
fn no_row_declares_a_backend_the_shipped_build_does_not_contain() {
    let registry = committed_registry();
    let mut violations = Vec::new();
    for row in registry.rows() {
        violations.extend(unbacked_declarations(row));
    }
    assert!(
        violations.is_empty(),
        "a support row promises a backend path the installed build cannot serve, \
         so deploy admission would pass a bundle the worker refuses at engine lookup:\n{}",
        violations.join("\n")
    );
}

/// The guard above is not vacuous: the registry really does hold rows on
/// every channel and architecture the mapping names, so every shipped
/// config is exercised. A registry that loaded no rows, or a mapping that
/// stopped covering one of them, would otherwise pass by checking nothing.
/// Each config the mapping names is asserted, so none can quietly fall out
/// of the guard while the others keep it green.
#[test]
fn every_shipped_agent_config_is_exercised_by_committed_rows() {
    let registry = committed_registry();
    let mut exercised = BTreeSet::new();
    for row in registry.rows() {
        for set in row.backend_packages() {
            let relative = shipped_agent_config(set.channel, row.cpu().architecture)
                .unwrap_or_else(|| panic!("row `{}` has no shipped agent config", row.row_id()));
            exercised.insert(relative);
        }
    }
    assert!(
        exercised.contains("packaging/conf/agent.amd64.json"),
        "committed x86_64 apt rows must be checked against the amd64 config: {exercised:?}"
    );
    assert!(
        exercised.contains("packaging/conf/agent.json"),
        "committed arm64 apt rows must be checked against the default config: {exercised:?}"
    );
    assert!(
        exercised.contains("packaging/homebrew/conf/agent.json.in"),
        "committed Homebrew rows must be checked against the macOS template: {exercised:?}"
    );
}

/// The guard discriminates. Re-adding the `tensorrt` set issue #204
/// removed, to the committed L4 row and nothing else, must be caught —
/// and the same row without it must pass, so the check is rejecting for
/// its own reason rather than refusing x86_64 rows wholesale.
#[test]
fn a_reinstated_tensorrt_declaration_on_an_x86_row_is_caught() {
    let source = repo_path("config/platform/rows/ubuntu2404-x86-l4-g2s8.json");
    let raw = std::fs::read_to_string(source).expect("the L4 row is committed");

    let committed = PlatformSupportRow::from_json(&raw).expect("the committed row decodes");
    assert!(
        unbacked_declarations(&committed).is_empty(),
        "control: the committed L4 row declares only backends the amd64 build contains"
    );

    let mut value: serde_json::Value = serde_json::from_str(&raw).expect("row is JSON");
    value["backend_packages"]
        .as_array_mut()
        .expect("backend_packages is an array")
        .insert(
            0,
            serde_json::json!({
                "backend_path": "tensorrt",
                "channel": "apt",
                "packages": ["tensorplate-serving"]
            }),
        );
    let reinstated = PlatformSupportRow::from_json(&value.to_string())
        .expect("the row with tensorrt re-added is still schema-valid");

    let violations = unbacked_declarations(&reinstated);
    assert_eq!(
        violations.len(),
        1,
        "only the re-added path may be flagged: {violations:?}"
    );
    assert!(
        violations[0].contains("tensorrt") && violations[0].contains("agent.amd64.json"),
        "the failure must name the path and the config that contradicts it: {}",
        violations[0]
    );
}

/// The arm64 Jetson claim is untouched: its build enables TensorRT and its
/// config advertises it. Without this, removing the path from every row
/// would also satisfy the guard.
#[test]
fn the_jetson_row_still_declares_tensorrt() {
    let registry = committed_registry();
    let row = registry
        .row("jetson-orin-nano-8gb-jp62")
        .expect("the Orin Nano row is committed");
    assert!(
        row.backend_packages()
            .iter()
            .any(|set| set.backend_path == "tensorrt"),
        "the arm64 release build compiles the TensorRT adapter; its row must still say so"
    );
    assert!(
        unbacked_declarations(row).is_empty(),
        "and the arm64 agent config must advertise it"
    );
}

/// No x86_64 row declares it, whatever its accelerator or support level.
#[test]
fn no_x86_row_declares_tensorrt() {
    let registry = committed_registry();
    let offenders: Vec<&str> = registry
        .rows()
        .filter(|row| row.cpu().architecture == CpuArchitecture::X86_64)
        .filter(|row| {
            row.backend_packages()
                .iter()
                .any(|set| set.backend_path == "tensorrt")
        })
        .map(PlatformSupportRow::row_id)
        .collect();
    assert!(
        offenders.is_empty(),
        "the amd64 tensorplate-serving build sets TP_ENABLE_TENSORRT=OFF; these rows claim \
         otherwise: {offenders:?}"
    );
}

/// The premise the whole chain rests on, read from the files that carry
/// it rather than from a comment.
///
/// The guard above compares a row against the shipped agent config; the
/// config is right only because the build behind it compiles no TensorRT
/// adapter. Nothing pinned that last link. `test/release/
/// test_build_configuration.py` derives the amd64 configure arguments
/// from the profile instead of asserting them, and checks only that the
/// adapter is never asked for without its SDK — so flipping the flag and
/// leaving the config and the rows behind fails nothing today. The arm64
/// `ON` is pinned there, in `ARM64_SNAPSHOT_ARGS`; these two are not.
///
/// Turning either of these builds on is a legitimate change. It is not a
/// change that can be made alone, and this is where that is said.
#[test]
fn the_builds_behind_the_python_pytorch_only_configs_compile_no_tensorrt() {
    for (build, config) in [
        (
            "tools/release/amd64-build-profile.sh",
            "packaging/conf/agent.amd64.json",
        ),
        (
            "packaging/homebrew/Formula/tensorplate-serving.rb",
            "packaging/homebrew/conf/agent.json.in",
        ),
    ] {
        let path = repo_path(build);
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        assert!(
            raw.contains("-DTP_ENABLE_TENSORRT=OFF"),
            "{build} builds the serving worker for a platform whose agent config \
             ({config}) advertises no `tensorrt`, and whose rows therefore declare no \
             `tensorrt` package set. If it now compiles the adapter, both are what have \
             to change with it"
        );
        assert!(
            !available_backends(config).contains("tensorrt"),
            "{config} advertises `tensorrt` while {build} compiles no adapter"
        );
    }
}

/// The amd64 config is installed only by the dh-exec filter, so nothing
/// else parses it. Its path is read here rather than assumed.
#[test]
fn the_amd64_agent_config_is_the_one_the_packaging_installs() {
    let install = repo_path("packaging/debian/tensorplate-agent.install");
    let raw = std::fs::read_to_string(install).expect("the install file is committed");
    assert!(
        raw.contains("[amd64] packaging/conf/agent.amd64.json"),
        "the amd64 agent config path this test derives must be the one dh-exec selects:\n{raw}"
    );
    assert!(
        raw.contains("[!amd64] packaging/conf/agent.json"),
        "the non-amd64 agent config path this test derives must be the one dh-exec selects:\n{raw}"
    );
    assert!(
        repo_path("packaging/conf/agent.amd64.json").exists(),
        "the amd64 agent config must exist"
    );
}

/// The macOS arm of the mapping is derived from packaging too, rather
/// than being the one entry taken on trust. A template swapped for one of
/// the apt configs would otherwise check the Homebrew rows against a file
/// no Mac ever reads — and `packaging/conf/agent.json` advertises
/// `tensorrt`, so that swap would silently retire this guard on macOS.
#[test]
fn the_homebrew_agent_config_is_the_one_the_formula_installs() {
    let formula = repo_path("packaging/homebrew/Formula/tensorplate-agent.rb");
    let raw = std::fs::read_to_string(formula).expect("the formula is committed");
    assert!(
        raw.contains("packaging/homebrew/conf/agent.json.in"),
        "the Homebrew agent config path this test derives must be the one the formula reads:\n{raw}"
    );
    assert!(
        raw.contains(r#"install config => "agent.json""#),
        "and the formula must install it as the agent config:\n{raw}"
    );
    assert!(
        repo_path("packaging/homebrew/conf/agent.json.in").exists(),
        "the Homebrew agent config template must exist"
    );
}
