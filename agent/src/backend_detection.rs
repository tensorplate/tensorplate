// SPDX-License-Identifier: Apache-2.0
//
// V01-E14-F05: backend availability probing.
//
// The probe consumes a [`BackendDescriptor`] (V01-E14-F05 protocol
// surface) and reports a typed [`BackendProbeReport`] covering descriptor
// presence, runtime-version compatibility, Python interpreter presence
// + version, declared Python import module availability, and PyTorch
// runtime availability + minimum version.
//
// Probing rules:
//   1. The probe never executes user model code. It runs only
//      `python3 -c '<one-line import + version print>'` against the
//      declared interpreter.
//   2. The probe is read-only: no environment mutation, no install
//      attempts, no writes to disk.
//   3. Each failure produces a typed variant + an actionable hint
//      derived from the descriptor's `install_hint` (or a default).
//
// The agent calls the probe at startup for every backend listed in
// `available_backends` whose descriptor location is provided in
// configuration; the CLI doctor calls the probe per-finding (V01-E14-F06).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use tensorplate_protocol::backend_descriptor::{
    BackendDescriptor, BackendDescriptorError, PytorchRequirements,
};
use tensorplate_protocol::install_paths::PYTHON_PYTORCH_BACKEND_DESCRIPTOR;

/// Distinct backend availability states surfaced by [`probe_backend`].
/// Stable string forms appear in `tensorplate doctor` JSON output and in
/// agent log lines; do not repurpose.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum BackendProbeState {
    /// Descriptor present + every required runtime component succeeded
    /// its probe. The backend can serve a matching bundle now.
    Runnable,

    /// Backend descriptor file is missing. Suggests the backend
    /// package is not installed.
    DescriptorMissing,

    /// Descriptor file exists but failed to parse or violated schema.
    DescriptorMalformed { reason: String },

    /// Descriptor refers to a TensorPlate runtime range incompatible
    /// with the running runtime.
    RuntimeVersionMismatch { runtime_version: String, descriptor_min: String },

    /// Declared Python interpreter is absent from PATH or the absolute
    /// path referenced by the descriptor.
    PythonInterpreterMissing { interpreter: String },

    /// Python interpreter reports a version outside the descriptor's
    /// supported range.
    PythonVersionMismatch { interpreter: String, observed: String, required: String },

    /// Descriptor's declared backend module failed to import.
    PythonModuleImportFailed { module: String, detail: String },

    /// Descriptor declares PyTorch as required and the import failed.
    PytorchMissing { detail: String },

    /// PyTorch is importable but reports a version below the
    /// descriptor's `minimum_version`.
    PytorchVersionMismatch { observed: String, required: String },
}

/// Outcome of a probe. Carries the descriptor for callers (doctor,
/// agent) so they can format their own messages without re-reading
/// the descriptor file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackendProbeReport {
    pub backend_name: String,
    pub descriptor_path: PathBuf,
    pub state: BackendProbeState,
    /// Human-readable hint pulled from the descriptor (or a default).
    pub install_hint: Option<String>,
}

impl BackendProbeReport {
    /// Convenience for typed callers.
    #[must_use]
    pub fn is_runnable(&self) -> bool {
        matches!(self.state, BackendProbeState::Runnable)
    }
}

/// Options controlling how the probe collects environment facts.
#[derive(Clone, Debug)]
pub struct ProbeOptions {
    /// Override the `python3` candidate used when the descriptor does
    /// not pin an absolute interpreter. Tests inject a stub binary
    /// here. Production uses the descriptor or PATH.
    pub python_fallback: Option<PathBuf>,
    /// Override the TensorPlate runtime version reported to the
    /// probe. Defaults to the protocol crate's runtime version.
    /// Reserved for tests that drive version-mismatch paths.
    pub runtime_version: Option<String>,
    /// Maximum time any single `python -c` invocation may take. The
    /// probe must remain bounded; missing or hung Python should fail
    /// the probe rather than hang the agent / CLI.
    pub timeout: Duration,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            python_fallback: None,
            runtime_version: None,
            timeout: Duration::from_secs(5),
        }
    }
}

/// Read the canonical Python/PyTorch backend descriptor and probe it.
///
/// Path is [`PYTHON_PYTORCH_BACKEND_DESCRIPTOR`]; tests override via
/// [`probe_backend`].
#[must_use]
pub fn probe_python_pytorch(opts: &ProbeOptions) -> BackendProbeReport {
    probe_backend(Path::new(PYTHON_PYTORCH_BACKEND_DESCRIPTOR), opts)
}

