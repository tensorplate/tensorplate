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
use std::process::Command;

use tensorplate_protocol::install_paths::{
    self, AGENT_CONFIG_PATH, BACKEND_DESCRIPTOR_DIR, CLI_CONFIG_PATH, OBSERVABILITY_CONFIG_PATH,
    PYTHON_PYTORCH_BACKEND_DESCRIPTOR, SERVING_BINARY_PATH, SERVING_WORKER_CONFIG_PATH,
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
            probe_backends: cfg!(target_os = "linux"),
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
    out.extend(probe_python_pytorch_backend(opts));
    out.extend(probe_optional_runtimes(opts));
    out
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
    // Treat the install layout as present if any of the durable-state
    // directories or installed binaries exists. We avoid a single
    // probe (e.g. /etc/tensorplate) because dpkg conffiles, broken
    // remove/purge cycles, or operator scripts can leave one of those
    // behind on an otherwise-clean host.
    let candidates = [
        tensorplate_protocol::install_paths::ETC_DIR,
        tensorplate_protocol::install_paths::STATE_DIR,
        tensorplate_protocol::install_paths::LOG_DIR,
        SERVING_BINARY_PATH,
        PYTHON_PYTORCH_BACKEND_DESCRIPTOR,
    ];
    candidates.iter().any(|p| prefixed(opts, p).exists())
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
    if opts.skip_systemd {
        return skipped_service_states("systemd not present on this host");
    }
    if opts.prefix.is_some() {
        return skipped_service_states("service-state query skipped for a prefixed test install");
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
    let descriptor = prefixed(opts, PYTHON_PYTORCH_BACKEND_DESCRIPTOR);
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

fn probe_optional_runtimes(opts: &InstallProbeOptions) -> Vec<Finding> {
    // CUDA / TensorRT / LibTorch live outside the package manifest. We
    // probe well-known absolute paths but never run vendor SDK
    // binaries: a positive result means "the file is on disk", not
    // "this runtime works". Real validation belongs to release validation.
    let cuda = any_runtime_artifact_exists(
        opts,
        &[
            "/usr/local/cuda/version.txt",
            "/usr/local/cuda/version.json",
            "/usr/local/cuda/lib64/libcudart.so",
            "/usr/local/cuda/targets/aarch64-linux/lib/libcudart.so",
            "/usr/lib/x86_64-linux-gnu/libcudart.so",
            "/usr/lib/aarch64-linux-gnu/libcudart.so",
            "/usr/lib/aarch64-linux-gnu/nvidia/libcuda.so",
        ],
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
        runtime_finding_simple(
            FindingId::CudaRuntime,
            cuda,
            "CUDA runtime artifact detected",
            "CUDA not detected; vision-on-TensorRT validation will skip",
        ),
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
    if mode != install_paths::mode::DIR_0750 {
        return Some(format!("{contract_path} mode={mode:#05o} expected=0o750"));
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
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn stage_install_layout(td: &Path) {
        for dir in install_paths::required_directories() {
            let p = td.join(dir.trim_start_matches('/'));
            fs::create_dir_all(&p).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o0750)).unwrap();
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

    #[test]
    fn jetpack_cuda_runtime_layout_is_detected() {
        let td = TempDir::new().unwrap();
        let cudart = td
            .path()
            .join("usr/local/cuda/targets/aarch64-linux/lib/libcudart.so");
        fs::create_dir_all(cudart.parent().unwrap()).unwrap();
        fs::write(&cudart, b"").unwrap();
        let opts = InstallProbeOptions {
            prefix: Some(td.path().to_path_buf()),
            probe_backends: false,
            skip_systemd: true,
        };
        let findings = probe_optional_runtimes(&opts);
        let cuda = findings
            .iter()
            .find(|f| matches!(f.id, FindingId::CudaRuntime))
            .unwrap();
        assert_eq!(cuda.status_label(), "ok");
    }
}
