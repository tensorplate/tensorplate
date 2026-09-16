// SPDX-License-Identifier: Apache-2.0
//
// packaging: install-time doctor probes.
//
// `tensorplate doctor` aggregates these alongside the V01-E11 agent
// probes. They cover the packaging contract from packaging:
//   - filesystem layout (paths, permissions, group ownership)
//   - config files (presence + schema_version sanity)
//   - systemd units (agent + observability present, no serving unit)
//   - serving binary installed under /usr/lib/tensorplate/
//   - CUDA / TensorRT / LibTorch presence (best-effort)
//   - Python/PyTorch backend descriptor + runtime status (packaging)
//
// Every probe returns at least one finding. Probes never mutate state
// and never run user code. Filesystem checks degrade gracefully on
// non-Linux hosts: a missing path on macOS dev hosts becomes a
// `skipped` finding, not a `fail`, so workspace CI still passes.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tensorplate_platform::PlatformRegistry;
use tensorplate_protocol::install_paths::{
    self, AGENT_CONFIG_PATH, BACKEND_DESCRIPTOR_DIR, CLI_CONFIG_PATH, OBSERVABILITY_CONFIG_PATH,
    PLATFORM_REGISTRY_DIR, PYTHON_PYTORCH_BACKEND_DESCRIPTOR, SERVING_BINARY_PATH,
    SERVING_WORKER_CONFIG_PATH,
};

use super::finding::{Finding, FindingId, Severity};

/// Inputs to the install probes. The CLI main builds the default; tests
/// inject a stub `prefix` so the probes run against a tempdir.
#[derive(Clone, Debug)]
pub struct InstallProbeOptions {
    /// Optional path prefix prepended to every absolute install path.
    /// Used by packaging tests to stage `/etc/tensorplate` under a
    /// `TempDir`. Production callers leave this empty.
    pub prefix: Option<PathBuf>,
    /// Whether to run the Python/PyTorch backend probe. Off by default
    /// for the CLI doctor on macOS hosts (no PyTorch wheel); enabled
    /// on real installs via the binary entry point.
    pub probe_backends: bool,
    /// Skip systemd presence checks. CLI doctor sets this on non-Linux
    /// hosts.
    pub skip_systemd: bool,
}

impl Default for InstallProbeOptions {
    fn default() -> Self {
        Self {
            prefix: None,
            probe_backends: cfg!(target_os = "linux")
                || std::env::var_os(install_paths::BACKEND_DESCRIPTOR_DIR_ENV).is_some(),
            skip_systemd: !cfg!(target_os = "linux"),
        }
    }
}

/// Run every install probe and return the aggregated findings.
///
/// When no install layout is present (dev hosts, CI runners without
/// the package installed), every install-specific probe degrades to
/// `missing` / `skipped` — never `fail` — so the dev experience
/// matches the documented contract: doctor passes on a clean dev host
/// and lights up `fail` only when an install is partially broken.
#[must_use]
pub fn run(opts: &InstallProbeOptions) -> Vec<Finding> {
    let mut out = Vec::new();
    let any_install = any_install_present(opts);
    out.extend(probe_core_packages(opts, any_install));
    out.extend(probe_path_layout(opts));
    out.extend(probe_config_files(opts));
    out.extend(probe_config_endpoints(opts));
    out.extend(probe_serving_binary(opts));
    if any_install {
        out.extend(probe_systemd_units(opts));
        out.extend(probe_service_states(opts));
    } else {
        out.extend(skipped_systemd_units(
            "no tensorplate install layout detected",
        ));
        out.extend(skipped_service_states(
            "no tensorplate install layout detected",
        ));
    }
    out.extend(probe_platform_registry(opts));
    out.extend(probe_python_pytorch_backend(opts));
    out.extend(probe_optional_runtimes(opts));
    out
}

/// Report whether the installed platform support registry loads.
///
/// The registry is read-only package data shared by the agent, this CLI,
/// and the observability service, so a corrupt one is a device-wide
/// static-config fault rather than one service's problem. Loading fails
/// closed on a single bad row: reporting `12 of 13 rows` would let a
/// supported machine be told it is unsupported, so the registry either
/// loads whole or not at all and this probe says which.
fn probe_platform_registry(opts: &InstallProbeOptions) -> Vec<Finding> {
    let directory = match platform_registry_path(opts) {
        Ok(directory) => directory,
        Err(err) => {
            return vec![Finding::fail(
                FindingId::PlatformRegistry,
                Severity::Critical,
                format!("platform support registry path is invalid: {err}"),
                Some(format!(
                    "unset {} or set it to an absolute directory",
                    install_paths::PLATFORM_REGISTRY_DIR_ENV
                )),
            )];
        }
    };
    if !directory.exists() {
        return vec![Finding::missing(
            FindingId::PlatformRegistry,
            Severity::Info,
            format!("no platform support registry at `{}`", directory.display()),
            Some(
                "install tensorplate-common (reinstall it if already present) to ship the platform support registry"
                    .into(),
            ),
        )];
    }
    // The registry directory is group-readable, not world-readable, so a
    // caller outside the `tensorplate` group can stat it but not read it.
    // That says nothing about the rows inside, and calling it invalid
    // would be a false claim about healthy data — on a device whose only
    // fault is the account being used. It must also not fail `doctor`,
    // which is the command an operator reaches for to find out why their
    // access is wrong in the first place.
    if let Err(err) = std::fs::read_dir(&directory) {
        if err.kind() == std::io::ErrorKind::PermissionDenied {
            return vec![Finding::warn(
                FindingId::PlatformRegistry,
                Severity::Warning,
                format!(
                    "platform support registry at `{}` is not readable by this account",
                    directory.display()
                ),
                Some(
                    "re-run as root or as a member of the `tensorplate` group; the registry itself was not inspected"
                        .into(),
                ),
            )];
        }
    }
    match PlatformRegistry::load(&directory) {
        Ok(registry) => vec![Finding::ok(
            FindingId::PlatformRegistry,
            Severity::Info,
            format!(
                "platform support registry at `{}` loaded ({} rows, {} supported combinations, {} roadmap targets)",
                directory.display(),
                registry.rows().count(),
                registry.supported_rows().count(),
                registry.roadmap_targets().count(),
            ),
            None,
        )],
        Err(err) => vec![Finding::fail(
            FindingId::PlatformRegistry,
            Severity::Critical,
            format!(
                "platform support registry at `{}` invalid: {err}",
                directory.display()
            ),
            Some("reinstall tensorplate-common; the registry is package data and is not edited on the device".into()),
        )],
    }
}

fn probe_core_packages(opts: &InstallProbeOptions, any_install: bool) -> Vec<Finding> {
    if opts.prefix.is_some() {
        return vec![Finding::skipped(
            FindingId::CorePackages,
            Severity::Info,
            "dpkg package query skipped for a prefixed test install",
            None,
        )];
    }
    if !any_install {
        return vec![Finding::missing(
            FindingId::CorePackages,
            Severity::Info,
            "no tensorplate core package footprint detected",
            Some("install tensorplate-common, -agent, -serving, -observability, and -cli".into()),
        )];
    }
    if !cfg!(target_os = "linux") {
        return vec![Finding::skipped(
            FindingId::CorePackages,
            Severity::Info,
            "dpkg package query is only available on the Linux package target",
            None,
        )];
    }

    let mut installed = Vec::new();
    let mut missing = Vec::new();
    for package in [
        "tensorplate-common",
        "tensorplate-agent",
        "tensorplate-serving",
        "tensorplate-observability",
        "tensorplate-cli",
    ] {
        match query_dpkg_package(package) {
            DpkgPackageState::Installed(version) => installed.push(format!("{package}={version}")),
            DpkgPackageState::Missing => missing.push(package),
            DpkgPackageState::Unavailable(detail) => {
                return vec![Finding::skipped(
                    FindingId::CorePackages,
                    Severity::Info,
                    format!("dpkg package query unavailable: {detail}"),
                    None,
                )];
            }
        }
    }
    if missing.is_empty() {
        vec![Finding::ok(
            FindingId::CorePackages,
            Severity::Info,
            format!("core Debian packages installed: {}", installed.join(", ")),
            None,
        )]
    } else {
        vec![Finding::fail(
            FindingId::CorePackages,
            Severity::Critical,
            format!("missing core Debian packages: {}", missing.join(", ")),
            Some("install the full TensorPlate core package set before starting services".into()),
        )]
    }
}

enum DpkgPackageState {
    Installed(String),
    Missing,
    Unavailable(String),
}

fn query_dpkg_package(package: &str) -> DpkgPackageState {
    let output = match Command::new("dpkg-query")
        .args(["-W", "-f=${db:Status-Abbrev}\t${Version}", package])
        .output()
    {
        Ok(output) => output,
        Err(err) => return DpkgPackageState::Unavailable(err.to_string()),
    };
    if !output.status.success() {
        return DpkgPackageState::Missing;
    }
    let body = String::from_utf8_lossy(&output.stdout);
    let Some((status, version)) = body.trim().split_once('\t') else {
        return DpkgPackageState::Unavailable("unexpected dpkg-query output".into());
    };
    if status.starts_with("ii") {
        DpkgPackageState::Installed(version.to_string())
    } else {
        DpkgPackageState::Missing
    }
}

fn any_install_present(opts: &InstallProbeOptions) -> bool {
    let homebrew_context = cfg!(target_os = "macos");
    let cli_config = if opts.prefix.is_none() && homebrew_context {
        std::env::var_os("TENSORPLATE_CLI_CONFIG")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    } else {
        None
    };
    any_install_present_with_homebrew_paths(opts, homebrew_context, cli_config.as_deref())
}

fn any_install_present_with_homebrew_paths(
    opts: &InstallProbeOptions,
    homebrew_context: bool,
    cli_config: Option<&Path>,
) -> bool {
    // Treat the install layout as present if any of the durable-state
    // directories or installed binaries exists. We avoid a single
    // probe (e.g. /etc/tensorplate) because dpkg conffiles, broken
    // remove/purge cycles, or operator scripts can leave one of those
    // behind on an otherwise-clean host. Homebrew's component formulae
    // install outside these Debian paths, so include the CLI config that
    // its wrapper selects and the package-channel platform-registry path.
    let candidates = [
        tensorplate_protocol::install_paths::ETC_DIR,
        tensorplate_protocol::install_paths::STATE_DIR,
        tensorplate_protocol::install_paths::LOG_DIR,
        SERVING_BINARY_PATH,
        PYTHON_PYTORCH_BACKEND_DESCRIPTOR,
    ];
    candidates.iter().any(|p| prefixed(opts, p).exists())
        || (homebrew_context
            && (cli_config.is_some_and(Path::exists)
                || platform_registry_path(opts).is_ok_and(|path| path.exists())))
        || python_backend_descriptor_path(opts).is_ok_and(|path| path.exists())
}

fn skipped_service_states(reason: &str) -> Vec<Finding> {
    vec![
        Finding::skipped(
            FindingId::AgentServiceState,
            Severity::Info,
            format!("{reason}; agent service-state check skipped"),
            None,
        ),
        Finding::skipped(
            FindingId::ObservabilityServiceState,
            Severity::Info,
            format!("{reason}; observability service-state check skipped"),
            None,
        ),
    ]
}