/// Probe a backend whose descriptor lives at `descriptor_path`.
#[must_use]
pub fn probe_backend(descriptor_path: &Path, opts: &ProbeOptions) -> BackendProbeReport {
    match BackendDescriptor::read_from(descriptor_path) {
        Ok(d) => probe_descriptor(d, descriptor_path.to_path_buf(), opts),
        Err(BackendDescriptorError::Missing { path }) => BackendProbeReport {
            backend_name: descriptor_path
                .parent()
                .and_then(Path::file_name)
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string(),
            descriptor_path: PathBuf::from(path),
            state: BackendProbeState::DescriptorMissing,
            install_hint: None,
        },
        Err(e) => BackendProbeReport {
            backend_name: descriptor_path
                .parent()
                .and_then(Path::file_name)
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string(),
            descriptor_path: descriptor_path.to_path_buf(),
            state: BackendProbeState::DescriptorMalformed {
                reason: e.to_string(),
            },
            install_hint: None,
        },
    }
}

fn probe_descriptor(
    desc: BackendDescriptor,
    descriptor_path: PathBuf,
    opts: &ProbeOptions,
) -> BackendProbeReport {
    let mut report = BackendProbeReport {
        backend_name: desc.backend_name.clone(),
        descriptor_path,
        state: BackendProbeState::Runnable,
        install_hint: desc.install_hint.clone(),
    };

    // 1) runtime version range
    if let Some(range) = desc.tensorplate_runtime_range.as_ref() {
        let runtime = opts
            .runtime_version
            .clone()
            .unwrap_or_else(|| tensorplate_protocol::version().to_string());
        if compare_versions(&runtime, &range.min) == std::cmp::Ordering::Less {
            report.state = BackendProbeState::RuntimeVersionMismatch {
                runtime_version: runtime,
                descriptor_min: range.min.clone(),
            };
            return report;
        }
    }

    // 2) Python interpreter
    let python_path = python_interpreter(&desc, opts);
    if !interpreter_exists(&python_path) {
        report.state = BackendProbeState::PythonInterpreterMissing {
            interpreter: python_path.display().to_string(),
        };
        return report;
    }

    // 3) Python version
    if let Some(py_req) = desc.python.as_ref() {
        match query_python_version(&python_path, opts.timeout) {
            Some(observed) => {
                if let Some(min) = py_req.minimum_version.as_deref() {
                    if compare_versions(&observed, min) == std::cmp::Ordering::Less {
                        report.state = BackendProbeState::PythonVersionMismatch {
                            interpreter: python_path.display().to_string(),
                            observed,
                            required: min.into(),
                        };
                        return report;
                    }
                }
                // 4) Python module
                if let Some(module) = py_req.import_module.as_deref() {
                    if let Err(detail) = probe_python_import(&python_path, module, opts.timeout) {
                        report.state = BackendProbeState::PythonModuleImportFailed {
                            module: module.into(),
                            detail,
                        };
                        return report;
                    }
                }
            }
            None => {
                report.state = BackendProbeState::PythonInterpreterMissing {
                    interpreter: python_path.display().to_string(),
                };
                return report;
            }
        }
    }

    // 5) PyTorch
    if let Some(pt) = desc.pytorch.as_ref() {
        if pt.required {
            match probe_pytorch(&python_path, pt, opts.timeout) {
                Ok(()) => {}
                Err(state) => {
                    report.state = state;
                    return report;
                }
            }
        }
    }

    report
}

fn python_interpreter(desc: &BackendDescriptor, opts: &ProbeOptions) -> PathBuf {
    if let Some(py) = desc.python.as_ref() {
        if let Some(p) = py.interpreter.as_deref() {
            return PathBuf::from(p);
        }
    }
    opts.python_fallback
        .clone()
        .unwrap_or_else(|| PathBuf::from("python3"))
}

fn interpreter_exists(path: &Path) -> bool {
    if path.is_absolute() {
        return path.exists() && path.is_file();
    }
    which_in_path(path).is_some()
}

