// SPDX-License-Identifier: Apache-2.0
//
// packaging: backend availability probing.
//
// The probe consumes a [`BackendDescriptor`] (packaging protocol
// surface) and reports a typed [`BackendProbeReport`] covering descriptor
// presence, runtime-version compatibility, Python interpreter presence
// + version, declared Python import module availability, and PyTorch
// runtime availability + minimum version.
//
// A bundle that names a runner profile runs in that profile's interpreter,
// not the descriptor's `python.interpreter`, so every installed profile is
// probed where its sidecar would start and the report keeps one state per
// profile. [`BackendProbeReport::serving_state`] picks the state that
// decides one bundle.
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
// configuration; the CLI doctor calls the probe per-finding (packaging).

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::backend_descriptor::{
    BackendDescriptor, BackendDescriptorError, PytorchRequirements, RunnerProfile,
};
use crate::install_paths::PYTHON_PYTORCH_BACKEND_DESCRIPTOR;
use crate::package_inventory::{
    read_in_background, wait_bounded, DpkgInventory, PackageInventory, PIPE_CLOSE_GRACE,
};

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

    /// Descriptor file exists but failed to parse or violated schema, or
    /// a runner profile declaration beside it was refused.
    DescriptorMalformed { reason: String },

    /// A runner profile declared for the backend names a package that is
    /// not installed. `declaration` is the file that declares the profile.
    RunnerProfilePackageMissing {
        profile: String,
        package: String,
        declaration: String,
    },

    /// Descriptor refers to a TensorPlate runtime range incompatible
    /// with the running runtime.
    RuntimeVersionMismatch {
        runtime_version: String,
        descriptor_min: String,
    },

    /// Declared Python interpreter is absent from PATH or the absolute
    /// path referenced by the descriptor.
    PythonInterpreterMissing { interpreter: String },

    /// Python interpreter reports a version outside the descriptor's
    /// supported range.
    PythonVersionMismatch {
        interpreter: String,
        observed: String,
        required: String,
    },

    /// Descriptor's declared backend module failed to import.
    PythonModuleImportFailed { module: String, detail: String },

    /// Descriptor declares PyTorch as required and the import failed.
    PytorchMissing { detail: String },

    /// PyTorch is importable but reports a version below the
    /// descriptor's `minimum_version`.
    PytorchVersionMismatch { observed: String, required: String },
}

impl BackendProbeState {
    /// Whether the state refuses every bundle of the backend, rather than
    /// the bundles that would run in one interpreter.
    #[must_use]
    pub const fn is_backend_wide(&self) -> bool {
        match self {
            Self::DescriptorMissing
            | Self::DescriptorMalformed { .. }
            | Self::RunnerProfilePackageMissing { .. }
            | Self::RuntimeVersionMismatch { .. } => true,
            Self::Runnable
            | Self::PythonInterpreterMissing { .. }
            | Self::PythonVersionMismatch { .. }
            | Self::PythonModuleImportFailed { .. }
            | Self::PytorchMissing { .. }
            | Self::PytorchVersionMismatch { .. } => false,
        }
    }
}

/// What a runner profile's interpreter reported about itself.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ObservedInterpreter {
    /// `major.minor.micro`.
    pub version: String,
    /// `sys.version`, as printed.
    pub sys_version: String,
    /// `sys.prefix`: the environment the interpreter runs in.
    pub prefix: String,
}

/// One installed runner profile, probed where its sidecar would start.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunnerProfileProbe {
    /// The profile as the installed descriptor declares it.
    pub profile: RunnerProfile,
    /// Whether the profile's interpreter can start the sidecar. Never a
    /// PyTorch state: a profile's engines are not the descriptor's.
    pub state: BackendProbeState,
    /// `None` when the interpreter did not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed: Option<ObservedInterpreter>,
}

/// The state that decides whether a backend can serve one bundle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServingState<'a> {
    /// A backend-wide refusal, or the state of the interpreter the
    /// bundle's sidecar would run in.
    Probed(&'a BackendProbeState),
    /// The bundle names a runner profile that is not installed.
    RunnerProfileNotInstalled,
}

/// Outcome of a probe. Carries the descriptor for callers (doctor,
/// agent) so they can format their own messages without re-reading
/// the descriptor file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackendProbeReport {
    pub backend_name: String,
    pub descriptor_path: PathBuf,
    /// A backend-wide refusal, or the state of the descriptor's own
    /// interpreter, where a bundle that names no runner profile runs.
    pub state: BackendProbeState,
    /// Human-readable hint pulled from the descriptor (or a default).
    pub install_hint: Option<String>,
    /// Every installed runner profile, in the descriptor's merged order.
    /// Empty when `state` is backend-wide: nothing was probed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runner_profiles: Vec<RunnerProfileProbe>,
}