fn skipped_systemd_units(reason: &str) -> Vec<Finding> {
    vec![
        Finding::skipped(
            FindingId::AgentSystemdUnit,
            Severity::Info,
            format!("{reason}; agent unit check skipped"),
            None,
        ),
        Finding::skipped(
            FindingId::ObservabilitySystemdUnit,
            Severity::Info,
            format!("{reason}; observability unit check skipped"),
            None,
        ),
        Finding::skipped(
            FindingId::ServingSystemdAbsent,
            Severity::Info,
            format!("{reason}; serving-no-unit check skipped"),
            None,
        ),
    ]
}

fn prefixed(opts: &InstallProbeOptions, path: &str) -> PathBuf {
    opts.prefix
        .as_ref()
        .map(|p| {
            let p = p.as_path();
            // Strip leading `/` so PathBuf::join treats it as relative.
            let stripped = path.trim_start_matches('/');
            p.join(stripped)
        })
        .unwrap_or_else(|| PathBuf::from(path))
}

fn platform_registry_path(opts: &InstallProbeOptions) -> Result<PathBuf, String> {
    if opts.prefix.is_some() {
        Ok(prefixed(opts, PLATFORM_REGISTRY_DIR))
    } else {
        install_paths::platform_registry_dir()
    }
}

fn python_backend_descriptor_path(opts: &InstallProbeOptions) -> Result<PathBuf, String> {
    if opts.prefix.is_some() {
        Ok(prefixed(opts, PYTHON_PYTORCH_BACKEND_DESCRIPTOR))
    } else {
        Ok(install_paths::backend_descriptor_dir()?
            .join("python_pytorch")
            .join("backend.json"))
    }
}

fn probe_path_layout(opts: &InstallProbeOptions) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    let mut unsafe_paths: Vec<String> = Vec::new();
    for dir in install_paths::required_directories() {
        let path = prefixed(opts, dir);
        if !path.exists() {
            missing.push((*dir).to_string());
            continue;
        }
        if let Some(detail) = directory_metadata_problem(opts, dir, &path) {
            unsafe_paths.push(detail);
        }
    }
    if !unsafe_paths.is_empty() {
        out.push(Finding::fail(
            FindingId::PathLayout,
            Severity::Critical,
            format!("unsafe install paths detected: {}", unsafe_paths.join(", ")),
            Some(
                "rerun the packaging install-paths.sh helper or reinstall the tensorplate-common package"
                    .into(),
            ),
        ));
        return out;
    }
    if missing.is_empty() {
        out.push(Finding::ok(
            FindingId::PathLayout,
            Severity::Info,
            format!(
                "all {} install directories exist with safe permissions",
                install_paths::required_directories().len()
            ),
            None,
        ));
    } else if missing.len() == install_paths::required_directories().len() {
        // Entire tree absent — likely running on a dev host where the
        // package has never been installed. Surface as `missing` rather
        // than `fail` so the host CI doesn't go red on macOS.
        out.push(Finding::missing(
            FindingId::PathLayout,
            Severity::Info,
            "no tensorplate install layout detected (run apt install tensorplate-common to create it)",
            Some(
                "if this is a Jetson device, install the core packages and re-run tensorplate doctor"
                    .into(),
            ),
        ));
    } else {
        out.push(Finding::fail(
            FindingId::PathLayout,
            Severity::Critical,
            format!("missing install directories: {}", missing.join(", ")),
            Some("reinstall tensorplate-common to recreate the layout".into()),
        ));
    }
    out
}

fn probe_config_files(opts: &InstallProbeOptions) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut missing = Vec::new();
    let mut malformed = Vec::new();
    for cfg_path in install_paths::required_config_files() {
        let p = prefixed(opts, cfg_path);
        if !p.exists() {
            missing.push((*cfg_path).to_string());
            continue;
        }
        match std::fs::read_to_string(&p) {
            Ok(body) => {
                if !has_recognized_schema_version(&body) {
                    malformed.push((*cfg_path).to_string());
                }
                if let Some(detail) = config_metadata_problem(opts, cfg_path, &p) {
                    malformed.push(detail);
                }
            }
            Err(e) => {
                malformed.push(format!("{cfg_path}: {e}"));
            }
        }
    }
    if !malformed.is_empty() {
        findings.push(Finding::fail(
            FindingId::ConfigFiles,
            Severity::Critical,
            format!(
                "config files missing or with unrecognized schema_version: {}",
                malformed.join(", ")
            ),
            Some("compare against config/schemas/*.json or restore from the package".into()),
        ));
    } else if missing.is_empty() {
        findings.push(Finding::ok(
            FindingId::ConfigFiles,
            Severity::Info,
            format!(
                "all {} config files present with recognized schema_version",
                install_paths::required_config_files().len()
            ),
            None,
        ));
    } else if missing.len() == install_paths::required_config_files().len() {
        findings.push(Finding::missing(
            FindingId::ConfigFiles,
            Severity::Info,
            "no /etc/tensorplate config files present (install the tensorplate-* core packages)",
            None,
        ));
    } else {
        findings.push(Finding::fail(
            FindingId::ConfigFiles,
            Severity::Critical,
            format!("missing config files: {}", missing.join(", ")),
            Some(
                "the package owns these as conffiles; reinstall the affected -agent / -observability / -serving / -cli package"
                    .into(),
            ),
        ));
    }
    findings
}

fn probe_config_endpoints(opts: &InstallProbeOptions) -> Vec<Finding> {
    let agent = prefixed(opts, AGENT_CONFIG_PATH);
    let serving = prefixed(opts, SERVING_WORKER_CONFIG_PATH);
    let observability = prefixed(opts, OBSERVABILITY_CONFIG_PATH);
    if [&agent, &serving, &observability]
        .iter()
        .all(|p| !p.exists())
    {
        return vec![Finding::missing(
            FindingId::ConfigEndpoints,
            Severity::Info,
            "no installed service configs available for endpoint-locality checks",
            None,
        )];
    }

    let mut unsafe_configs = Vec::new();
    match read_config_json(&agent) {
        Ok(v) if agent_endpoint_is_local(&v) => {}
        Ok(_) => unsafe_configs.push(format!(
            "{AGENT_CONFIG_PATH}: control transport is not local"
        )),
        Err(err) => unsafe_configs.push(format!("{AGENT_CONFIG_PATH}: {err}")),
    }
    match read_config_json(&serving) {
        Ok(v) if serving_endpoint_is_local(&v) => {}
        Ok(_) => unsafe_configs.push(format!(
            "{SERVING_WORKER_CONFIG_PATH}: bind is not loopback-only"
        )),
        Err(err) => unsafe_configs.push(format!("{SERVING_WORKER_CONFIG_PATH}: {err}")),
    }
    match read_config_json(&observability) {
        Ok(v) if observability_listener_is_local(&v) => {}
        Ok(_) => unsafe_configs.push(format!(
            "{OBSERVABILITY_CONFIG_PATH}: listener is not local"
        )),
        Err(err) => unsafe_configs.push(format!("{OBSERVABILITY_CONFIG_PATH}: {err}")),
    }

    if unsafe_configs.is_empty() {
        vec![Finding::ok(
            FindingId::ConfigEndpoints,
            Severity::Info,
            "installed service configs keep control, serving, and observability endpoints local",
            None,
        )]
    } else {
        vec![Finding::fail(
            FindingId::ConfigEndpoints,
            Severity::Critical,
            format!(
                "non-local or unreadable installed configs: {}",
                unsafe_configs.join(", ")
            ),
            Some("restore the packaged local-only defaults before first start".into()),
        )]
    }
}

fn read_config_json(path: &Path) -> Result<serde_json::Value, String> {
    let body = std::fs::read_to_string(path).map_err(|err| err.to_string())?;
    serde_json::from_str(&body).map_err(|err| err.to_string())
}

fn agent_endpoint_is_local(v: &serde_json::Value) -> bool {
    match v.get("transport").and_then(serde_json::Value::as_str) {
        Some("unix_socket") => v
            .get("socket_path")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|path| path.starts_with("/run/tensorplate/")),
        Some("loopback_tcp") => v
            .get("tcp_bind_host")
            .and_then(serde_json::Value::as_str)
            .is_some_and(is_loopback_host),
        _ => false,
    }
}

fn serving_endpoint_is_local(v: &serde_json::Value) -> bool {
    let bind = v.get("bind");
    bind.and_then(|b| b.get("host"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(is_loopback_host)
        && bind
            .and_then(|b| b.get("allow_non_loopback"))
            .and_then(serde_json::Value::as_bool)
            == Some(false)
}

fn observability_listener_is_local(v: &serde_json::Value) -> bool {
    matches!(
        v.get("listener")
            .and_then(|listener| listener.get("transport"))
            .and_then(serde_json::Value::as_str),
        Some("in_process" | "unix_socket")
    )
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "::1" | "localhost")
}

fn has_recognized_schema_version(body: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    matches!(
        v.get("schema_version").and_then(serde_json::Value::as_str),
        Some(tensorplate_protocol::SCHEMA_VERSION)
    )
}

fn probe_serving_binary(opts: &InstallProbeOptions) -> Vec<Finding> {
    let p = prefixed(opts, SERVING_BINARY_PATH);
    if p.is_file() {
        vec![Finding::ok(
            FindingId::ServingBinaryInstalled,
            Severity::Info,
            format!(
                "serving worker installed at `{SERVING_BINARY_PATH}` (agent-supervised, not systemd-managed)",
            ),
            None,
        )]
    } else {
        vec![Finding::missing(
            FindingId::ServingBinaryInstalled,
            Severity::Info,
            format!("serving worker binary missing at `{SERVING_BINARY_PATH}`"),
            Some(
                "install tensorplate-serving — the agent supervises this binary; there is no separate systemd unit"
                    .into(),
            ),
        )]
    }
}

fn probe_systemd_units(opts: &InstallProbeOptions) -> Vec<Finding> {
    if opts.skip_systemd {
        return vec![
            Finding::skipped(
                FindingId::AgentSystemdUnit,
                Severity::Info,
                "systemd not present on this host; agent unit check skipped",
                None,
            ),
            Finding::skipped(
                FindingId::ObservabilitySystemdUnit,
                Severity::Info,
                "systemd not present on this host; observability unit check skipped",
                None,
            ),
            Finding::skipped(
                FindingId::ServingSystemdAbsent,
                Severity::Info,
                "systemd not present on this host; serving-no-unit check skipped",
                None,
            ),
        ];
    }
    let mut out = Vec::new();
    let unit_dirs = systemd_unit_search_dirs(opts);
    let agent_present = find_unit(&unit_dirs, "tensorplate-agent.service");
    let obs_present = find_unit(&unit_dirs, "tensorplate-observability.service");
    let serving_present = find_unit(&unit_dirs, "tensorplate-serving.service");

    if let Some(p) = agent_present {
        out.push(Finding::ok(
            FindingId::AgentSystemdUnit,
            Severity::Info,
            format!("tensorplate-agent.service installed at `{}`", p.display()),
            None,
        ));
    } else {
        out.push(Finding::fail(
            FindingId::AgentSystemdUnit,
            Severity::Critical,
            "tensorplate-agent.service not found in any systemd unit directory",
            Some("reinstall the tensorplate-agent package".into()),
        ));
    }
    if let Some(p) = obs_present {
        out.push(Finding::ok(
            FindingId::ObservabilitySystemdUnit,
            Severity::Info,
            format!(
                "tensorplate-observability.service installed at `{}`",
                p.display()
            ),
            None,
        ));
    } else {
        out.push(Finding::fail(
            FindingId::ObservabilitySystemdUnit,
            Severity::Critical,
            "tensorplate-observability.service not found in any systemd unit directory",
            Some("reinstall the tensorplate-observability package".into()),
        ));
    }
    if let Some(p) = serving_present {
        // Architecture invariant: there is no v0.1.0 serving systemd
        // unit. If one shows up, fail loudly so operators don't end up
        // racing with the agent's supervisor.
        out.push(Finding::fail(
            FindingId::ServingSystemdAbsent,
            Severity::Critical,
            format!(
                "tensorplate-serving.service unexpectedly present at `{}`",
                p.display()
            ),
            Some(
                "v0.1.0 invariant: the agent supervises the serving worker (V01-E09). Disable and remove this unit."
                    .into(),
            ),
        ));
    } else {
        out.push(Finding::ok(
            FindingId::ServingSystemdAbsent,
            Severity::Info,
            "no tensorplate-serving systemd unit (agent supervises the serving worker)",
            None,
        ));
    }
    out
}

fn probe_service_states(opts: &InstallProbeOptions) -> Vec<Finding> {
    if opts.prefix.is_some() {
        return skipped_service_states("service-state query skipped for a prefixed test install");
    }
    // The supervisor differs by platform and so does the question. On
    // Linux the agent is a systemd unit; on macOS it is a launchd job
    // Homebrew loads. Reporting "systemd not present" on a Mac said
    // nothing about whether the agent was running there, which is the
    // thing an operator is asking.
    if cfg!(target_os = "macos") {
        return probe_launchd_service_states();
    }
    if opts.skip_systemd {
        return skipped_service_states("no supported service supervisor on this host");
    }
    vec![
        probe_service_state(
            FindingId::AgentServiceState,
            "tensorplate-agent.service",
            "start with `systemctl enable --now tensorplate-agent` after install checks pass",
        ),
        probe_service_state(
            FindingId::ObservabilityServiceState,
            "tensorplate-observability.service",
            "start with `systemctl enable --now tensorplate-observability`",
        ),
    ]
}

/// The state column for one service in `brew services list` output.
///
/// Matched on the exact service name rather than a substring:
/// `tensorplate-agent` is a prefix of nothing today, and relying on that
/// staying true is how a future `tensorplate-agent-proxy` would silently
/// answer for the agent.
fn launchd_state_from_listing<'a>(listing: &'a str, service: &str) -> Option<&'a str> {
    listing.lines().find_map(|line| {
        let mut columns = line.split_whitespace();
        (columns.next()? == service)
            .then(|| columns.next())
            .flatten()
    })
}

