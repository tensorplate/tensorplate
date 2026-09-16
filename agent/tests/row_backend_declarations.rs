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

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use tensorplate_agent::config::AgentConfig;
use tensorplate_platform::{CpuArchitecture, PackageChannel, PlatformRegistry, PlatformSupportRow};

fn repo_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(relative)
}

/// One line of the dh-exec install file, the shell build profile or a Ruby
/// formula, with its comment removed.
///
/// A `#` opens a comment where it begins the line or follows whitespace,
/// which holds in all three and leaves Ruby's `"#{...}"` interpolation
/// alone. Every derivation below reads code lines only. A substring search
/// over whole file text is satisfied by a retired line left behind as a
/// comment, which is how a packaging or build change that reverts one of
/// these mappings would otherwise keep its own guard green.
fn without_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'#' && (index == 0 || bytes[index - 1].is_ascii_whitespace()) {
            return &line[..index];
        }
    }
    line
}

/// Every config `packaging/debian/tensorplate-agent.install` installs as
/// `etc/tensorplate/agent.json`, keyed by the dh-exec architecture filter
/// that selects it.
///
/// Both dh-exec shapes are read, because the file uses both: an explicit
/// `src => dest` rename, and an install into a directory that keeps the
/// source basename. The destination is half the match. A config that lands
/// anywhere other than the path `packaging/debian/tensorplate-agent.service`
/// starts the agent with is not one any host of that architecture reads,
/// and a row checked against it would be checked against nothing.
fn debian_installed_agent_configs() -> BTreeMap<String, String> {
    let path = repo_path("packaging/debian/tensorplate-agent.install");
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let mut installed = BTreeMap::new();
    for line in raw.lines() {
        let fields: Vec<&str> = without_comment(line).split_whitespace().collect();
        // dh-exec puts the architecture filter first. A line without one
        // installs everywhere and selects no per-architecture config.
        let Some((filter, entry)) = fields.split_first() else {
            continue;
        };
        if !(filter.starts_with('[') && filter.ends_with(']')) {
            continue;
        }
        let (source, destination) = match entry {
            [source, "=>", destination] => ((*source).to_string(), (*destination).to_string()),
            [source, directory] => {
                let base = source.rsplit('/').next().unwrap_or(source);
                (
                    (*source).to_string(),
                    format!("{}/{base}", directory.trim_end_matches('/')),
                )
            }
            _ => continue,
        };
        if destination == "etc/tensorplate/agent.json" {
            installed.insert((*filter).to_string(), source);
        }
    }
    installed
}

/// The config dh-exec installs as the agent conffile under `filter`.
fn debian_agent_config(filter: &str) -> String {
    let installed = debian_installed_agent_configs();
    installed.get(filter).cloned().unwrap_or_else(|| {
        panic!(
            "packaging/debian/tensorplate-agent.install installs no agent config as \
             etc/tensorplate/agent.json under `{filter}`; it installs {installed:?}. \
             A host that filter selects reads no file at the path its systemd unit names, \
             so there is nothing for its rows to be checked against"
        )
    })
}

/// The template `packaging/homebrew/Formula/tensorplate-agent.rb` installs
/// as the agent's `agent.json`.
///
/// Both halves are required. A formula that assigns one template and
/// installs another, or installs it under a different name, ships no
/// config at the path its service block runs the agent with.
fn homebrew_agent_config() -> String {
    let path = repo_path("packaging/homebrew/Formula/tensorplate-agent.rb");
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let mut template = None;
    let mut installs_it_as_agent_json = false;
    for line in raw.lines() {
        let code = without_comment(line);
        if let Some(assigned) = code.trim().strip_prefix("config = buildpath/") {
            template = Some(assigned.trim().trim_matches('"').to_string());
        }
        if code.contains(r#"install config => "agent.json""#) {
            installs_it_as_agent_json = true;
        }
    }
    let template = template.unwrap_or_else(|| {
        panic!(
            "{} must assign the agent config template it ships to `config`",
            path.display()
        )
    });
    assert!(
        installs_it_as_agent_json,
        "{} assigns `{template}` but does not install it as agent.json, so no Mac reads it",
        path.display()
    );
    template
}