impl BackendProbeReport {
    /// Whether a bundle that names no runner profile can be served.
    #[must_use]
    pub fn is_runnable(&self) -> bool {
        matches!(self.state, BackendProbeState::Runnable)
    }

    /// The state that decides a bundle naming `runner_profile`, or none.
    #[must_use]
    pub fn serving_state(&self, runner_profile: Option<&str>) -> ServingState<'_> {
        let Some(id) = runner_profile else {
            return ServingState::Probed(&self.state);
        };
        if self.state.is_backend_wide() {
            return ServingState::Probed(&self.state);
        }
        self.runner_profiles
            .iter()
            .find(|probe| probe.profile.id == id)
            .map_or(ServingState::RunnerProfileNotInstalled, |probe| {
                ServingState::Probed(&probe.state)
            })
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
    /// Directory an installed tree is staged under: prepended to every
    /// runner profile path that is executed. Tests only; reported paths
    /// stay the declared ones.
    pub staged_root: Option<PathBuf>,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            python_fallback: None,
            runtime_version: None,
            timeout: Duration::from_secs(5),
            staged_root: None,
        }
    }
}

/// The variables the serving worker's launcher sets for a runner
/// profile's sidecar, over the environment it inherits. Mirrors
/// `runner_launch_request` in
/// `runtime/src/adapters/python_pytorch/runner_profile.cpp`.
#[must_use]
pub fn runner_launch_environment(
    profile: &RunnerProfile,
    temp_dir: &Path,
) -> [(&'static str, String); 3] {
    [
        ("LD_LIBRARY_PATH", profile.library_search_paths.join(":")),
        ("ORT_DISABLE_TELEMETRY", "1".to_string()),
        ("TMPDIR", temp_dir.display().to_string()),
    ]
}

/// The variables C++'s `temp_directory_path` reads, in its order.
pub const LAUNCHER_TEMP_VARIABLES: [&str; 4] = ["TMPDIR", "TMP", "TEMP", "TEMPDIR"];

/// The temporary directory the launcher hands a runner profile's sidecar,
/// in an environment answering `lookup`: the first of
/// [`LAUNCHER_TEMP_VARIABLES`] that is set, and `/tmp` otherwise.
#[must_use]
pub fn launcher_temp_dir(lookup: impl Fn(&str) -> Option<String>) -> PathBuf {
    LAUNCHER_TEMP_VARIABLES
        .into_iter()
        .find_map(lookup)
        .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from)
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
    probe_backend_with_inventory(descriptor_path, opts, &DpkgInventory::default())
}

/// [`probe_backend`] with the installed packages taken from `inventory`.
#[must_use]
pub fn probe_backend_with_inventory(
    descriptor_path: &Path,
    opts: &ProbeOptions,
    inventory: &dyn PackageInventory,
) -> BackendProbeReport {
    report_read(
        BackendDescriptor::read_with_inventory(descriptor_path, inventory),
        descriptor_path,
        opts,
    )
}

/// The report for one read of the descriptor: its probes when it was read,
/// the state its refusal maps to when it was not.
fn report_read(
    read: Result<BackendDescriptor, BackendDescriptorError>,
    descriptor_path: &Path,
    opts: &ProbeOptions,
) -> BackendProbeReport {
    let unread = |descriptor_path: PathBuf, state| BackendProbeReport {
        backend_name: descriptor_path
            .parent()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string(),
        descriptor_path,
        state,
        install_hint: None,
        runner_profiles: Vec::new(),
    };
    match read {
        Ok(d) => probe_descriptor(&d, descriptor_path.to_path_buf(), opts),
        Err(BackendDescriptorError::Missing { path }) => {
            unread(PathBuf::from(path), BackendProbeState::DescriptorMissing)
        }
        Err(BackendDescriptorError::PackageNotInstalled {
            path,
            profile,
            package,
        }) => unread(
            descriptor_path.to_path_buf(),
            BackendProbeState::RunnerProfilePackageMissing {
                profile,
                package,
                declaration: path,
            },
        ),
        Err(e) => unread(
            descriptor_path.to_path_buf(),
            BackendProbeState::DescriptorMalformed {
                reason: e.to_string(),
            },
        ),
    }
}