/// Report both launchd jobs' states from one supervisor snapshot.
///
/// `brew services list` is the supervisor of record on macOS: the jobs are
/// Homebrew-managed, and asking launchd directly would need the label
/// Homebrew chose rather than the service name an operator types. A host
/// without Homebrew is skipped rather than failed -- the CLI runs on
/// machines that never installed the services.
fn probe_launchd_service_states() -> Vec<Finding> {
    probe_launchd_service_states_with(|| {
        Command::new("brew")
            .args(["services", "list"])
            .output()
            .map_err(|err| err.to_string())
    })
}

fn probe_launchd_service_states_with(
    query: impl FnOnce() -> Result<Output, String>,
) -> Vec<Finding> {
    launchd_service_states_from_output(query())
}

fn launchd_service_states_from_output(output: Result<Output, String>) -> Vec<Finding> {
    let output = match output {
        Ok(output) => output,
        Err(err) => {
            return skipped_service_states(&format!(
                "launchd service-state query unavailable: {err}"
            ));
        }
    };
    if !output.status.success() {
        let stderr = bounded_stderr(&output.stderr);
        let detail = if stderr.is_empty() {
            format!("`brew services list` exited with {}", output.status)
        } else {
            format!(
                "`brew services list` exited with {}: {stderr}",
                output.status
            )
        };
        return skipped_service_states(&format!(
            "launchd service-state query unavailable: {detail}"
        ));
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    vec![
        launchd_service_state_from_listing(
            &listing,
            FindingId::AgentServiceState,
            "tensorplate-agent",
            "start with `brew services start tensorplate-agent` after install checks pass",
        ),
        launchd_service_state_from_listing(
            &listing,
            FindingId::ObservabilityServiceState,
            "tensorplate-observability",
            "start with `brew services start tensorplate-observability`",
        ),
    ]
}

fn bounded_stderr(stderr: &[u8]) -> String {
    const MAX_CHARS: usize = 512;

    let detail = String::from_utf8_lossy(stderr);
    let detail = detail.trim();
    let mut chars = detail.chars();
    let mut bounded: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        bounded.push('…');
    }
    bounded
}

fn launchd_service_state_from_listing(
    listing: &str,
    id: FindingId,
    service: &str,
    start_hint: &str,
) -> Finding {
    let Some(state) = launchd_state_from_listing(listing, service) else {
        return Finding::missing(
            id,
            Severity::Info,
            format!("{service} is not a known Homebrew service on this host"),
            Some(start_hint.into()),
        );
    };
    match state {
        "started" => Finding::ok(id, Severity::Info, format!("{service} is started"), None),
        "error" => Finding::fail(
            id,
            Severity::Critical,
            format!("{service} is in error"),
            Some(format!(
                "inspect `$(brew --prefix)/var/log/tensorplate/{}.error.log` before retrying",
                service.trim_start_matches("tensorplate-")
            )),
        ),
        other => Finding::warn(
            id,
            Severity::Warning,
            format!("{service} is {other}"),
            Some(start_hint.into()),
        ),
    }
}

fn probe_service_state(id: FindingId, unit: &str, start_hint: &str) -> Finding {
    let output = match Command::new("systemctl").args(["is-active", unit]).output() {
        Ok(output) => output,
        Err(err) => {
            return Finding::skipped(
                id,
                Severity::Info,
                format!("systemctl service-state query unavailable for {unit}: {err}"),
                None,
            );
        }
    };
    let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
    match state.as_str() {
        "active" => Finding::ok(id, Severity::Info, format!("{unit} is active"), None),
        "failed" => Finding::fail(
            id,
            Severity::Critical,
            format!("{unit} is failed"),
            Some(format!("inspect `journalctl -u {unit}` before retrying")),
        ),
        "inactive" | "activating" | "deactivating" => Finding::warn(
            id,
            Severity::Warning,
            format!("{unit} is {state}"),
            Some(start_hint.into()),
        ),
        other if !other.is_empty() => Finding::warn(
            id,
            Severity::Warning,
            format!("{unit} service state is {other}"),
            Some(start_hint.into()),
        ),
        _ => Finding::skipped(
            id,
            Severity::Info,
            format!("systemctl returned no state for {unit}"),
            None,
        ),
    }
}

fn systemd_unit_search_dirs(opts: &InstallProbeOptions) -> Vec<PathBuf> {
    let candidates = [
        "/lib/systemd/system",
        "/etc/systemd/system",
        "/usr/lib/systemd/system",
    ];
    candidates.iter().map(|c| prefixed(opts, c)).collect()
}