/// The agent config the packaging installs on a host this row matches.
///
/// Both answers are read out of the packaging rather than restated here:
/// the apt pair from the dh-exec filters in
/// `packaging/debian/tensorplate-agent.install`, the macOS one from the
/// Homebrew formula. What stays in this file is only the pairing of an
/// architecture with the filter that selects it, which is a fact about
/// architectures rather than about packaging. A channel and architecture
/// pair with no shipped config returns `None` rather than falling back to
/// one of the others: guessing here would let a new row be checked against
/// a config no host of that row ever reads.
fn shipped_agent_config(channel: PackageChannel, arch: CpuArchitecture) -> Option<String> {
    match (channel, arch) {
        (PackageChannel::Apt, CpuArchitecture::X86_64) => Some(debian_agent_config("[amd64]")),
        (PackageChannel::Apt, CpuArchitecture::Arm64) => Some(debian_agent_config("[!amd64]")),
        (PackageChannel::Homebrew, CpuArchitecture::Arm64) => Some(homebrew_agent_config()),
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
        if !available_backends(&relative).contains(&set.backend_path) {
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
///
/// The flag is read as a value, not as a spelling. `TP_ENABLE_TENSORRT` is
/// a CMake cache variable, so a second `-D` for it wins over the first,
/// and a whole-file substring search for the `OFF` spelling is satisfied
/// by a comment that mentions it. Either shape turns the adapter on while
/// leaving this test green, which is the same class of gap as a row that
/// declares a backend nothing backs.
#[test]
fn the_builds_behind_the_python_pytorch_only_configs_compile_no_tensorrt() {
    for (build, channel, arch) in [
        (
            "tools/release/amd64-build-profile.sh",
            PackageChannel::Apt,
            CpuArchitecture::X86_64,
        ),
        (
            "packaging/homebrew/Formula/tensorplate-serving.rb",
            PackageChannel::Homebrew,
            CpuArchitecture::Arm64,
        ),
    ] {
        // The config side is the same derivation the row guard uses, so a
        // packaging change that points this architecture at another config
        // is compared against that build here too.
        let config = shipped_agent_config(channel, arch).unwrap_or_else(|| {
            panic!("{build} builds the worker for hosts the packaging ships no agent config for")
        });
        let path = repo_path(build);
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let code: Vec<&str> = raw.lines().map(without_comment).collect();
        assert!(
            code.iter()
                .any(|line| line.contains("-DTP_ENABLE_TENSORRT=OFF")),
            "{build} builds the serving worker for a platform whose agent config \
             ({config}) advertises no `tensorrt`, and whose rows therefore declare no \
             `tensorrt` package set. If it now compiles the adapter, both are what have \
             to change with it"
        );
        assert!(
            !code
                .iter()
                .any(|line| line.contains("-DTP_ENABLE_TENSORRT=ON")),
            "{build} configures the TensorRT adapter on while {config} advertises none; \
             a later -D for the same cache variable is the value CMake takes"
        );
        assert!(
            !available_backends(&config).contains("tensorrt"),
            "{config} advertises `tensorrt` while {build} compiles no adapter"
        );
    }
}

/// The amd64 config is installed only by the dh-exec filter, so nothing
/// else parses it. The whole mapping is pinned here, source and installed
/// path together and as a set: a filter whose config stops landing at the
/// conffile path ships nothing an amd64 host reads, and a filter added
/// later selects a config no test compares a row against.
#[test]
fn the_amd64_agent_config_is_the_one_the_packaging_installs() {
    let installed = debian_installed_agent_configs();
    let mapping: Vec<(&str, &str)> = installed
        .iter()
        .map(|(filter, source)| (filter.as_str(), source.as_str()))
        .collect();
    assert_eq!(
        mapping,
        vec![
            ("[!amd64]", "packaging/conf/agent.json"),
            ("[amd64]", "packaging/conf/agent.amd64.json"),
        ],
        "these are the configs dh-exec installs as /etc/tensorplate/agent.json, and the \
         per-architecture answers every check in this file is derived from"
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
    assert_eq!(
        homebrew_agent_config(),
        "packaging/homebrew/conf/agent.json.in",
        "the Homebrew agent config path this test derives must be the template the formula \
         installs as agent.json"
    );
    assert!(
        repo_path("packaging/homebrew/conf/agent.json.in").exists(),
        "the Homebrew agent config template must exist"
    );
}