fn probe_descriptor(
    desc: &BackendDescriptor,
    descriptor_path: PathBuf,
    opts: &ProbeOptions,
) -> BackendProbeReport {
    let mut report = BackendProbeReport {
        backend_name: desc.backend_name.clone(),
        descriptor_path,
        state: BackendProbeState::Runnable,
        install_hint: desc.install_hint.clone(),
        runner_profiles: Vec::new(),
    };

    // 1) runtime version range
    if let Some(range) = desc.tensorplate_runtime_range.as_ref() {
        let runtime = opts
            .runtime_version
            .clone()
            .unwrap_or_else(|| crate::version().to_string());
        if compare_versions(&runtime, &range.min) == std::cmp::Ordering::Less {
            report.state = BackendProbeState::RuntimeVersionMismatch {
                runtime_version: runtime,
                descriptor_min: range.min.clone(),
            };
            return report;
        }
    }

    report.runner_profiles = desc
        .runner_profiles
        .iter()
        .map(|profile| probe_runner_profile(desc, profile, opts))
        .collect();
    report.state = probe_own_interpreter(desc, opts);
    report
}

/// The state of the descriptor's own interpreter: `python` and `pytorch`.
fn probe_own_interpreter(desc: &BackendDescriptor, opts: &ProbeOptions) -> BackendProbeState {
    // 2) Python interpreter
    let python_path = python_interpreter(desc, opts);
    if !interpreter_exists(&python_path) {
        return BackendProbeState::PythonInterpreterMissing {
            interpreter: python_path.display().to_string(),
        };
    }

    // 3) Python version
    if let Some(py_req) = desc.python.as_ref() {
        if let Some(observed) = query_python_version(&python_path, opts.timeout) {
            if let Some(min) = py_req.minimum_version.as_deref() {
                if compare_versions(&observed, min) == std::cmp::Ordering::Less {
                    return BackendProbeState::PythonVersionMismatch {
                        interpreter: python_path.display().to_string(),
                        observed,
                        required: min.into(),
                    };
                }
            }
            // 4) Python module
            if let Some(module) = py_req.import_module.as_deref() {
                if let Err(detail) = probe_python_import(&python_path, module, &[], opts.timeout) {
                    return BackendProbeState::PythonModuleImportFailed {
                        module: module.into(),
                        detail,
                    };
                }
            }
        } else {
            return BackendProbeState::PythonInterpreterMissing {
                interpreter: python_path.display().to_string(),
            };
        }
    }

    // 5) PyTorch
    if let Some(pt) = desc.pytorch.as_ref() {
        if pt.required {
            if let Err(state) = probe_pytorch(&python_path, pt, opts.timeout) {
                return state;
            }
        }
    }

    BackendProbeState::Runnable
}

/// What a runner profile's interpreter is asked about itself. Its first
/// line is [`INTERPRETER_QUERY_MARKER`].
const INTERPRETER_QUERY: &str = include_str!("backend_probe_interpreter.py");

/// Marks the interpreter query in a `-c` argument, so a recorded
/// interpreter can tell the probe's queries apart.
pub const INTERPRETER_QUERY_MARKER: &str = "# tensorplate probe: interpreter";

/// Probe one runner profile's interpreter in the environment the launcher
/// gives its sidecar. An interpreter the system will not execute is
/// missing, as the launcher refuses it; the descriptor's `python`
/// requirements hold for the sidecar module wherever it runs.
fn probe_runner_profile(
    desc: &BackendDescriptor,
    profile: &RunnerProfile,
    opts: &ProbeOptions,
) -> RunnerProfileProbe {
    let mut probe = RunnerProfileProbe {
        profile: profile.clone(),
        state: BackendProbeState::Runnable,
        observed: None,
    };
    let missing = || BackendProbeState::PythonInterpreterMissing {
        interpreter: profile.interpreter.clone(),
    };
    let interpreter = staged(opts, &profile.interpreter);
    let environment =
        runner_launch_environment(profile, &launcher_temp_dir(|name| std::env::var(name).ok()));
    let Some(observed) = observe_interpreter(&interpreter, &environment, opts.timeout) else {
        probe.state = missing();
        return probe;
    };
    let requirements = desc.python.as_ref();
    if let Some(min) = requirements.and_then(|py| py.minimum_version.as_deref()) {
        if compare_versions(&observed.version, min) == std::cmp::Ordering::Less {
            probe.state = BackendProbeState::PythonVersionMismatch {
                interpreter: profile.interpreter.clone(),
                observed: observed.version.clone(),
                required: min.into(),
            };
        }
    }
    if let (BackendProbeState::Runnable, Some(module)) = (
        &probe.state,
        requirements.and_then(|py| py.import_module.as_deref()),
    ) {
        if let Err(detail) = probe_python_import(&interpreter, module, &environment, opts.timeout) {
            probe.state = BackendProbeState::PythonModuleImportFailed {
                module: module.into(),
                detail,
            };
        }
    }
    probe.observed = Some(observed);
    probe
}

/// `path` under the staged root, when the options name one.
#[must_use]
pub fn staged(opts: &ProbeOptions, path: &str) -> PathBuf {
    opts.staged_root.as_ref().map_or_else(
        || PathBuf::from(path),
        |root| root.join(path.trim_start_matches('/')),
    )
}