fn find_unit(dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    for d in dirs {
        let p = d.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn probe_python_pytorch_backend(opts: &InstallProbeOptions) -> Vec<Finding> {
    let descriptor = match python_backend_descriptor_path(opts) {
        Ok(descriptor) => descriptor,
        Err(err) => {
            return vec![
                Finding::fail(
                    FindingId::PythonPytorchBackend,
                    Severity::Critical,
                    format!("Python/PyTorch backend descriptor path is invalid: {err}"),
                    Some(format!(
                        "unset {} or set it to an absolute directory",
                        install_paths::BACKEND_DESCRIPTOR_DIR_ENV
                    )),
                ),
                Finding::skipped(
                    FindingId::PythonPytorchRuntime,
                    Severity::Info,
                    "skipped: backend descriptor path invalid",
                    None,
                ),
            ];
        }
    };
    if !descriptor.exists() {
        return vec![
            Finding::missing(
                FindingId::PythonPytorchBackend,
                Severity::Info,
                format!(
                    "no Python/PyTorch backend descriptor at `{}`",
                    descriptor.display()
                ),
                Some(
                    "install tensorplate-backend-python-pytorch to enable python_pytorch bundles (SmolVLA)"
                        .into(),
                ),
            ),
            Finding::skipped(
                FindingId::PythonPytorchRuntime,
                Severity::Info,
                "skipped: backend descriptor absent",
                None,
            ),
        ];
    }
    // Read the descriptor and report a single backend finding + a
    // separate runtime finding so operators can distinguish "backend
    // installed" from "PyTorch importable".
    let parsed =
        tensorplate_protocol::backend_descriptor::BackendDescriptor::read_from(&descriptor);
    let mut out = Vec::new();
    match parsed {
        Ok(d) => {
            out.push(Finding::ok(
                FindingId::PythonPytorchBackend,
                Severity::Info,
                format!(
                    "Python/PyTorch backend descriptor present (package={} version={})",
                    d.package_name, d.package_version
                ),
                None,
            ));
            if opts.probe_backends {
                let report = probe_backend_runtime(&descriptor);
                out.push(runtime_finding(&report));
            } else {
                out.push(Finding::skipped(
                    FindingId::PythonPytorchRuntime,
                    Severity::Info,
                    "backend runtime probe disabled on this host (use `--probe-backends` on a target)",
                    None,
                ));
            }
        }
        Err(err) => {
            out.push(Finding::fail(
                FindingId::PythonPytorchBackend,
                Severity::Critical,
                format!("Python/PyTorch backend descriptor invalid: {err}"),
                Some(
                    "compare against protocol/schemas/backend_descriptor.json or reinstall tensorplate-backend-python-pytorch"
                        .into(),
                ),
            ));
            out.push(Finding::skipped(
                FindingId::PythonPytorchRuntime,
                Severity::Info,
                "skipped: backend descriptor invalid",
                None,
            ));
        }
    }
    out
}

fn probe_backend_runtime(
    descriptor: &Path,
) -> tensorplate_protocol::backend_probe::BackendProbeReport {
    tensorplate_protocol::backend_probe::probe_backend(
        descriptor,
        &tensorplate_protocol::backend_probe::ProbeOptions::default(),
    )
}

fn runtime_finding(report: &tensorplate_protocol::backend_probe::BackendProbeReport) -> Finding {
    use tensorplate_protocol::backend_probe::BackendProbeState as S;
    match &report.state {
        S::Runnable => Finding::ok(
            FindingId::PythonPytorchRuntime,
            Severity::Info,
            "Python/PyTorch runtime is importable and meets the descriptor's minimum versions",
            None,
        ),
        S::DescriptorMissing => Finding::missing(
            FindingId::PythonPytorchRuntime,
            Severity::Info,
            "backend descriptor disappeared between probes",
            None,
        ),
        S::DescriptorMalformed { reason } => Finding::fail(
            FindingId::PythonPytorchRuntime,
            Severity::Critical,
            format!("backend descriptor invalid: {reason}"),
            None,
        ),
        S::RuntimeVersionMismatch {
            runtime_version,
            descriptor_min,
        } => Finding::fail(
            FindingId::PythonPytorchRuntime,
            Severity::Critical,
            format!(
                "tensorplate runtime {runtime_version} below backend descriptor minimum {descriptor_min}"
            ),
            Some("upgrade TensorPlate or install a backend version compatible with this runtime".into()),
        ),
        S::PythonInterpreterMissing { interpreter } => Finding::fail(
            FindingId::PythonPytorchRuntime,
            Severity::Critical,
            format!("Python interpreter `{interpreter}` is missing"),
            Some("install Python 3.10+ or edit the descriptor's python.interpreter".into()),
        ),
        S::PythonVersionMismatch {
            interpreter,
            observed,
            required,
        } => Finding::warn(
            FindingId::PythonPytorchRuntime,
            Severity::Warning,
            format!(
                "Python interpreter `{interpreter}` is {observed}; descriptor requires {required}"
            ),
            Some("upgrade Python in the descriptor's interpreter".into()),
        ),
        S::PythonModuleImportFailed { module, detail } => Finding::fail(
            FindingId::PythonPytorchRuntime,
            Severity::Critical,
            format!("Python module `{module}` failed to import: {detail}"),
            report.install_hint.clone(),
        ),
        S::PytorchMissing { detail } => Finding::fail(
            FindingId::PythonPytorchRuntime,
            Severity::Critical,
            format!("PyTorch is not importable: {detail}"),
            report.install_hint.clone().or_else(|| {
                Some("see docs/install/python-pytorch-backend.md for PyTorch install instructions".into())
            }),
        ),
        S::PytorchVersionMismatch { observed, required } => Finding::warn(
            FindingId::PythonPytorchRuntime,
            Severity::Warning,
            format!("PyTorch {observed} below descriptor minimum {required}"),
            Some("upgrade PyTorch in the descriptor's interpreter".into()),
        ),
    }
}

/// Files the NVIDIA **driver** installs.
///
/// `libcuda` is the driver's own user-mode library: the driver package
/// ships it, the CUDA toolkit does not, and a host can have it with no
/// toolkit anywhere. `/proc/driver/nvidia/version` is the file
/// `packaging/scripts/install.sh` reads *by default* for its x86_64
/// hardware check. The two checks are not equivalent and must not be
/// described as one: the installer tests that file for readability,
/// honours a `TP_INSTALL_NVIDIA_VERSION` override, and falls back to a
/// successful `nvidia-smi` driver query, none of which this list does.
/// So the installer can report a driver doctor does not find here --
/// in practice `libcuda.so.1` below usually closes that gap.
///
/// `/proc/driver/nvidia/version` is an x86_64 answer: it comes from
/// `nvidia.ko`, and L4T drives the Tegra GPU through `nvgpu` instead,
/// so a Jetson is recognized by its `libcuda` alone. That is why
/// `NVIDIA_DRIVER_LIB_DIRS` below is scanned as well as these exact
/// names -- an L4T layout that ships only `libcuda.so.1.1`, or a host
/// whose `libcuda.so.1` symlink ldconfig has not written, would
/// otherwise read as driverless.
const NVIDIA_DRIVER_PATHS: &[&str] = &[
    "/proc/driver/nvidia/version",
    "/usr/lib/x86_64-linux-gnu/libcuda.so.1",
    "/usr/lib/aarch64-linux-gnu/libcuda.so.1",
    // L4T keeps the Tegra driver libraries in their own directory and
    // adds it to the loader path with an ld.so.conf.d entry.
    "/usr/lib/aarch64-linux-gnu/nvidia/libcuda.so",
    "/usr/lib/aarch64-linux-gnu/nvidia/libcuda.so.1",
    "/usr/lib/aarch64-linux-gnu/tegra/libcuda.so",
    "/usr/lib/aarch64-linux-gnu/tegra/libcuda.so.1",
];

/// Directories searched for a versioned `libcuda.so.<soname>` when none
/// of the exact driver names above is present. The prefix cannot match
/// `libcudart.so.*`: the CUDA runtime library's name diverges at the
/// `r`, before the `.` this prefix requires.
const NVIDIA_DRIVER_LIB_DIRS: &[&str] = &[
    "/usr/lib/aarch64-linux-gnu/tegra",
    "/usr/lib/aarch64-linux-gnu/nvidia",
    "/usr/lib/aarch64-linux-gnu",
    "/usr/lib/x86_64-linux-gnu",
];

/// The versioned NVIDIA driver library name, without its soname.
const NVIDIA_DRIVER_SONAME_PREFIX: &str = "libcuda.so.";

/// Files a **system CUDA toolkit** installs. `libcudart` is the CUDA
/// runtime library the toolkit ships, which is what a TensorRT-linked
/// worker needs and what the driver alone does not provide.
const CUDA_TOOLKIT_PATHS: &[&str] = &[
    "/usr/local/cuda/version.txt",
    "/usr/local/cuda/version.json",
    "/usr/local/cuda/lib64/libcudart.so",
    "/usr/local/cuda/targets/aarch64-linux/lib/libcudart.so",
    "/usr/local/cuda/targets/x86_64-linux/lib/libcudart.so",
    "/usr/lib/x86_64-linux-gnu/libcudart.so",
    "/usr/lib/aarch64-linux-gnu/libcudart.so",
];

/// Directories searched for a versioned `libcudart.so.<soname>`. Every
/// unversioned name above is a symlink from the toolkit's *development*
/// package, so a host carrying only the runtime package has the library
/// under none of them.
const CUDA_TOOLKIT_LIB_DIRS: &[&str] = &[
    "/usr/local/cuda/lib64",
    "/usr/local/cuda/targets/aarch64-linux/lib",
    "/usr/local/cuda/targets/x86_64-linux/lib",
    "/usr/lib/x86_64-linux-gnu",
    "/usr/lib/aarch64-linux-gnu",
];

/// The versioned CUDA runtime library name, without its soname.
const CUDA_RUNTIME_SONAME_PREFIX: &str = "libcudart.so.";

/// What the serving build installed next to this CLI needs from a
/// **system** CUDA toolkit.
///
/// The answer is a property of the build, not of the host: the same
/// GPU machine is fine without a toolkit under one build and cannot
/// load its accelerator path under another.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServingCudaNeed {
    /// The arm64 (Jetson) package. `tools/release/build-release-artifacts.sh`
    /// configures it with `TP_ENABLE_TENSORRT=ON` and
    /// `TP_REQUIRE_TENSORRT_SDK=ON`, so the worker carries the TensorRT
    /// adapter and links the CUDA runtime the toolkit ships.
    TensorrtLinked,
    /// The amd64 package. `tools/release/amd64-build-profile.sh` configures
    /// it with `TP_ENABLE_TENSORRT=OFF`, so the worker has no TensorRT
    /// adapter at all; its accelerator path is the python_pytorch
    /// sidecar, whose CUDA build of PyTorch carries its own CUDA runtime
    /// libraries inside the wheel.
    SidecarRuntime,
    /// No build shipped for this platform has a CUDA path: the Homebrew
    /// formula also builds `TP_ENABLE_TENSORRT=OFF`, and the macOS
    /// accelerator is Metal.
    NoCudaPath,
}

/// Which serving build is installed, in the only terms packaging
/// guarantees.
///
/// This deliberately reads the CLI's own build target rather than the
/// detected host: the question is which *package* is installed, and the
/// CLI answering it was installed from the same per-architecture
/// artifact set as the serving worker. `host_facts` answers the other
/// question — what the machine is — and must not be derived this way.
const fn installed_serving_cuda_need() -> ServingCudaNeed {
    if !cfg!(target_os = "linux") {
        ServingCudaNeed::NoCudaPath
    } else if cfg!(target_arch = "aarch64") {
        ServingCudaNeed::TensorrtLinked
    } else if cfg!(target_arch = "x86_64") {
        ServingCudaNeed::SidecarRuntime
    } else {
        ServingCudaNeed::NoCudaPath
    }
}

/// Which CUDA-related artifacts are on disk, and where. Paths only: a
/// name here means the file exists, never that anything works.
#[derive(Clone, Debug, Default)]
struct CudaArtifacts {
    driver: Option<String>,
    toolkit: Option<String>,
}

fn probe_cuda_artifacts(opts: &InstallProbeOptions) -> CudaArtifacts {
    CudaArtifacts {
        driver: first_existing_artifact(opts, NVIDIA_DRIVER_PATHS).or_else(|| {
            first_versioned_library(opts, NVIDIA_DRIVER_LIB_DIRS, NVIDIA_DRIVER_SONAME_PREFIX)
        }),
        toolkit: first_existing_artifact(opts, CUDA_TOOLKIT_PATHS).or_else(|| {
            first_versioned_library(opts, CUDA_TOOLKIT_LIB_DIRS, CUDA_RUNTIME_SONAME_PREFIX)
        }),
    }
}

/// Which CUDA-consuming components are actually installed here.
///
/// The verdict below is about what the *installed* software needs, and
/// software that is not installed needs nothing. `--cli-only` is a
/// documented install mode (`docs/install/external-install.md`) that
/// brings no serving worker, and the python_pytorch sidecar is an
/// opt-in package, so neither may be assumed present. Without this,
/// doctor states requirements of components that are not on the
/// machine — an install hint for a worker that was never installed, or
/// an `ok` justified by a sidecar the same report lists as `missing`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct InstalledCudaConsumers {
    serving_worker: bool,
    python_pytorch_backend: bool,
}

fn probe_installed_cuda_consumers(opts: &InstallProbeOptions) -> InstalledCudaConsumers {
    InstalledCudaConsumers {
        serving_worker: prefixed(opts, SERVING_BINARY_PATH).is_file(),
        // Resolved the same way `python_pytorch_backend` resolves it, so
        // the two findings cannot disagree about the same descriptor.
        python_pytorch_backend: python_backend_descriptor_path(opts)
            .is_ok_and(|descriptor| descriptor.exists()),
    }
}

/// The hint every no-driver verdict carries: doctor reads paths, and the
/// question "does this host have an accelerator at all" belongs to the
/// findings that answer it.
const NO_DRIVER_HINT: &str =
    "`accelerator_facts` and `platform_row` report whether this host has an accelerator at all";

/// The hint every verdict that rests on the sidecar carries: the wheel's
/// own CUDA runtime is not read here, so a `ok` is a statement about
/// paths and packages, never about a working device.
const PATHS_ONLY_HINT: &str = "paths only: whether that PyTorch can reach the accelerator is not established here — see `python_pytorch_runtime` and `accelerator_facts`";