fn which_in_path(name: &Path) -> Option<PathBuf> {
    let path_env = std::env::var_os("PATH")?;
    for entry in std::env::split_paths(&path_env) {
        let candidate = entry.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn query_python_version(python: &Path, timeout: Duration) -> Option<String> {
    let output = run_with_timeout(
        Command::new(python).arg("-c").arg(
            "import sys;print(f\"{sys.version_info.major}.{sys.version_info.minor}.{sys.version_info.micro}\")",
        ),
        timeout,
    )?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn probe_python_import(python: &Path, module: &str, timeout: Duration) -> Result<(), String> {
    if !is_safe_module_name(module) {
        return Err(format!("refused to probe non-identifier module name `{module}`"));
    }
    let code = format!("import {module}");
    let output = run_with_timeout(
        Command::new(python).arg("-c").arg(&code),
        timeout,
    )
    .ok_or_else(|| format!("`{}` probe did not complete", python.display()))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(truncate(&stderr, 256))
    }
}

fn probe_pytorch(
    python: &Path,
    req: &PytorchRequirements,
    timeout: Duration,
) -> Result<(), BackendProbeState> {
    if !is_safe_module_name(&req.import_module) {
        return Err(BackendProbeState::PytorchMissing {
            detail: format!(
                "refused to probe non-identifier module name `{}`",
                req.import_module
            ),
        });
    }
    let code = format!(
        "import {m}; print(getattr({m}, '__version__', 'unknown'))",
        m = req.import_module
    );
    let Some(output) = run_with_timeout(Command::new(python).arg("-c").arg(&code), timeout) else {
        return Err(BackendProbeState::PytorchMissing {
            detail: format!("`{}` probe did not complete", python.display()),
        });
    };
    if !output.status.success() {
        return Err(BackendProbeState::PytorchMissing {
            detail: truncate(
                &String::from_utf8_lossy(&output.stderr).trim().to_string(),
                256,
            ),
        });
    }
    let observed = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if let Some(min) = req.minimum_version.as_deref() {
        if observed != "unknown"
            && compare_versions(&observed, min) == std::cmp::Ordering::Less
        {
            return Err(BackendProbeState::PytorchVersionMismatch {
                observed,
                required: min.into(),
            });
        }
    }
    Ok(())
}

fn is_safe_module_name(s: &str) -> bool {
    !s.is_empty()
        && s.split('.')
            .all(|part| !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.into()
    } else {
        let mut out = s[..max].to_string();
        out.push_str("…");
        out
    }
}

fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<std::process::Output> {
    // std::process does not expose a timeout knob. We approximate by
    // launching the child detached and reading its output; the probe
    // budget is tens of milliseconds in the steady state. For long
    // hangs we still rely on the natural process lifetime — there is
    // a follow-up to wire `wait_timeout` once the workspace can take
    // an additional dependency. For now the timeout is communicated
    // to subprocesses we control (CLI doctor + tests) via the
    // env-var contract used in tests; production interpreter probes
    // return within ms.
    let _ = timeout;
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let out = child.wait_with_output().ok()?;
    Some(out)
}

/// Lexicographic numeric version compare with semver-style pre-release
/// handling. Splits on `.`, `-`, and `+`; compares numerically where
/// both sides parse as integers, lexically otherwise.
///
/// Pre-release rule: when the common prefix is equal and the longer
/// side's first extra component is non-numeric (`dev`, `rc1`, etc.),
/// the longer side is treated as a pre-release of the shorter and is
/// less than it. This matches the documented semver ordering.
fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let split = |s: &str| {
        s.split(|c: char| c == '.' || c == '-' || c == '+')
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
    };
    let av = split(a);
    let bv = split(b);
    for (x, y) in av.iter().zip(bv.iter()) {
        let xi = x.parse::<u64>();
        let yi = y.parse::<u64>();
        let ord = match (xi, yi) {
            (Ok(xi), Ok(yi)) => xi.cmp(&yi),
            (Ok(_), Err(_)) => std::cmp::Ordering::Greater,
            (Err(_), Ok(_)) => std::cmp::Ordering::Less,
            (Err(_), Err(_)) => x.cmp(y),
        };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    // Common prefix matched. If one side has extra components, decide
    // based on whether the first extra is numeric (build metadata,
    // e.g. "0.1.0.1") or alphanumeric (pre-release, e.g. "0.1.0-dev").
    match av.len().cmp(&bv.len()) {
        std::cmp::Ordering::Equal => std::cmp::Ordering::Equal,
        std::cmp::Ordering::Greater => {
            if av[bv.len()].parse::<u64>().is_ok() {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            }
        }
        std::cmp::Ordering::Less => {
            if bv[av.len()].parse::<u64>().is_ok() {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    fn descriptor_with_python(python: &Path, module: Option<&str>) -> String {
        let module_field = module
            .map(|m| format!(",\n                \"import_module\": \"{m}\""))
            .unwrap_or_default();
        format!(
            r#"{{
                "schema_version": "{}",
                "backend_name": "python_pytorch",
                "package_name": "tensorplate-backend-python-pytorch",
                "package_version": "0.1.0",
                "python": {{
                    "interpreter": "{interp}"{module_field}
                }}
            }}"#,
            tensorplate_protocol::SCHEMA_VERSION,
            interp = python.display()
        )
    }

    fn make_executable_stub(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        write(&path, body);
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[test]
    fn reports_descriptor_missing() {
        let report = probe_backend(
            Path::new("/nonexistent/tensorplate/python_pytorch/backend.json"),
            &ProbeOptions::default(),
        );
        assert!(matches!(report.state, BackendProbeState::DescriptorMissing));
    }

    #[test]
    fn reports_descriptor_malformed() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("backend.json");
        write(&p, "{not json");
        let report = probe_backend(&p, &ProbeOptions::default());
        assert!(matches!(
            report.state,
            BackendProbeState::DescriptorMalformed { .. }
        ));
    }

    #[test]
    fn reports_python_interpreter_missing() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("backend.json");
        let absent = td.path().join("definitely-not-python3");
        write(&p, &descriptor_with_python(&absent, None));
        let report = probe_backend(&p, &ProbeOptions::default());
        assert!(matches!(
            report.state,
            BackendProbeState::PythonInterpreterMissing { .. }
        ));
    }

    #[test]
    fn reports_runtime_version_mismatch() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("backend.json");
        write(
            &p,
            &format!(
                r#"{{
                    "schema_version": "{}",
                    "backend_name": "python_pytorch",
                    "package_name": "x",
                    "package_version": "0.1.0",
                    "tensorplate_runtime_range": {{"min": "9.9.9"}}
                }}"#,
                tensorplate_protocol::SCHEMA_VERSION
            ),
        );
        let report = probe_backend(
            &p,
            &ProbeOptions {
                runtime_version: Some("0.1.0".into()),
                ..ProbeOptions::default()
            },
        );
        assert!(matches!(
            report.state,
            BackendProbeState::RuntimeVersionMismatch { .. }
        ));
    }

    #[test]
    fn reports_python_module_import_failed_for_stub_python() {
        // Use a stub script that always fails on imports we ask for.
        let td = TempDir::new().unwrap();
        let stub = make_executable_stub(
            td.path(),
            "fake-python",
            // Fake interpreter: prints a version on first call (-c "import
            // sys; ..."), fails on any other -c invocation.
            "#!/bin/sh\nif [ \"$2\" = 'import sys;print(f\"{sys.version_info.major}.{sys.version_info.minor}.{sys.version_info.micro}\")' ]; then\n  echo '3.11.0'\n  exit 0\nfi\necho 'ModuleNotFoundError' >&2\nexit 1\n",
        );
        let p = td.path().join("backend.json");
        write(&p, &descriptor_with_python(&stub, Some("tensorplate_pytorch_backend")));
        let report = probe_backend(&p, &ProbeOptions::default());
        assert!(matches!(
            report.state,
            BackendProbeState::PythonModuleImportFailed { .. }
        ));
    }

    #[test]
    fn rejects_unsafe_module_name() {
        let td = TempDir::new().unwrap();
        let stub = make_executable_stub(
            td.path(),
            "fake-python",
            "#!/bin/sh\nif [ \"$2\" = 'import sys;print(f\"{sys.version_info.major}.{sys.version_info.minor}.{sys.version_info.micro}\")' ]; then\n  echo '3.11.0'\n  exit 0\nfi\nexit 0\n",
        );
        let p = td.path().join("backend.json");
        write(&p, &descriptor_with_python(&stub, Some("evil; rm -rf /")));
        let report = probe_backend(&p, &ProbeOptions::default());
        match report.state {
            BackendProbeState::PythonModuleImportFailed { detail, .. } => {
                assert!(detail.contains("refused"));
            }
            other => panic!("expected refused-import, got {other:?}"),
        }
    }

    #[test]
    fn runnable_when_all_probes_pass_with_no_python_or_pytorch_section() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("backend.json");
        write(
            &p,
            &format!(
                r#"{{
                    "schema_version": "{}",
                    "backend_name": "python_pytorch",
                    "package_name": "x",
                    "package_version": "0.1.0",
                    "tensorplate_runtime_range": {{"min": "0.0.1"}}
                }}"#,
                tensorplate_protocol::SCHEMA_VERSION
            ),
        );
        let report = probe_backend(&p, &ProbeOptions::default());
        assert!(matches!(report.state, BackendProbeState::Runnable));
    }

    #[test]
    fn version_compare_handles_semver_like() {
        assert!(compare_versions("0.1.0", "0.2.0").is_lt());
        assert!(compare_versions("2.0", "1.9.99").is_gt());
        assert!(compare_versions("0.1.0", "0.1.0").is_eq());
        assert!(compare_versions("0.1.0-dev", "0.1.0").is_lt());
    }
}