fn observe_interpreter(
    python: &Path,
    environment: &[(&'static str, String)],
    timeout: Duration,
) -> Option<ObservedInterpreter> {
    let output = run_with_timeout(
        Command::new(python)
            .arg("-c")
            .arg(INTERPRETER_QUERY)
            .envs(environment.iter().map(|(name, value)| (name, value))),
        timeout,
    )?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

fn python_interpreter(desc: &BackendDescriptor, opts: &ProbeOptions) -> PathBuf {
    if let Some(py) = desc.python.as_ref() {
        if let Some(p) = py.interpreter.as_deref() {
            return if Path::new(p).is_absolute() {
                staged(opts, p)
            } else {
                PathBuf::from(p)
            };
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

fn probe_python_import(
    python: &Path,
    module: &str,
    environment: &[(&'static str, String)],
    timeout: Duration,
) -> Result<(), String> {
    if !is_safe_module_name(module) {
        return Err(format!(
            "refused to probe non-identifier module name `{module}`"
        ));
    }
    let code = format!("import {module}");
    let output = run_with_timeout(
        Command::new(python)
            .arg("-c")
            .arg(&code)
            .envs(environment.iter().map(|(name, value)| (name, value))),
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

/// A framework import reads gigabytes of libraries, from a cold page cache
/// after a boot: it gets this long, however short the other queries' limit.
const FRAMEWORK_IMPORT_TIMEOUT: Duration = Duration::from_secs(120);

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
    let Some(output) = run_with_timeout(
        Command::new(python).arg("-c").arg(&code),
        timeout.max(FRAMEWORK_IMPORT_TIMEOUT),
    ) else {
        return Err(BackendProbeState::PytorchMissing {
            detail: format!("`{}` probe did not complete", python.display()),
        });
    };
    if !output.status.success() {
        return Err(BackendProbeState::PytorchMissing {
            detail: truncate(String::from_utf8_lossy(&output.stderr).trim(), 256),
        });
    }
    let observed = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if let Some(min) = req.minimum_version.as_deref() {
        if observed != "unknown" && compare_versions(&observed, min) == std::cmp::Ordering::Less {
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
        && s.split('.').all(|part| {
            !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.into()
    } else {
        let mut out = s[..max].to_string();
        out.push('…');
        out
    }
}

/// A probe query, run from `/` so that nothing in the caller's working
/// directory can be imported in place of an installed module.
fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<Output> {
    run_bounded(cmd.current_dir("/"), timeout).ok()
}

/// Run `command` to completion with its input closed, or kill and reap it
/// at `timeout`. The error says why there is no output: the command did
/// not start, did not exit in time, or left a pipe open after it exited.
pub fn run_bounded(command: &mut Command, timeout: Duration) -> Result<Output, String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("did not start: {e}"))?;
    // Read while waiting: a child blocked on a full pipe never exits.
    let stdout = read_in_background(child.stdout.take());
    let stderr = read_in_background(child.stderr.take());
    let status = wait_bounded(&mut child, timeout).map_err(|e| format!("did not finish: {e}"))?;
    // A process the command started can outlive it and hold a pipe open.
    let output = |pipe: std::sync::mpsc::Receiver<Vec<u8>>| {
        pipe.recv_timeout(PIPE_CLOSE_GRACE)
            .map_err(|_| "left its output open after it exited".to_string())
    };
    Ok(Output {
        status,
        stdout: output(stdout)?,
        stderr: output(stderr)?,
    })
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
            .map(String::from)
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
            crate::SCHEMA_VERSION,
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
    fn a_runner_profile_package_that_is_not_installed_is_its_own_state() {
        // Something to install, not a descriptor to repair: the reason
        // vocabulary classifies the two apart.
        let descriptor = Path::new("/usr/share/tensorplate/backends/python_pytorch/backend.json");
        let declaration =
            "/usr/share/tensorplate/backends/python_pytorch/runner_profiles.d/kokoro.json";
        let report = report_read(
            Err(BackendDescriptorError::PackageNotInstalled {
                path: declaration.into(),
                profile: "kokoro".into(),
                package: "tensorplate-speech-runtime-cuda".into(),
            }),
            descriptor,
            &ProbeOptions::default(),
        );
        assert_eq!(report.backend_name, "python_pytorch");
        assert_eq!(report.descriptor_path, descriptor);
        assert_eq!(
            report.state,
            BackendProbeState::RunnerProfilePackageMissing {
                profile: "kokoro".into(),
                package: "tensorplate-speech-runtime-cuda".into(),
                declaration: declaration.into(),
            }
        );
        assert_eq!(
            serde_json::to_value(&report.state).unwrap()["state"],
            "runner_profile_package_missing"
        );

        // Every other refusal of a declaration stays a malformed descriptor.
        let report = report_read(
            Err(BackendDescriptorError::DuplicateRunnerProfile {
                id: "kokoro".into(),
                first: descriptor.display().to_string(),
                second: declaration.into(),
            }),
            descriptor,
            &ProbeOptions::default(),
        );
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
                crate::SCHEMA_VERSION
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
        write(
            &p,
            &descriptor_with_python(&stub, Some("tensorplate_pytorch_backend")),
        );
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
                crate::SCHEMA_VERSION
            ),
        );
        let report = probe_backend(&p, &ProbeOptions::default());
        assert!(matches!(report.state, BackendProbeState::Runnable));
    }

    struct EveryPackage;

    impl PackageInventory for EveryPackage {
        fn installed(
            &self,
            names: &std::collections::BTreeSet<&str>,
        ) -> Result<std::collections::BTreeSet<String>, String> {
            Ok(names.iter().map(|name| (*name).to_string()).collect())
        }
    }

    const OWN_VERSION_QUERY: &str = r#"import sys;print(f"{sys.version_info.major}.{sys.version_info.minor}.{sys.version_info.micro}")"#;

    /// A staged install: the descriptor's own interpreter imports the
    /// backend module and not PyTorch, and one runner profile is declared
    /// under `/opt/env` with `profile_interpreter` as its interpreter.
    fn staged_profile(td: &Path, profile_interpreter: Option<&str>, minimum: &str) -> PathBuf {
        make_executable_stub(
            td,
            "own-python",
            &format!(
                "#!/bin/sh\ncase \"$2\" in\n  '{OWN_VERSION_QUERY}') echo 3.12.3; exit 0 ;;\n  'import tensorplate_pytorch_backend') exit 0 ;;\nesac\necho 'ModuleNotFoundError: No module named torch' >&2\nexit 1\n"
            ),
        );
        if let Some(body) = profile_interpreter {
            make_executable_stub(&td.join("opt/env/bin"), "python", body);
        }
        let descriptor = td.join("backend.json");
        write(
            &descriptor,
            &format!(
                r#"{{
                    "schema_version": "{schema}",
                    "backend_name": "python_pytorch",
                    "package_name": "tensorplate-backend-python-pytorch",
                    "package_version": "0.1.0",
                    "python": {{
                        "interpreter": "/own-python",
                        "minimum_version": "{minimum}",
                        "import_module": "tensorplate_pytorch_backend"
                    }},
                    "pytorch": {{"required": true}},
                    "runner_profiles": [{{
                        "id": "faster_whisper",
                        "interpreter": "/opt/env/bin/python",
                        "environment_root": "/opt/env",
                        "library_search_paths": ["/opt/env/lib/a", "/opt/env/lib/b"],
                        "packages": ["speech-runtime"],
                        "compute_types": ["float16"]
                    }}]
                }}"#,
                schema = crate::SCHEMA_VERSION,
            ),
        );
        descriptor
    }

    /// A profile interpreter that records the environment each query is
    /// started with, answers the interpreter query with `version` and exits
    /// with `import_status` for the backend module import.
    fn profile_python(version: &str, import_status: u8) -> String {
        format!(
            "#!/bin/sh\ndir=\"$(dirname \"$0\")\"\ncase \"$2\" in\n  '{INTERPRETER_QUERY_MARKER}'*) env > \"$dir/environment.interpreter\"; echo '{{\"version\": \"{version}\", \"sys_version\": \"{version} (main) [GCC]\", \"prefix\": \"/opt/env\"}}'; exit 0 ;;\n  'import tensorplate_pytorch_backend') env > \"$dir/environment.import\"; echo 'ImportError: no sidecar module' >&2; exit {import_status} ;;\nesac\nexit 1\n"
        )
    }

    fn probe_staged(td: &Path, descriptor: &Path) -> BackendProbeReport {
        probe_backend_with_inventory(
            descriptor,
            &ProbeOptions {
                staged_root: Some(td.to_path_buf()),
                ..ProbeOptions::default()
            },
            &EveryPackage,
        )
    }

    #[test]
    fn a_runner_profile_is_probed_in_its_own_interpreter_not_the_descriptors() {
        let td = TempDir::new().unwrap();
        let descriptor = staged_profile(td.path(), Some(&profile_python("3.12.3", 0)), "3.10");
        let report = probe_staged(td.path(), &descriptor);

        // The descriptor's interpreter has no PyTorch; the profile's needs none.
        assert!(matches!(
            report.state,
            BackendProbeState::PytorchMissing { .. }
        ));
        assert!(!report.is_runnable());
        let [probe] = report.runner_profiles.as_slice() else {
            panic!("one profile expected: {:?}", report.runner_profiles);
        };
        assert_eq!(probe.state, BackendProbeState::Runnable);
        assert_eq!(probe.profile.interpreter, "/opt/env/bin/python");
        assert_eq!(
            probe.observed,
            Some(ObservedInterpreter {
                version: "3.12.3".into(),
                sys_version: "3.12.3 (main) [GCC]".into(),
                prefix: "/opt/env".into(),
            })
        );

        assert_eq!(
            report.serving_state(Some("faster_whisper")),
            ServingState::Probed(&BackendProbeState::Runnable)
        );
        assert_eq!(
            report.serving_state(None),
            ServingState::Probed(&report.state)
        );
        assert_eq!(
            report.serving_state(Some("kokoro")),
            ServingState::RunnerProfileNotInstalled
        );
    }

    #[test]
    fn a_runner_profile_is_probed_in_the_environment_the_launcher_gives_its_sidecar() {
        let td = TempDir::new().unwrap();
        let descriptor = staged_profile(td.path(), Some(&profile_python("3.12.3", 0)), "3.10");
        let _ = probe_staged(td.path(), &descriptor);

        let temp_dir = launcher_temp_dir(|name| std::env::var(name).ok());
        for query in ["interpreter", "import"] {
            let log = td.path().join(format!("opt/env/bin/environment.{query}"));
            let environment = fs::read_to_string(log).unwrap();
            for expected in [
                "LD_LIBRARY_PATH=/opt/env/lib/a:/opt/env/lib/b".to_string(),
                "ORT_DISABLE_TELEMETRY=1".to_string(),
                format!("TMPDIR={}", temp_dir.display()),
            ] {
                assert!(
                    environment.lines().any(|line| line == expected),
                    "{query}: `{expected}` missing from:\n{environment}"
                );
            }
        }
    }

    #[test]
    fn a_runner_profile_interpreter_that_cannot_start_is_missing() {
        let not_executable = TempDir::new().unwrap();
        let descriptor = staged_profile(not_executable.path(), None, "3.10");
        write(
            &not_executable.path().join("opt/env/bin/python"),
            "#!/bin/sh\n",
        );
        let absent = TempDir::new().unwrap();
        let absent_descriptor = staged_profile(absent.path(), None, "3.10");
        // What a failing interpreter printed is not an observation.
        let failing = TempDir::new().unwrap();
        let failing_descriptor = staged_profile(
            failing.path(),
            Some("#!/bin/sh\necho '{\"version\": \"3.12.3\", \"sys_version\": \"3.12.3\", \"prefix\": \"/opt/env\"}'\nexit 1\n"),
            "3.10",
        );

        for (td, descriptor) in [
            (&not_executable, &descriptor),
            (&absent, &absent_descriptor),
            (&failing, &failing_descriptor),
        ] {
            let report = probe_staged(td.path(), descriptor);
            assert_eq!(
                report.runner_profiles[0].state,
                BackendProbeState::PythonInterpreterMissing {
                    interpreter: "/opt/env/bin/python".into()
                }
            );
            assert_eq!(report.runner_profiles[0].observed, None);
        }
    }

    #[test]
    fn a_runner_profile_interpreter_is_held_to_the_descriptors_python_requirements() {
        let old = TempDir::new().unwrap();
        let descriptor = staged_profile(old.path(), Some(&profile_python("3.9.18", 0)), "3.10");
        let report = probe_staged(old.path(), &descriptor);
        assert_eq!(
            report.runner_profiles[0].state,
            BackendProbeState::PythonVersionMismatch {
                interpreter: "/opt/env/bin/python".into(),
                observed: "3.9.18".into(),
                required: "3.10".into(),
            }
        );
        assert!(report.runner_profiles[0].observed.is_some());
        // Too old is the answer whether or not the module would import there.
        assert!(!old.path().join("opt/env/bin/environment.import").exists());

        let no_module = TempDir::new().unwrap();
        let descriptor =
            staged_profile(no_module.path(), Some(&profile_python("3.12.3", 1)), "3.10");
        let report = probe_staged(no_module.path(), &descriptor);
        assert_eq!(
            report.runner_profiles[0].state,
            BackendProbeState::PythonModuleImportFailed {
                module: "tensorplate_pytorch_backend".into(),
                detail: "ImportError: no sidecar module".into(),
            }
        );
        assert_eq!(
            report.serving_state(Some("faster_whisper")),
            ServingState::Probed(&report.runner_profiles[0].state)
        );
    }

    #[test]
    fn a_backend_wide_refusal_decides_every_bundle_and_probes_no_profile() {
        let td = TempDir::new().unwrap();
        let descriptor = staged_profile(td.path(), Some(&profile_python("3.12.3", 0)), "3.10");
        let text = fs::read_to_string(&descriptor).unwrap().replacen(
            "\"python\":",
            "\"tensorplate_runtime_range\": {\"min\": \"9.9.9\"}, \"python\":",
            1,
        );
        write(&descriptor, &text);
        let report = probe_staged(td.path(), &descriptor);

        assert!(matches!(
            report.state,
            BackendProbeState::RuntimeVersionMismatch { .. }
        ));
        assert!(report.runner_profiles.is_empty());
        assert!(!td
            .path()
            .join("opt/env/bin/environment.interpreter")
            .exists());
        for bundle in [None, Some("faster_whisper"), Some("kokoro")] {
            assert_eq!(
                report.serving_state(bundle),
                ServingState::Probed(&report.state)
            );
        }
    }

    #[test]
    fn only_descriptor_and_runtime_range_states_are_backend_wide() {
        use BackendProbeState as S;
        let text = String::new;
        let wide = [
            S::DescriptorMissing,
            S::DescriptorMalformed { reason: text() },
            S::RunnerProfilePackageMissing {
                profile: text(),
                package: text(),
                declaration: text(),
            },
            S::RuntimeVersionMismatch {
                runtime_version: text(),
                descriptor_min: text(),
            },
        ];
        let per_interpreter = [
            S::Runnable,
            S::PythonInterpreterMissing {
                interpreter: text(),
            },
            S::PythonVersionMismatch {
                interpreter: text(),
                observed: text(),
                required: text(),
            },
            S::PythonModuleImportFailed {
                module: text(),
                detail: text(),
            },
            S::PytorchMissing { detail: text() },
            S::PytorchVersionMismatch {
                observed: text(),
                required: text(),
            },
        ];
        assert!(wide.iter().all(S::is_backend_wide));
        assert!(!per_interpreter.iter().any(S::is_backend_wide));
    }

    #[test]
    fn a_command_that_does_not_finish_is_killed_at_the_limit() {
        let td = TempDir::new().unwrap();
        let pid_file = td.path().join("pid");
        let started = std::time::Instant::now();
        let result = run_bounded(
            Command::new("sh")
                .arg("-c")
                .arg("echo $$ > \"$0\"; exec sleep 30")
                .arg(&pid_file),
            Duration::from_millis(300),
        );
        assert_eq!(result.unwrap_err(), "did not finish: no exit within 300ms");
        assert!(started.elapsed() < Duration::from_secs(10));
        let pid = fs::read_to_string(&pid_file).unwrap();
        assert!(
            !Path::new("/proc").join(pid.trim()).exists(),
            "the command is still running"
        );
    }

    #[test]
    fn a_process_the_command_leaves_holding_its_output_does_not_hold_the_caller() {
        let started = std::time::Instant::now();
        let result = run_bounded(
            Command::new("sh").arg("-c").arg("sleep 20 & exit 0"),
            Duration::from_secs(30),
        );
        assert_eq!(result.unwrap_err(), "left its output open after it exited");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_finished_command_returns_its_status_and_both_streams() {
        let output = run_bounded(
            Command::new("sh")
                .arg("-c")
                .arg("echo out; echo err >&2; exit 3"),
            Duration::from_secs(30),
        )
        .unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(
            (output.stdout.as_slice(), output.stderr.as_slice()),
            (&b"out\n"[..], &b"err\n"[..])
        );
        assert!(run_bounded(
            &mut Command::new("/nonexistent/python"),
            Duration::from_secs(1)
        )
        .unwrap_err()
        .starts_with("did not start: "));
    }

    #[test]
    fn an_interpreter_that_does_not_answer_in_time_is_missing_and_does_not_hold_the_probe() {
        let td = TempDir::new().unwrap();
        let descriptor = staged_profile(td.path(), Some("#!/bin/sh\nexec sleep 30\n"), "3.10");
        let started = std::time::Instant::now();
        let report = probe_backend_with_inventory(
            &descriptor,
            &ProbeOptions {
                staged_root: Some(td.path().to_path_buf()),
                timeout: Duration::from_millis(300),
                ..ProbeOptions::default()
            },
            &EveryPackage,
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(
            report.runner_profiles[0].state,
            BackendProbeState::PythonInterpreterMissing {
                interpreter: "/opt/env/bin/python".into()
            }
        );
    }

    #[test]
    fn a_framework_import_is_given_longer_than_the_other_queries() {
        let td = TempDir::new().unwrap();
        let python = make_executable_stub(
            td.path(),
            "python",
            &format!(
                "#!/bin/sh\ncase \"$2\" in\n  '{OWN_VERSION_QUERY}') echo 3.12.3 ;;\n  'import torch;'*) sleep 1; echo 2.9.0 ;;\n  *) exit 1 ;;\nesac\n"
            ),
        );
        let descriptor = td.path().join("backend.json");
        write(
            &descriptor,
            &descriptor_with_python(&python, None).replacen(
                "\"python\":",
                "\"pytorch\": {\"required\": true}, \"python\":",
                1,
            ),
        );
        let report = probe_backend(
            &descriptor,
            &ProbeOptions {
                timeout: Duration::from_millis(200),
                ..ProbeOptions::default()
            },
        );
        assert_eq!(report.state, BackendProbeState::Runnable);
    }

    #[test]
    fn a_probe_query_runs_from_the_root_directory() {
        let td = TempDir::new().unwrap();
        let descriptor = staged_profile(
            td.path(),
            Some(&format!(
                "#!/bin/sh\npwd > \"$(dirname \"$0\")/cwd\"\n{}",
                profile_python("3.12.3", 0).trim_start_matches("#!/bin/sh\n")
            )),
            "3.10",
        );
        let _ = probe_staged(td.path(), &descriptor);
        assert_eq!(
            fs::read_to_string(td.path().join("opt/env/bin/cwd")).unwrap(),
            "/\n"
        );
    }

    /// The query as a real interpreter answers it, where this host has one.
    #[test]
    fn the_interpreter_query_runs_in_a_real_python() {
        let python = Path::new("/usr/bin/python3");
        if !python.exists() {
            eprintln!("skipped: no /usr/bin/python3 on this host");
            return;
        }
        let observed = observe_interpreter(python, &[], Duration::from_secs(30)).unwrap();
        let ask = |code: &str| {
            let output = run_bounded(
                Command::new(python).arg("-c").arg(code),
                Duration::from_secs(30),
            )
            .unwrap();
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        };
        assert_eq!(observed.prefix, ask("import sys; print(sys.prefix)"));
        assert_eq!(
            observed.version,
            ask("import sys; print('.'.join(map(str, sys.version_info[:3])))")
        );
        assert_eq!(
            observed.sys_version.replace('\n', " "),
            ask("import sys; print(sys.version.replace(chr(10), ' '))")
        );

        // In a virtual environment the prefix is the environment, not the
        // interpreter it was made from. Not every host can make one.
        let td = TempDir::new().unwrap();
        let environment = td.path().join("env");
        let made = run_bounded(
            Command::new(python)
                .args(["-m", "venv", "--without-pip"])
                .arg(&environment),
            Duration::from_secs(60),
        );
        if !made.is_ok_and(|output| output.status.success()) {
            eprintln!("skipped the virtual environment: this host cannot make one");
            return;
        }
        let inside = observe_interpreter(
            &environment.join("bin/python"),
            &[],
            Duration::from_secs(30),
        )
        .unwrap();
        assert_eq!(
            fs::canonicalize(&inside.prefix).unwrap(),
            fs::canonicalize(&environment).unwrap()
        );
        assert_ne!(inside.prefix, observed.prefix);
    }

    #[test]
    fn the_interpreter_query_starts_with_its_marker() {
        assert!(INTERPRETER_QUERY.starts_with(&format!("{INTERPRETER_QUERY_MARKER}\n")));
    }

    #[test]
    fn the_launcher_temp_dir_is_the_first_variable_set_and_tmp_otherwise() {
        let set = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_string())
            }
        };
        assert_eq!(launcher_temp_dir(set(&[])), PathBuf::from("/tmp"));
        assert_eq!(
            launcher_temp_dir(set(&[("TEMPDIR", "/d")])),
            PathBuf::from("/d")
        );
        assert_eq!(
            launcher_temp_dir(set(&[("TEMPDIR", "/d"), ("TEMP", "/c")])),
            PathBuf::from("/c")
        );
        assert_eq!(
            launcher_temp_dir(set(&[("TEMPDIR", "/d"), ("TEMP", "/c"), ("TMP", "/b")])),
            PathBuf::from("/b")
        );
        assert_eq!(
            launcher_temp_dir(set(&[("TMP", "/b"), ("TMPDIR", "/a")])),
            PathBuf::from("/a")
        );
        // Set and empty is still the launcher's answer, which it then refuses.
        assert_eq!(
            launcher_temp_dir(set(&[("TMPDIR", ""), ("TMP", "/b")])),
            PathBuf::from("")
        );
    }

    #[test]
    fn version_compare_handles_semver_like() {
        assert!(compare_versions("0.1.0", "0.2.0").is_lt());
        assert!(compare_versions("2.0", "1.9.99").is_gt());
        assert!(compare_versions("0.1.0", "0.1.0").is_eq());
        assert!(compare_versions("0.1.0-dev", "0.1.0").is_lt());
    }
}
