// SPDX-License-Identifier: Apache-2.0
//
// V01-E14-F06: install-time doctor probes.
//
// `tensorplate doctor` aggregates these alongside the V01-E11 agent
// probes. They cover the packaging contract from V01-E14:
//   - filesystem layout (paths, permissions, group ownership)
//   - config files (presence + schema_version sanity)
//   - systemd units (agent + observability present, no serving unit)
//   - serving binary installed under /usr/lib/tensorplate/
//   - CUDA / TensorRT / LibTorch presence (best-effort)
//   - Python/PyTorch backend descriptor + runtime status (V01-E14-F05)
//
// Every probe returns at least one finding. Probes never mutate state
// and never run user code. Filesystem checks degrade gracefully on
// non-Linux hosts: a missing path on macOS dev hosts becomes a
// `skipped` finding, not a `fail`, so workspace CI still passes.

use std::path::{Path, PathBuf};

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
#[must_use]
pub fn run(opts: &InstallProbeOptions) -> Vec<Finding> {
    let mut out = Vec::new();
    out.extend(probe_path_layout(opts));
    out.extend(probe_config_files(opts));
    out.extend(probe_serving_binary(opts));
    out.extend(probe_systemd_units(opts));
    out.extend(probe_python_pytorch_backend(opts));
    out.extend(probe_optional_runtimes(opts));
    out
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
    let mut world_writable: Vec<String> = Vec::new();
    for dir in install_paths::required_directories() {
        let path = prefixed(opts, dir);
        if !path.exists() {
            missing.push((*dir).to_string());
            continue;
        }
        if is_world_writable(&path) {
            world_writable.push((*dir).to_string());
        }
    }
    if !world_writable.is_empty() {
        out.push(Finding::fail(
            FindingId::PathLayout,
            Severity::Critical,
            format!(
                "world-writable install paths detected: {}",
                world_writable.join(", ")
            ),
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

fn probe_optional_runtimes(_opts: &InstallProbeOptions) -> Vec<Finding> {
    // CUDA / TensorRT / LibTorch live outside the package manifest. We
    // probe well-known absolute paths but never run vendor SDK
    // binaries: a positive result means "the file is on disk", not
    // "this runtime works". Real validation belongs to V01-E15.
    let cuda = path_exists("/usr/local/cuda/version.txt")
        || path_exists("/usr/local/cuda/version.json")
        || path_exists("/usr/lib/x86_64-linux-gnu/libcudart.so")
        || path_exists("/usr/lib/aarch64-linux-gnu/libcudart.so");
    let tensorrt = path_exists("/usr/include/NvInferVersion.h")
        || path_exists("/usr/lib/x86_64-linux-gnu/libnvinfer.so")
        || path_exists("/usr/lib/aarch64-linux-gnu/libnvinfer.so");
    let libtorch = path_exists("/usr/local/libtorch")
        || path_exists("/opt/libtorch")
        || path_exists("/usr/lib/libtorch.so");

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

#[cfg(unix)]
fn is_world_writable(p: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p)
        .map(|m| (m.mode() & 0o0002) != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_world_writable(_p: &Path) -> bool {
    false
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
            fs::set_permissions(&p, fs::Permissions::from_mode(0o0640)).unwrap();
        };
        let v = tensorplate_protocol::SCHEMA_VERSION;
        conf(
            AGENT_CONFIG_PATH,
            &format!("{{\"schema_version\":\"{v}\"}}"),
        );
        conf(
            OBSERVABILITY_CONFIG_PATH,
            &format!("{{\"schema_version\":\"{v}\"}}"),
        );
        conf(
            SERVING_WORKER_CONFIG_PATH,
            &format!("{{\"schema_version\":\"{v}\"}}"),
        );
        conf(CLI_CONFIG_PATH, &format!("{{\"schema_version\":\"{v}\"}}"));
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
}