/// Report the CUDA runtime state of this host against what the
/// installed build needs from it.
///
/// Four facts are kept apart because they fail apart: whether the NVIDIA
/// driver is installed, whether a system CUDA toolkit is, whether the
/// installed serving build needs the second one, and whether the
/// components that would consume either are installed at all. A host
/// with a driver and no toolkit is a defect under a TensorRT-linked
/// build and entirely normal under the amd64 build, and one message
/// cannot be true of both.
///
/// The driver governs the status on its own, independently of the
/// toolkit: `libcuda` comes from the driver package and every CUDA
/// consumer loads through it, so an unrelated system toolkit on a
/// driverless host must not turn a defect into an `ok`.
fn cuda_runtime_finding(
    need: ServingCudaNeed,
    installed: InstalledCudaConsumers,
    found: &CudaArtifacts,
) -> Finding {
    let driver = match &found.driver {
        Some(path) => format!("NVIDIA driver present (`{path}`)"),
        None => "no NVIDIA driver at the known paths".to_string(),
    };
    let toolkit = match &found.toolkit {
        Some(path) => format!("system CUDA toolkit at `{path}`"),
        None => "no system CUDA toolkit at the known paths".to_string(),
    };
    let no_driver = "with no driver, no installed component can reach an NVIDIA accelerator";
    match need {
        ServingCudaNeed::NoCudaPath => Finding::skipped(
            FindingId::CudaRuntime,
            Severity::Info,
            "skipped: the serving build shipped for this platform has no CUDA path, so no CUDA runtime is required",
            None,
        ),
        _ if !installed.serving_worker && !installed.python_pytorch_backend => Finding::skipped(
            FindingId::CudaRuntime,
            Severity::Info,
            format!("skipped: no serving worker at `{SERVING_BINARY_PATH}` and no python_pytorch sidecar, so nothing installed here consumes a CUDA runtime"),
            Some(
                "`serving_binary_installed` and `python_pytorch_backend` report which of the two is installed"
                    .into(),
            ),
        ),
        // The build decides what the *worker* needs, and the worker is
        // not here. `tensorplate-backend-python-pytorch` only
        // `Recommends` the serving package, so the sidecar can be
        // installed alone; it is then the one CUDA consumer on the
        // host, and its runtime comes from its own wheel on every
        // build. Asking such a host for the toolkit an absent
        // TensorRT-linked worker would have wanted states a
        // requirement of software that is not installed.
        _ if !installed.serving_worker => {
            let consumer = "no serving worker is installed (see `serving_binary_installed`), so the installed python_pytorch sidecar is the only CUDA consumer here";
            if found.driver.is_none() {
                return Finding::missing(
                    FindingId::CudaRuntime,
                    Severity::Info,
                    format!("{driver}; {toolkit}; {consumer} — but {no_driver}"),
                    Some(NO_DRIVER_HINT.into()),
                );
            }
            Finding::ok(
                FindingId::CudaRuntime,
                Severity::Info,
                format!(
                    "{driver}; {toolkit}; {consumer}, and a CUDA build of PyTorch ships its own CUDA runtime in the wheel"
                ),
                Some(PATHS_ONLY_HINT.into()),
            )
        }
        ServingCudaNeed::TensorrtLinked => {
            let adapter = if found.toolkit.is_some() {
                "this build's serving worker links its TensorRT adapter against that runtime"
            } else {
                "this build's serving worker carries the TensorRT adapter, which cannot load without the CUDA runtime"
            };
            match (found.driver.is_some(), found.toolkit.is_some()) {
                (true, true) => Finding::ok(
                    FindingId::CudaRuntime,
                    Severity::Info,
                    format!("{driver}; {toolkit}; {adapter}"),
                    None,
                ),
                (true, false) => Finding::warn(
                    FindingId::CudaRuntime,
                    Severity::Warning,
                    format!("{driver}; {toolkit}; {adapter}"),
                    Some(
                        "install the JetPack CUDA runtime so `/usr/local/cuda/targets/aarch64-linux/lib/libcudart.so*` is present"
                            .into(),
                    ),
                ),
                (false, _) => Finding::warn(
                    FindingId::CudaRuntime,
                    Severity::Warning,
                    format!("{driver}; {toolkit}; {adapter}, and {no_driver}"),
                    Some(NO_DRIVER_HINT.into()),
                ),
            }
        }
        ServingCudaNeed::SidecarRuntime => {
            let toolkit_need = if found.toolkit.is_some() {
                "this build's serving worker carries no TensorRT adapter, so nothing installed requires that toolkit"
            } else {
                "this build's serving worker carries no TensorRT adapter, so no system toolkit is required here"
            };
            if found.driver.is_none() {
                return Finding::missing(
                    FindingId::CudaRuntime,
                    Severity::Info,
                    format!("{driver}; {toolkit}; {toolkit_need} — but {no_driver}"),
                    Some(NO_DRIVER_HINT.into()),
                );
            }
            // The accelerator path of this build is the sidecar, so the
            // sentence about the sidecar's wheel is only a statement
            // about this host when the sidecar is on it.
            let sidecar = if installed.python_pytorch_backend {
                "the installed python_pytorch sidecar reaches the accelerator, and a CUDA build of PyTorch ships its own CUDA runtime in the wheel"
            } else {
                "no python_pytorch sidecar is installed (see `python_pytorch_backend`); if one is installed, a CUDA build of PyTorch ships its own CUDA runtime in the wheel"
            };
            Finding::ok(
                FindingId::CudaRuntime,
                Severity::Info,
                format!("{driver}; {toolkit}; {toolkit_need} — {sidecar}"),
                Some(PATHS_ONLY_HINT.into()),
            )
        }
    }
}

fn probe_optional_runtimes(opts: &InstallProbeOptions) -> Vec<Finding> {
    // CUDA / TensorRT / LibTorch live outside the package manifest. We
    // probe well-known absolute paths but never run vendor SDK
    // binaries and never import the sidecar's PyTorch: a positive
    // result means "the file is on disk", not "this runtime works".
    // Real validation belongs to release validation.
    let cuda = cuda_runtime_finding(
        installed_serving_cuda_need(),
        probe_installed_cuda_consumers(opts),
        &probe_cuda_artifacts(opts),
    );
    let tensorrt = any_runtime_artifact_exists(
        opts,
        &[
            "/usr/include/NvInferVersion.h",
            "/usr/lib/x86_64-linux-gnu/libnvinfer.so",
            "/usr/lib/aarch64-linux-gnu/libnvinfer.so",
        ],
    );
    let libtorch = any_runtime_artifact_exists(
        opts,
        &[
            "/usr/local/libtorch",
            "/opt/libtorch",
            "/usr/lib/libtorch.so",
        ],
    );

    vec![
        cuda,
        runtime_finding_simple(
            FindingId::TensorrtRuntime,
            tensorrt,
            "TensorRT artifact detected",
            "TensorRT not detected; vision-on-TensorRT validation will skip",
        ),
        runtime_finding_simple(
            FindingId::LibtorchRuntime,
            libtorch,
            "LibTorch artifact detected",
            "LibTorch not detected; the LibTorch native adapter is optional in v0.1.0",
        ),
    ]
}

fn runtime_finding_simple(id: FindingId, present: bool, ok_msg: &str, miss_msg: &str) -> Finding {
    if present {
        Finding::ok(id, Severity::Info, ok_msg.to_string(), None)
    } else {
        Finding::missing(id, Severity::Info, miss_msg.to_string(), None)
    }
}

fn path_exists(p: &str) -> bool {
    Path::new(p).exists()
}

/// The first of `paths` that exists, reported as the contract path it
/// was looked for under — never the staging prefix a test injected.
fn first_existing_artifact(opts: &InstallProbeOptions, paths: &[&str]) -> Option<String> {
    paths
        .iter()
        .find(|path| prefixed(opts, path).exists())
        .map(|path| (*path).to_string())
}

/// The first versioned library named `<prefix><soname>` in `dirs`.
///
/// `read_dir` order is unspecified, so the names are sorted: one host
/// must report the same path on every run, or the evidence a validation
/// run files would differ from the one before it for no reason.
fn first_versioned_library(
    opts: &InstallProbeOptions,
    dirs: &[&str],
    prefix: &str,
) -> Option<String> {
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(prefixed(opts, dir)) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name.len() > prefix.len() && name.starts_with(prefix))
            .collect();
        names.sort();
        if let Some(name) = names.first() {
            return Some(format!("{dir}/{name}"));
        }
    }
    None
}

fn any_runtime_artifact_exists(opts: &InstallProbeOptions, paths: &[&str]) -> bool {
    paths.iter().any(|path| {
        opts.prefix.as_ref().map_or_else(
            || path_exists(path),
            |prefix| prefix.join(path.trim_start_matches('/')).exists(),
        )
    })
}

#[cfg(unix)]
fn path_mode(p: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).map(|m| m.mode() & 0o7777).ok()
}

#[cfg(not(unix))]
fn path_mode(_p: &Path) -> Option<u32> {
    None
}

fn directory_metadata_problem(
    opts: &InstallProbeOptions,
    contract_path: &str,
    path: &Path,
) -> Option<String> {
    let mode = path_mode(path)?;
    let expected = install_paths::expected_dir_mode(contract_path);
    if mode != expected {
        return Some(format!(
            "{contract_path} mode={mode:#05o} expected={expected:#05o}"
        ));
    }
    ownership_problem(opts, contract_path, path)
}

fn config_metadata_problem(
    opts: &InstallProbeOptions,
    contract_path: &str,
    path: &Path,
) -> Option<String> {
    let expected_mode = if contract_path == CLI_CONFIG_PATH {
        install_paths::mode::FILE_0644
    } else {
        install_paths::mode::FILE_0640
    };
    let mode = path_mode(path)?;
    if mode != expected_mode {
        return Some(format!(
            "{contract_path} mode={mode:#05o} expected={expected_mode:#05o}"
        ));
    }
    ownership_problem(opts, contract_path, path)
}

#[cfg(unix)]
fn ownership_problem(
    opts: &InstallProbeOptions,
    contract_path: &str,
    path: &Path,
) -> Option<String> {
    use std::os::unix::fs::MetadataExt;

    if opts.prefix.is_some() || !cfg!(target_os = "linux") {
        return None;
    }
    let metadata = std::fs::metadata(path).ok()?;
    let Some(expected_group) = lookup_unix_id("group", install_paths::SYSTEM_GROUP) else {
        return Some(format!(
            "{contract_path} cannot validate ownership because group `{}` is missing",
            install_paths::SYSTEM_GROUP
        ));
    };
    if metadata.gid() != expected_group {
        return Some(format!(
            "{contract_path} gid={} expected={expected_group}",
            metadata.gid()
        ));
    }
    let service_owned = contract_path.starts_with(install_paths::STATE_DIR)
        || contract_path.starts_with(install_paths::LOG_DIR)
        || contract_path.starts_with(install_paths::RUN_DIR);
    let expected_uid = if service_owned {
        let Some(uid) = lookup_unix_id("passwd", install_paths::SYSTEM_USER) else {
            return Some(format!(
                "{contract_path} cannot validate ownership because user `{}` is missing",
                install_paths::SYSTEM_USER
            ));
        };
        uid
    } else {
        0
    };
    (metadata.uid() != expected_uid).then(|| {
        format!(
            "{contract_path} uid={} expected={expected_uid}",
            metadata.uid()
        )
    })
}

#[cfg(not(unix))]
fn ownership_problem(
    _opts: &InstallProbeOptions,
    _contract_path: &str,
    _path: &Path,
) -> Option<String> {
    None
}

fn lookup_unix_id(database: &str, name: &str) -> Option<u32> {
    let output = Command::new("getent")
        .args([database, name])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .split(':')
        .nth(2)?
        .parse()
        .ok()
}

