// SPDX-License-Identifier: Apache-2.0
//
// packaging: parse the shipped packaging config under
// `packaging/conf/cli.json`. Confirms the default profile uses the
// agent's Unix domain socket, that the config discovery chain picks the
// shipped file up without any env override, and that the Debian package
// installs it at the path that chain reads.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};

use tensorplate_cli::config::SYSTEM_CLI_CONFIG_PATH;
use tensorplate_cli::{CliConfig, ConfigSource, ProfileMode};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn packaging_cli_config_path() -> PathBuf {
    repo_root().join("packaging").join("conf").join("cli.json")
}

fn homebrew_cli_config_path() -> PathBuf {
    repo_root()
        .join("packaging")
        .join("homebrew")
        .join("conf")
        .join("cli.json.in")
}

#[test]
fn shipped_cli_config_parses_and_validates() {
    let raw = fs::read_to_string(packaging_cli_config_path()).expect("read cli.json");
    let cfg = CliConfig::parse_json(&raw).expect("packaging cli.json should validate");

    assert_eq!(cfg.default_profile, "local");
    let local = cfg
        .profiles
        .get("local")
        .expect("local profile must be present");
    assert!(matches!(local.mode, ProfileMode::Local));
    let socket = local
        .socket_path
        .as_ref()
        .expect("local profile must define socket_path");
    assert_eq!(socket, std::path::Path::new("/run/tensorplate/agent.sock"));
}

/// The whole point of the packaged conffile: with neither `--config` nor
/// `$TENSORPLATE_CLI_CONFIG`, discovery lands on it and the packaged
/// socket path is what commands and `doctor` report.
#[test]
fn discovery_with_nothing_set_lands_on_the_shipped_config() {
    let resolved = CliConfig::resolve_from(None, None, &packaging_cli_config_path())
        .expect("the shipped config must resolve");
    assert_eq!(
        resolved.source,
        ConfigSource::System(packaging_cli_config_path())
    );
    let socket = resolved
        .config
        .profile(&resolved.config.default_profile)
        .expect("default profile")
        .socket_path
        .clone()
        .expect("local profile must define socket_path");
    assert_eq!(socket, Path::new("/run/tensorplate/agent.sock"));
    assert!(resolved.warning.is_none());
}

/// Nothing in a native install writes an NDJSON log: both services log to
/// the journal and the agent writes plain text. A `log_source.path` here
/// would make `tensorplate logs` either fail to stat a file that never
/// exists or return zero entries and say nothing about why.
#[test]
fn shipped_cli_config_names_no_log_path_without_a_writer() {
    let raw = fs::read_to_string(packaging_cli_config_path()).expect("read cli.json");
    let cfg = CliConfig::parse_json(&raw).expect("packaging cli.json should validate");
    assert_eq!(
        cfg.log_source.path, None,
        "the Debian cli config must not name a log file no component writes"
    );
}

/// Discovery reads one fixed path; the package must install the conffile
/// exactly there, or the fix above resolves nothing on a real host.
#[test]
fn the_debian_package_installs_the_config_where_discovery_reads_it() {
    let manifest = repo_root()
        .join("packaging")
        .join("debian")
        .join("tensorplate-cli.install");
    let body = fs::read_to_string(&manifest).expect("read tensorplate-cli.install");
    let system = Path::new(SYSTEM_CLI_CONFIG_PATH);
    let destination = system
        .parent()
        .expect("system config path has a parent")
        .strip_prefix("/")
        .expect("system config path is absolute");
    let expected = format!(
        "packaging/conf/{}",
        system
            .file_name()
            .and_then(|n| n.to_str())
            .expect("system config file name")
    );
    let installed = body.lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields.next() == Some(expected.as_str())
            && fields
                .next()
                .is_some_and(|dest| Path::new(dest.trim_end_matches('/')) == destination)
    });
    assert!(
        installed,
        "{} must install {expected} to {}/",
        manifest.display(),
        destination.display()
    );
}

/// `resolve()` reads the environment with `var_os`, not `var`. The
/// difference only shows on a path that is not valid UTF-8: `var` would
/// discard it and fall through to the packaged conffile, so a config the
/// operator named would silently stop being used. Driven through the
/// binary because that wiring lives in `resolve()`, which the unit tests
/// cannot reach.
#[test]
#[cfg(unix)]
fn a_non_utf8_environment_config_is_used_not_discarded() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::process::Command;

    let td = tempfile::tempdir().expect("tempdir");
    let dir = td
        .path()
        .to_str()
        .expect("tempdir path is utf-8")
        .to_string();
    let mut raw = dir.clone().into_bytes();
    raw.extend_from_slice(b"/\xffcli.json");
    let named = OsString::from_vec(raw);

    let out = Command::new(env!("CARGO_BIN_EXE_tensorplate"))
        .env("TENSORPLATE_CLI_CONFIG", &named)
        .args(["logs", "--tail", "1"])
        .output()
        .expect("run tensorplate");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a config named in the environment must be read, not skipped: {stderr}"
    );
    assert!(
        stderr.contains("failed to read cli config") && stderr.contains(&dir),
        "the error must name the file the environment pointed at: {stderr}"
    );
}

#[test]
fn homebrew_cli_config_uses_the_agent_socket_and_structured_log() {
    let p = homebrew_cli_config_path();
    let raw = fs::read_to_string(p)
        .expect("read Homebrew cli config")
        .replace("@HOMEBREW_PREFIX@", "/opt/homebrew");
    let cfg = CliConfig::parse_json(&raw).expect("Homebrew cli config should validate");

    let local = cfg
        .profiles
        .get("local")
        .expect("local profile must be present");
    assert!(matches!(local.mode, ProfileMode::Local));
    assert_eq!(
        local.socket_path.as_deref(),
        Some(std::path::Path::new(
            "/opt/homebrew/var/run/tensorplate/agent.sock"
        ))
    );
    assert_eq!(
        cfg.log_source.path.as_deref(),
        Some(std::path::Path::new(
            "/opt/homebrew/var/log/tensorplate/events.ndjson"
        ))
    );
}