#[allow(dead_code)]
fn _suppress_unused() {
    // Keep the path constants used by future doctor extensions
    // referenced so a refactor doesn't accidentally drop them.
    let _ = (
        AGENT_CONFIG_PATH,
        OBSERVABILITY_CONFIG_PATH,
        SERVING_WORKER_CONFIG_PATH,
        CLI_CONFIG_PATH,
        BACKEND_DESCRIPTOR_DIR,
    );
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::cell::Cell;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;
    use tempfile::TempDir;

    /// Real `brew services list` output shape: a header row, then one row
    /// per service, columns separated by runs of spaces.
    const BREW_LISTING: &str = "\
Name                      Status  User       File
tensorplate-agent         started operator ~/Library/LaunchAgents/homebrew.mxcl.tensorplate-agent.plist
tensorplate-observability none
other-service             error   root       ~/Library/LaunchAgents/other.plist
";

    #[test]
    fn a_launchd_listing_is_read_by_exact_service_name() {
        assert_eq!(
            launchd_state_from_listing(BREW_LISTING, "tensorplate-agent"),
            Some("started")
        );
        // A service Homebrew knows but has never started reads `none`,
        // which is a state and not an absence -- the probe warns with the
        // start hint rather than reporting the service unknown.
        assert_eq!(
            launchd_state_from_listing(BREW_LISTING, "tensorplate-observability"),
            Some("none")
        );
        // Another service's state must never answer for ours.
        assert_eq!(
            launchd_state_from_listing(BREW_LISTING, "tensorplate-serving"),
            None
        );
    }

    #[test]
    fn a_launchd_name_is_matched_whole_not_as_a_prefix() {
        // `tensorplate-agent` is a prefix of nothing today. Relying on
        // that staying true is how a future sibling service would
        // silently answer for the agent.
        let listing = "\
Name                       Status  User       File
tensorplate-agent-proxy    started operator ~/Library/LaunchAgents/proxy.plist
";
        assert_eq!(
            launchd_state_from_listing(listing, "tensorplate-agent"),
            None,
            "a longer name that starts the same must not answer"
        );
    }

    #[test]
    fn a_homebrew_cli_config_is_an_install_footprint_without_the_optional_backend() {
        let td = TempDir::new().unwrap();
        let cli_config = td.path().join("opt/homebrew/etc/tensorplate/cli.json");
        fs::create_dir_all(cli_config.parent().unwrap()).unwrap();
        fs::write(&cli_config, b"{}\n").unwrap();
        let opts = InstallProbeOptions {
            prefix: Some(td.path().to_path_buf()),
            probe_backends: false,
            skip_systemd: true,
        };

        assert!(
            !python_backend_descriptor_path(&opts).unwrap().exists(),
            "the optional Python backend must not make this test pass"
        );
        assert!(any_install_present_with_homebrew_paths(
            &opts,
            true,
            Some(&cli_config)
        ));
    }

    #[test]
    fn non_homebrew_context_ignores_homebrew_only_install_footprints() {
        let td = TempDir::new().unwrap();
        let opts = InstallProbeOptions {
            prefix: Some(td.path().to_path_buf()),
            probe_backends: false,
            skip_systemd: false,
        };
        let cli_config = td.path().join("opt/homebrew/etc/tensorplate/cli.json");
        fs::create_dir_all(cli_config.parent().unwrap()).unwrap();
        fs::write(&cli_config, b"{}\n").unwrap();
        fs::create_dir_all(platform_registry_path(&opts).unwrap()).unwrap();

        assert!(
            !any_install_present_with_homebrew_paths(&opts, false, Some(&cli_config)),
            "a CLI config or registry override must not imply a partial Debian install"
        );
    }

    #[test]
    fn launchd_service_states_share_one_successful_brew_query() {
        let calls = Cell::new(0);
        let findings = probe_launchd_service_states_with(|| {
            calls.set(calls.get() + 1);
            Ok(Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: BREW_LISTING.as_bytes().to_vec(),
                stderr: Vec::new(),
            })
        });

        assert_eq!(calls.get(), 1);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].id, FindingId::AgentServiceState);
        assert_eq!(findings[0].status_label(), "ok");
        assert_eq!(findings[1].id, FindingId::ObservabilityServiceState);
        assert_eq!(findings[1].status_label(), "warning");
    }

    #[test]
    fn an_unsuccessful_brew_query_skips_both_services_with_bounded_stderr() {
        let stderr = format!("brew supervisor failure {}", "x".repeat(1_024));
        let findings = launchd_service_states_from_output(Ok(Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }));

        assert_eq!(findings.len(), 2);
        assert!(findings.iter().all(|finding| {
            finding.status_label() == "skipped"
                && finding.message.contains("brew supervisor failure")
                && finding.message.contains('…')
        }));
        assert_eq!(bounded_stderr(stderr.as_bytes()).chars().count(), 513);
    }

    fn stage_install_layout(td: &Path) {
        for dir in install_paths::required_directories() {
            let p = td.join(dir.trim_start_matches('/'));
            fs::create_dir_all(&p).unwrap();
            fs::set_permissions(
                &p,
                fs::Permissions::from_mode(install_paths::expected_dir_mode(dir)),
            )
            .unwrap();
        }
        // Drop the canonical conffile bodies so the schema_version
        // check passes.
        let conf = |path: &str, body: &str| {
            let p = td.join(path.trim_start_matches('/'));
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, body).unwrap();
            let mode = if path == CLI_CONFIG_PATH {
                0o0644
            } else {
                0o0640
            };
            fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
        };
        conf(
            AGENT_CONFIG_PATH,
            include_str!("../../../../packaging/conf/agent.json"),
        );
        conf(
            OBSERVABILITY_CONFIG_PATH,
            include_str!("../../../../packaging/conf/observability.json"),
        );
        conf(
            SERVING_WORKER_CONFIG_PATH,
            include_str!("../../../../packaging/conf/serving_worker.json"),
        );
        conf(
            CLI_CONFIG_PATH,
            include_str!("../../../../packaging/conf/cli.json"),
        );
    }

    fn stage_serving_binary(td: &Path) {
        let p = td.join(SERVING_BINARY_PATH.trim_start_matches('/'));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o0755)).unwrap();
    }

    fn stage_systemd_units(td: &Path) {
        let dir = td.join("lib/systemd/system");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("tensorplate-agent.service"), b"[Unit]\n").unwrap();
        fs::write(dir.join("tensorplate-observability.service"), b"[Unit]\n").unwrap();
    }

    #[test]
    fn happy_install_reports_only_ok_findings() {
        let td = TempDir::new().unwrap();
        stage_install_layout(td.path());
        stage_serving_binary(td.path());
        stage_systemd_units(td.path());
        let opts = InstallProbeOptions {
            prefix: Some(td.path().to_path_buf()),
            probe_backends: false,
            skip_systemd: false,
        };
        let findings = run(&opts);
        let path_layout = findings
            .iter()
            .find(|f| matches!(f.id, FindingId::PathLayout))
            .unwrap();
        assert_eq!(path_layout.status_label(), "ok");
        let config = findings
            .iter()
            .find(|f| matches!(f.id, FindingId::ConfigFiles))
            .unwrap();
        assert_eq!(config.status_label(), "ok");
        let endpoints = findings
            .iter()
            .find(|f| matches!(f.id, FindingId::ConfigEndpoints))
            .unwrap();
        assert_eq!(endpoints.status_label(), "ok");
        let serving = findings
            .iter()
            .find(|f| matches!(f.id, FindingId::ServingBinaryInstalled))
            .unwrap();
        assert_eq!(serving.status_label(), "ok");
        let agent_unit = findings
            .iter()
            .find(|f| matches!(f.id, FindingId::AgentSystemdUnit))
            .unwrap();
        assert_eq!(agent_unit.status_label(), "ok");
        let serving_absent = findings
            .iter()
            .find(|f| matches!(f.id, FindingId::ServingSystemdAbsent))
            .unwrap();
        assert_eq!(serving_absent.status_label(), "ok");
    }

    #[test]
    fn rogue_serving_unit_is_a_fail() {
        let td = TempDir::new().unwrap();
        stage_install_layout(td.path());
        stage_serving_binary(td.path());
        stage_systemd_units(td.path());
        let dir = td.path().join("lib/systemd/system");
        fs::write(dir.join("tensorplate-serving.service"), b"[Unit]\n").unwrap();
        let opts = InstallProbeOptions {
            prefix: Some(td.path().to_path_buf()),
            probe_backends: false,
            skip_systemd: false,
        };
        let findings = run(&opts);
        let serving_absent = findings
            .iter()
            .find(|f| matches!(f.id, FindingId::ServingSystemdAbsent))
            .unwrap();
        assert_eq!(serving_absent.status_label(), "fail");
    }

    #[test]
    fn missing_backend_is_actionable() {
        let td = TempDir::new().unwrap();
        stage_install_layout(td.path());
        let opts = InstallProbeOptions {
            prefix: Some(td.path().to_path_buf()),
            probe_backends: false,
            skip_systemd: true,
        };
        let findings = run(&opts);
        let backend = findings
            .iter()
            .find(|f| matches!(f.id, FindingId::PythonPytorchBackend))
            .unwrap();
        assert_eq!(backend.status_label(), "missing");
        assert!(backend.hint.is_some());
    }

    #[test]
    fn world_writable_path_is_a_fail() {
        let td = TempDir::new().unwrap();
        stage_install_layout(td.path());
        let weakened = td.path().join("var/lib/tensorplate");
        fs::set_permissions(&weakened, fs::Permissions::from_mode(0o0757)).unwrap();
        let opts = InstallProbeOptions {
            prefix: Some(td.path().to_path_buf()),
            probe_backends: false,
            skip_systemd: true,
        };
        let findings = run(&opts);
        let layout = findings
            .iter()
            .find(|f| matches!(f.id, FindingId::PathLayout))
            .unwrap();
        assert_eq!(layout.status_label(), "fail");
    }

    #[test]
    fn no_install_layout_is_missing_not_fail() {
        let td = TempDir::new().unwrap();
        let opts = InstallProbeOptions {
            prefix: Some(td.path().to_path_buf()),
            probe_backends: false,
            skip_systemd: true,
        };
        let findings = run(&opts);
        let layout = findings
            .iter()
            .find(|f| matches!(f.id, FindingId::PathLayout))
            .unwrap();
        assert_eq!(layout.status_label(), "missing");
    }

    fn stage_file(td: &Path, contract_path: &str) {
        let p = td.join(contract_path.trim_start_matches('/'));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, b"").unwrap();
    }

    fn cuda_opts(td: &Path) -> InstallProbeOptions {
        InstallProbeOptions {
            prefix: Some(td.to_path_buf()),
            probe_backends: false,
            skip_systemd: true,
        }
    }

    /// The finding on a host that has the serving worker installed.
    /// Every case below is about such a host; the gate that skips the
    /// finding when the worker is absent has its own tests.
    fn cuda_finding(td: &Path, need: ServingCudaNeed) -> Finding {
        stage_serving_binary(td);
        let opts = cuda_opts(td);
        cuda_runtime_finding(
            need,
            probe_installed_cuda_consumers(&opts),
            &probe_cuda_artifacts(&opts),
        )
    }

    /// Every way the two CUDA-consuming components can be installed.
    /// Listed rather than sampled: the sidecar without the worker is a
    /// real apt state (`tensorplate-backend-python-pytorch` only
    /// `Recommends` the serving package) and reaches its own arm.
    fn every_installed_combination() -> Vec<InstalledCudaConsumers> {
        let mut out = Vec::new();
        for serving_worker in [false, true] {
            for python_pytorch_backend in [false, true] {
                out.push(InstalledCudaConsumers {
                    serving_worker,
                    python_pytorch_backend,
                });
            }
        }
        out
    }

    /// The staging prefix is a test fixture, not a fact about the host.
    /// Jetson evidence copies these messages verbatim, so a leaked local
    /// path would be published into a validation record.
    fn assert_no_staging_prefix(td: &Path, cuda: &Finding) {
        let prefix = td.display().to_string();
        assert!(
            !cuda.message.contains(&prefix),
            "the finding must report the contract path, not the staging prefix: {}",
            cuda.message
        );
    }

    #[test]
    fn jetpack_cuda_runtime_layout_is_detected() {
        let td = TempDir::new().unwrap();
        stage_file(td.path(), "/usr/lib/aarch64-linux-gnu/nvidia/libcuda.so");
        stage_file(
            td.path(),
            "/usr/local/cuda/targets/aarch64-linux/lib/libcudart.so",
        );

        let cuda = cuda_finding(td.path(), ServingCudaNeed::TensorrtLinked);

        assert_eq!(cuda.status_label(), "ok");
        assert!(
            cuda.message
                .contains("/usr/local/cuda/targets/aarch64-linux/lib/libcudart.so"),
            "the finding must name the artifact it found: {}",
            cuda.message
        );
        assert_no_staging_prefix(td.path(), &cuda);
    }

    #[test]
    fn a_tensorrt_linked_build_without_a_cuda_toolkit_is_a_warning() {
        // The Jetson package carries the TensorRT adapter, which cannot
        // load without the CUDA runtime. Calling this host fine would be
        // a false statement about an accelerator path that cannot run.
        let td = TempDir::new().unwrap();
        stage_file(td.path(), "/usr/lib/aarch64-linux-gnu/nvidia/libcuda.so");

        let cuda = cuda_finding(td.path(), ServingCudaNeed::TensorrtLinked);

        assert_eq!(cuda.status_label(), "warning");
        assert_eq!(cuda.severity_label(), "warning");
        assert!(
            cuda.message.contains("no system CUDA toolkit"),
            "the driver's own library is not a CUDA toolkit: {}",
            cuda.message
        );
        assert!(cuda.hint.is_some(), "a warning must say what to install");
    }

    #[test]
    fn a_tensorrt_linked_build_with_a_toolkit_and_no_driver_is_a_warning() {
        // `libcuda` comes from the driver package: a TensorRT adapter
        // linked against the CUDA runtime still cannot load without it,
        // so the toolkit alone must not produce an `ok`.
        let td = TempDir::new().unwrap();
        stage_file(
            td.path(),
            "/usr/local/cuda/targets/aarch64-linux/lib/libcudart.so",
        );

        let cuda = cuda_finding(td.path(), ServingCudaNeed::TensorrtLinked);

        assert_eq!(cuda.status_label(), "warning");
        assert_eq!(cuda.severity_label(), "warning");
        assert!(
            cuda.message
                .contains("no installed component can reach an NVIDIA accelerator"),
            "the absent driver must stay in the message: {}",
            cuda.message
        );
    }

    #[test]
    fn a_system_cuda_toolkit_does_not_stand_in_for_an_absent_driver() {
        // A system CUDA toolkit is an unrelated package. Installing one
        // on a driverless host changes nothing about whether anything
        // can reach the accelerator, so it must not upgrade the verdict.
        let td = TempDir::new().unwrap();
        stage_file(td.path(), "/usr/local/cuda/version.json");

        let cuda = cuda_finding(td.path(), ServingCudaNeed::SidecarRuntime);

        assert_eq!(cuda.status_label(), "missing");
        assert!(
            cuda.message.contains("no NVIDIA driver"),
            "the absent driver is the fact that matters here: {}",
            cuda.message
        );
        assert!(
            cuda.message
                .contains("no installed component can reach an NVIDIA accelerator"),
            "the toolkit must not delete the sentence about the driver: {}",
            cuda.message
        );
    }

    #[test]
    fn an_amd64_host_with_a_driver_and_no_toolkit_is_not_a_defect() {
        // Issue #205: the L4 cloud row. A loaded driver and no system
        // toolkit is exactly what the amd64 build expects, because its
        // worker has no TensorRT adapter and the sidecar's PyTorch wheel
        // carries its own CUDA runtime.
        let td = TempDir::new().unwrap();
        stage_file(td.path(), "/proc/driver/nvidia/version");

        let cuda = cuda_finding(td.path(), ServingCudaNeed::SidecarRuntime);

        assert_eq!(cuda.status_label(), "ok");
        assert_eq!(cuda.severity_label(), "info");
        assert!(
            !cuda.message.contains("CUDA not detected"),
            "a host with a loaded driver was told CUDA was not detected: {}",
            cuda.message
        );
        assert!(
            cuda.message.contains("NVIDIA driver present")
                && cuda.message.contains("no system CUDA toolkit"),
            "both facts must be stated separately: {}",
            cuda.message
        );
        assert!(
            !cuda.message.contains("TensorRT validation will skip"),
            "the amd64 worker has no TensorRT adapter to skip: {}",
            cuda.message
        );
    }

    #[test]
    fn the_amd64_message_does_not_claim_a_sidecar_that_is_not_installed() {
        // A clean amd64 install carries no python_pytorch sidecar, so
        // justifying `ok` with that sidecar's wheel would describe a
        // component the same report lists as `missing`.
        let td = TempDir::new().unwrap();
        stage_file(td.path(), "/proc/driver/nvidia/version");

        let without = cuda_finding(td.path(), ServingCudaNeed::SidecarRuntime);
        assert_eq!(without.status_label(), "ok");
        assert!(
            without
                .message
                .contains("no python_pytorch sidecar is installed"),
            "the absent sidecar must be named as absent: {}",
            without.message
        );

        stage_file(td.path(), PYTHON_PYTORCH_BACKEND_DESCRIPTOR);
        let with = cuda_finding(td.path(), ServingCudaNeed::SidecarRuntime);
        assert_eq!(with.status_label(), "ok");
        assert!(
            with.message
                .contains("the installed python_pytorch sidecar reaches the accelerator"),
            "an installed sidecar must be named as installed: {}",
            with.message
        );
    }

    #[test]
    fn a_host_with_no_serving_worker_skips_the_cuda_finding() {
        // `--cli-only` is a documented install mode that brings no
        // serving worker. Telling that host to install the JetPack CUDA
        // runtime states a requirement of software that is not here.
        let td = TempDir::new().unwrap();
        stage_file(td.path(), "/usr/lib/aarch64-linux-gnu/nvidia/libcuda.so");
        let opts = cuda_opts(td.path());

        let cuda = cuda_runtime_finding(
            ServingCudaNeed::TensorrtLinked,
            probe_installed_cuda_consumers(&opts),
            &probe_cuda_artifacts(&opts),
        );

        assert_eq!(cuda.status_label(), "skipped");
        assert_eq!(cuda.severity_label(), "info");
        assert!(
            !cuda.message.contains("JetPack"),
            "no worker is installed that could need JetPack: {}",
            cuda.message
        );
        assert!(
            cuda.message.contains(SERVING_BINARY_PATH)
                && cuda.message.contains("no python_pytorch sidecar"),
            "the skip must say which components are absent: {}",
            cuda.message
        );
    }

    #[test]
    fn a_sidecar_installed_without_a_serving_worker_is_still_a_cuda_consumer() {
        // `tensorplate-backend-python-pytorch` only `Recommends` the
        // serving package, so the sidecar can be installed alone. It is
        // then the one CUDA consumer on the host, and skipping the
        // finding would state that nothing here consumes a CUDA runtime
        // while the accelerator path of the install is sitting on disk.
        let td = TempDir::new().unwrap();
        stage_file(td.path(), "/proc/driver/nvidia/version");
        stage_file(td.path(), PYTHON_PYTORCH_BACKEND_DESCRIPTOR);
        let opts = cuda_opts(td.path());

        let cuda = cuda_runtime_finding(
            // The TensorRT-linked build, to pin that the absent worker's
            // build no longer decides: no toolkit is asked for here.
            ServingCudaNeed::TensorrtLinked,
            probe_installed_cuda_consumers(&opts),
            &probe_cuda_artifacts(&opts),
        );

        assert_eq!(cuda.status_label(), "ok");
        assert_eq!(cuda.severity_label(), "info");
        assert!(
            cuda.message
                .contains("the installed python_pytorch sidecar is the only CUDA consumer here"),
            "the installed sidecar must be named as the consumer: {}",
            cuda.message
        );
        assert!(
            !cuda.message.contains("JetPack") && cuda.hint != Some("install the JetPack CUDA runtime so `/usr/local/cuda/targets/aarch64-linux/lib/libcudart.so*` is present".to_string()),
            "no worker is installed that could need JetPack: {} / {:?}",
            cuda.message,
            cuda.hint
        );
    }

    #[test]
    fn a_driverless_host_carrying_only_the_sidecar_is_not_skipped() {
        // The state the skip used to hide: the sidecar installed, no
        // driver for it to reach an accelerator through. `skipped` there
        // says nothing consumes a CUDA runtime, which is false.
        let td = TempDir::new().unwrap();
        stage_file(td.path(), PYTHON_PYTORCH_BACKEND_DESCRIPTOR);
        let opts = cuda_opts(td.path());

        let cuda = cuda_runtime_finding(
            ServingCudaNeed::SidecarRuntime,
            probe_installed_cuda_consumers(&opts),
            &probe_cuda_artifacts(&opts),
        );

        assert_eq!(cuda.status_label(), "missing");
        assert!(
            cuda.message
                .contains("no installed component can reach an NVIDIA accelerator"),
            "the absent driver must be stated, not skipped past: {}",
            cuda.message
        );
    }

    #[test]
    fn a_host_with_neither_driver_nor_toolkit_says_the_driver_is_absent() {
        let td = TempDir::new().unwrap();

        let cuda = cuda_finding(td.path(), ServingCudaNeed::SidecarRuntime);

        assert_eq!(cuda.status_label(), "missing");
        assert_eq!(cuda.severity_label(), "info");
        assert!(
            cuda.message.contains("no NVIDIA driver"),
            "the absent driver is the fact that matters here: {}",
            cuda.message
        );
    }

    #[test]
    fn a_runtime_only_cuda_install_counts_as_a_toolkit() {
        // The unversioned `libcudart.so` names are symlinks from the
        // toolkit's development package. A host with only the runtime
        // package has the CUDA runtime and none of those names.
        let td = TempDir::new().unwrap();
        stage_file(td.path(), "/proc/driver/nvidia/version");
        stage_file(td.path(), "/usr/lib/x86_64-linux-gnu/libcudart.so.12");

        let cuda = cuda_finding(td.path(), ServingCudaNeed::TensorrtLinked);

        assert_eq!(cuda.status_label(), "ok");
        assert!(
            cuda.message
                .contains("/usr/lib/x86_64-linux-gnu/libcudart.so.12"),
            "the versioned library found must be named: {}",
            cuda.message
        );
        assert_no_staging_prefix(td.path(), &cuda);
    }

    #[test]
    fn the_versioned_library_scan_reports_the_lowest_soname() {
        // `read_dir` order is unspecified, so the scan sorts. Pin which
        // name wins: one host must report the same path on every run, or
        // the evidence a validation run files differs from the last for
        // no reason.
        let td = TempDir::new().unwrap();
        stage_file(td.path(), "/proc/driver/nvidia/version");
        for name in ["libcudart.so.12", "libcudart.so.11.0", "libcudart.so.9"] {
            stage_file(td.path(), &format!("/usr/lib/x86_64-linux-gnu/{name}"));
        }

        let cuda = cuda_finding(td.path(), ServingCudaNeed::TensorrtLinked);

        assert_eq!(cuda.status_label(), "ok");
        assert!(
            cuda.message
                .contains("/usr/lib/x86_64-linux-gnu/libcudart.so.11.0"),
            "the lexicographically first soname must win: {}",
            cuda.message
        );
    }

    #[test]
    fn an_unversioned_libcudart_name_alone_is_not_a_toolkit_match() {
        // `libcudart.so.` with nothing after it is not a library.
        let td = TempDir::new().unwrap();
        stage_file(td.path(), "/proc/driver/nvidia/version");
        stage_file(td.path(), "/usr/lib/x86_64-linux-gnu/libcudart.so.");

        let cuda = cuda_finding(td.path(), ServingCudaNeed::TensorrtLinked);

        assert_eq!(cuda.status_label(), "warning");
        assert!(
            cuda.message.contains("no system CUDA toolkit"),
            "a bare soname prefix is not a toolkit: {}",
            cuda.message
        );
    }

    #[test]
    fn every_driver_path_in_the_contract_list_is_detected_and_named() {
        // Each entry is a contract path an operator or a validation
        // record can be pointed at. Staging them one at a time pins the
        // whole list: a dropped or misspelled entry stops being found.
        for path in NVIDIA_DRIVER_PATHS {
            let td = TempDir::new().unwrap();
            stage_file(td.path(), path);

            let cuda = cuda_finding(td.path(), ServingCudaNeed::SidecarRuntime);

            assert_eq!(
                cuda.status_label(),
                "ok",
                "{path} was not read as an NVIDIA driver: {}",
                cuda.message
            );
            assert!(
                cuda.message
                    .contains(&format!("NVIDIA driver present (`{path}`)")),
                "the finding must name the driver path it found: {}",
                cuda.message
            );
            assert_no_staging_prefix(td.path(), &cuda);
        }
    }

    #[test]
    fn an_l4t_driver_library_with_another_soname_is_read_as_a_driver() {
        // `/proc/driver/nvidia/version` comes from `nvidia.ko`, which a
        // Jetson does not load, so L4T is recognized by its `libcuda`
        // alone. A layout that ships `libcuda.so.1.1` and no
        // `libcuda.so.1` would read as driverless, which on the arm64
        // build turns a device that is fine into a `warning`.
        let td = TempDir::new().unwrap();
        stage_file(td.path(), "/usr/lib/aarch64-linux-gnu/tegra/libcuda.so.1.1");
        stage_file(
            td.path(),
            "/usr/local/cuda/targets/aarch64-linux/lib/libcudart.so",
        );

        let cuda = cuda_finding(td.path(), ServingCudaNeed::TensorrtLinked);

        assert_eq!(cuda.status_label(), "ok");
        assert!(
            cuda.message.contains(
                "NVIDIA driver present (`/usr/lib/aarch64-linux-gnu/tegra/libcuda.so.1.1`)"
            ),
            "the driver library found must be named: {}",
            cuda.message
        );
        assert_no_staging_prefix(td.path(), &cuda);
    }

    #[test]
    fn the_driver_scan_never_reads_the_cuda_runtime_library_as_a_driver() {
        // Both scans share `/usr/lib/<triple>`, and `libcuda.so.` is a
        // prefix of nothing in `libcudart.so.<soname>` -- the names
        // diverge at the `r`, before the `.`. If that ever stopped
        // holding, a toolkit-only host would report a driver it has not
        // got, which is the one error this finding must not make.
        let td = TempDir::new().unwrap();
        stage_file(td.path(), "/usr/lib/x86_64-linux-gnu/libcudart.so.12");

        let cuda = cuda_finding(td.path(), ServingCudaNeed::SidecarRuntime);

        assert_eq!(cuda.status_label(), "missing");
        assert!(
            cuda.message.contains("no NVIDIA driver at the known paths")
                && cuda
                    .message
                    .contains("system CUDA toolkit at `/usr/lib/x86_64-linux-gnu/libcudart.so.12`"),
            "the CUDA runtime library is a toolkit, never a driver: {}",
            cuda.message
        );
    }

    #[test]
    fn every_driver_library_directory_in_the_contract_list_is_scanned() {
        for dir in NVIDIA_DRIVER_LIB_DIRS {
            let td = TempDir::new().unwrap();
            stage_file(td.path(), &format!("{dir}/libcuda.so.550.54.15"));

            let cuda = cuda_finding(td.path(), ServingCudaNeed::SidecarRuntime);

            assert_eq!(
                cuda.status_label(),
                "ok",
                "{dir} was not scanned for a versioned driver library: {}",
                cuda.message
            );
            assert!(
                cuda.message.contains(&format!(
                    "NVIDIA driver present (`{dir}/libcuda.so.550.54.15`)"
                )),
                "the finding must name the driver library it found: {}",
                cuda.message
            );
            assert_no_staging_prefix(td.path(), &cuda);
        }
    }

    #[test]
    fn every_toolkit_path_in_the_contract_list_is_detected_and_named() {
        for path in CUDA_TOOLKIT_PATHS {
            let td = TempDir::new().unwrap();
            stage_file(td.path(), "/proc/driver/nvidia/version");
            stage_file(td.path(), path);

            let cuda = cuda_finding(td.path(), ServingCudaNeed::TensorrtLinked);

            assert_eq!(
                cuda.status_label(),
                "ok",
                "{path} was not read as a system CUDA toolkit: {}",
                cuda.message
            );
            assert!(
                cuda.message
                    .contains(&format!("system CUDA toolkit at `{path}`")),
                "the finding must name the toolkit path it found: {}",
                cuda.message
            );
            assert_no_staging_prefix(td.path(), &cuda);
        }
    }

    #[test]
    fn every_versioned_library_directory_in_the_contract_list_is_scanned() {
        for dir in CUDA_TOOLKIT_LIB_DIRS {
            let td = TempDir::new().unwrap();
            stage_file(td.path(), "/proc/driver/nvidia/version");
            stage_file(td.path(), &format!("{dir}/libcudart.so.12"));

            let cuda = cuda_finding(td.path(), ServingCudaNeed::TensorrtLinked);

            assert_eq!(
                cuda.status_label(),
                "ok",
                "{dir} was not scanned for a versioned CUDA runtime: {}",
                cuda.message
            );
            assert!(
                cuda.message.contains(&format!("{dir}/libcudart.so.12")),
                "the finding must name the library it found: {}",
                cuda.message
            );
            assert_no_staging_prefix(td.path(), &cuda);
        }
    }

    #[test]
    fn a_platform_with_no_cuda_path_skips_the_finding() {
        // The macOS build has no TensorRT adapter and serves on Metal.
        // Reporting a missing CUDA runtime there describes a runtime
        // nothing on the platform would use.
        let td = TempDir::new().unwrap();

        let cuda = cuda_finding(td.path(), ServingCudaNeed::NoCudaPath);

        assert_eq!(cuda.status_label(), "skipped");
        assert_eq!(cuda.severity_label(), "info");
    }

    #[test]
    fn the_cuda_finding_never_fails_doctor() {
        // doctor's exit code counts `fail`. A host prerequisite is a
        // reportable state, not an install fault, on every combination.
        for need in [
            ServingCudaNeed::TensorrtLinked,
            ServingCudaNeed::SidecarRuntime,
            ServingCudaNeed::NoCudaPath,
        ] {
            for installed in every_installed_combination() {
                for artifacts in [
                    CudaArtifacts::default(),
                    CudaArtifacts {
                        driver: Some("/proc/driver/nvidia/version".into()),
                        toolkit: None,
                    },
                    CudaArtifacts {
                        driver: None,
                        toolkit: Some("/usr/local/cuda/lib64/libcudart.so".into()),
                    },
                    CudaArtifacts {
                        driver: Some("/proc/driver/nvidia/version".into()),
                        toolkit: Some("/usr/local/cuda/lib64/libcudart.so".into()),
                    },
                ] {
                    let finding = cuda_runtime_finding(need, installed, &artifacts);
                    assert_ne!(
                        finding.status_label(),
                        "fail",
                        "{need:?} with {installed:?} and {artifacts:?} failed doctor"
                    );
                    assert_ne!(finding.severity_label(), "critical");
                }
            }
        }
    }

    #[test]
    fn a_driverless_host_is_never_ok_when_a_cuda_consumer_is_installed() {
        // The one verdict the fix must never produce: a host nothing
        // installed can reach an accelerator from, reported as fine.
        for need in [
            ServingCudaNeed::TensorrtLinked,
            ServingCudaNeed::SidecarRuntime,
        ] {
            // Every way a CUDA consumer can be installed, the sidecar
            // without the worker included: that combination reaches its
            // own arm, and a skip there would hide the absent driver.
            for installed in every_installed_combination()
                .into_iter()
                .filter(|c| c.serving_worker || c.python_pytorch_backend)
            {
                for toolkit in [None, Some("/usr/local/cuda/lib64/libcudart.so".to_string())] {
                    let finding = cuda_runtime_finding(
                        need,
                        installed,
                        &CudaArtifacts {
                            driver: None,
                            toolkit: toolkit.clone(),
                        },
                    );
                    assert_ne!(
                        finding.status_label(),
                        "ok",
                        "{need:?} with {installed:?}, no driver and toolkit {toolkit:?} was called ok: {}",
                        finding.message
                    );
                    assert_ne!(
                        finding.status_label(),
                        "skipped",
                        "{need:?} with {installed:?} has a CUDA consumer installed and must not skip: {}",
                        finding.message
                    );
                    assert!(
                        finding
                            .message
                            .contains("no installed component can reach an NVIDIA accelerator"),
                        "{need:?} with {installed:?} and no driver must say so: {}",
                        finding.message
                    );
                }
            }
        }
    }

    #[test]
    fn the_installed_build_decides_what_the_cuda_finding_asks_for() {
        // Stated per target rather than derived from the function, so
        // the mapping is pinned instead of tracked.
        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        assert_eq!(
            installed_serving_cuda_need(),
            ServingCudaNeed::TensorrtLinked
        );
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        assert_eq!(
            installed_serving_cuda_need(),
            ServingCudaNeed::SidecarRuntime
        );
        #[cfg(not(target_os = "linux"))]
        assert_eq!(installed_serving_cuda_need(), ServingCudaNeed::NoCudaPath);
    }

    #[test]
    fn probe_optional_runtimes_takes_the_verdict_against_the_installed_build() {
        // The production seam. The staged host -- serving worker
        // installed, NVIDIA driver present, no system CUDA toolkit --
        // answers with a different status under each `ServingCudaNeed`,
        // so this status is only reachable by reading the build. A
        // constant substituted for `installed_serving_cuda_need()` fails
        // here on every target it does not happen to equal.
        let td = TempDir::new().unwrap();
        stage_serving_binary(td.path());
        stage_file(td.path(), "/proc/driver/nvidia/version");
        let opts = cuda_opts(td.path());

        let findings = probe_optional_runtimes(&opts);
        let cuda = findings
            .iter()
            .find(|f| f.id == FindingId::CudaRuntime)
            .expect("probe_optional_runtimes must report cuda_runtime");

        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        let expected = "warning";
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        let expected = "ok";
        #[cfg(not(target_os = "linux"))]
        let expected = "skipped";

        assert_eq!(cuda.status_label(), expected, "{}", cuda.message);
        assert_no_staging_prefix(td.path(), cuda);
    }
}
