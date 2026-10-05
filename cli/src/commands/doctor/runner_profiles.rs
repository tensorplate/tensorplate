// SPDX-License-Identifier: Apache-2.0
//
// packaging: `tensorplate doctor`'s runner profile findings.
//
// A bundle that names a runner profile is served by a sidecar started in
// the interpreter and environment the installed backend descriptor
// declares for that profile. These findings report that record as the
// serving worker's launcher reads it and check it where the sidecar would
// start: the interpreter, what the profile's engines need to import and
// load, and the temporary directory the launcher hands the sidecar.
//
// Doctor runs in the operator's shell, not in the agent's unit. What the
// agent's environment adds is read from the unit's `EnvironmentFile`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;
use tensorplate_protocol::backend_descriptor::{BackendDescriptor, RunnerProfile};
use tensorplate_protocol::backend_probe::{
    launcher_temp_dir, run_bounded, runner_launch_environment, staged, BackendProbeReport,
    BackendProbeState, ProbeOptions, RunnerProfileProbe,
};
use tensorplate_protocol::install_paths::{BACKEND_DESCRIPTOR_DIR, BACKEND_DESCRIPTOR_DIR_ENV};

use super::finding::{Finding, FindingId, Severity};

/// The agent unit's `EnvironmentFile`.
pub(super) const AGENT_ENVIRONMENT_FILE: &str = "/etc/default/tensorplate-agent";

const DEPENDENCY_SCRIPT: &str = include_str!("runner_profile_dependencies.py");

/// PyTorch alone takes seconds to import from a cold page cache.
const DEPENDENCY_TIMEOUT: Duration = Duration::from_secs(120);

const REINSTALL_HINT: &str =
    "reinstall the packages the profile's declaration names, then restart tensorplate-agent";

/// The variables the agent unit's `EnvironmentFile` sets.
#[derive(Debug, Default)]
pub(super) struct AgentEnvironment {
    variables: BTreeMap<String, String>,
    /// Why the file could not be read, when it exists and could not.
    unreadable: Option<String>,
}

impl AgentEnvironment {
    /// A missing file sets nothing, as for the unit. One that is there and
    /// cannot be read may set anything.
    pub(super) fn read(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => Self {
                unreadable: Some(e.to_string()),
                ..Self::default()
            },
        }
    }

    fn parse(text: &str) -> Self {
        let unquote = |value: &str| {
            let value = value.trim();
            ['"', '\'']
                .into_iter()
                .find_map(|q| value.strip_prefix(q)?.strip_suffix(q))
                .unwrap_or(value)
                .to_string()
        };
        Self {
            variables: text
                .lines()
                .map(str::trim)
                .filter(|line| !line.starts_with(['#', ';']))
                .filter_map(|line| line.split_once('='))
                .map(|(name, value)| (name.trim().to_string(), unquote(value)))
                .collect(),
            unreadable: None,
        }
    }

    pub(super) fn get(&self, name: &str) -> Option<&str> {
        self.variables.get(name).map(String::as_str)
    }
}

/// The launcher starts a bundle that names no runner profile in the
/// interpreter these variables name, when the agent's environment sets one.
const OWN_INTERPRETER_VARIABLES: [&str; 3] = [
    "TP_PYTHON_PYTORCH_EXECUTABLE",
    "TP_TEST_PYTHON_EXE",
    "TP_TEST_PYTHON",
];

pub(super) fn own_interpreter_override(
    descriptor: &BackendDescriptor,
    agent_environment: &AgentEnvironment,
) -> Option<String> {
    let (name, value) = OWN_INTERPRETER_VARIABLES.into_iter().find_map(|name| {
        let value = agent_environment.get(name)?;
        (!value.is_empty()).then_some((name, value))
    })?;
    let probed = descriptor.python.as_ref()?.interpreter.as_deref()?;
    (value != probed).then(|| {
        format!(
            "; note: `{name}={value}` in `{AGENT_ENVIRONMENT_FILE}` starts a bundle that names no runner profile in that interpreter, which this check did not run (it ran `{probed}`)"
        )
    })
}

/// What the runner profile findings are computed from.
pub(super) struct Inputs<'a> {
    pub report: &'a BackendProbeReport,
    pub probe: &'a ProbeOptions,
    /// Installed version of each package a profile names.
    pub versions: &'a BTreeMap<String, String>,
    pub agent_environment: &'a AgentEnvironment,
    /// The descriptor directory `report` was read from, as installed.
    pub descriptor_dir: &'a Path,
    /// Whether the agent runs under the packaged systemd unit.
    pub systemd: bool,
    /// `/proc/self/mountinfo`, when it could be read.
    pub mountinfo: Option<&'a str>,
}

/// The three findings where no runner profile can be probed.
pub(super) fn skipped(reason: &str) -> Vec<Finding> {
    [
        FindingId::RunnerProfiles,
        FindingId::RunnerProfileDependencies,
        FindingId::RunnerLaunchEnvironment,
    ]
    .into_iter()
    .map(|id| Finding::skipped(id, Severity::Info, format!("skipped: {reason}"), None))
    .collect()
}

pub(super) fn findings(inputs: &Inputs<'_>) -> Vec<Finding> {
    vec![
        record_finding(inputs),
        dependency_finding(inputs),
        match &inputs.agent_environment.unreadable {
            Some(why) => unread_environment_finding(why),
            None => launch_environment_finding(inputs),
        },
    ]
}

/// The other findings assumed the packaged defaults for what this file may set.
fn unread_environment_finding(why: &str) -> Finding {
    Finding::warn(
        FindingId::RunnerLaunchEnvironment,
        Severity::Warning,
        format!(
            "could not read `{AGENT_ENVIRONMENT_FILE}` ({why}): the temporary directory, backend descriptor directory and interpreter override it may set for the agent are not known to this report, which assumed none"
        ),
        Some(format!(
            "run `tensorplate doctor` as a user who can read `{AGENT_ENVIRONMENT_FILE}`"
        )),
    )
}

fn sidecar_temp_dir(environment: &AgentEnvironment) -> PathBuf {
    launcher_temp_dir(|name| environment.get(name).map(str::to_string))
}

fn record_finding(inputs: &Inputs<'_>) -> Finding {
    let profiles = &inputs.report.runner_profiles;
    let mut faults = Vec::new();
    let agent_dir = inputs
        .agent_environment
        .get(BACKEND_DESCRIPTOR_DIR_ENV)
        .filter(|dir| !dir.is_empty())
        .unwrap_or(BACKEND_DESCRIPTOR_DIR);
    if inputs.systemd && Path::new(agent_dir) != inputs.descriptor_dir {
        faults.push(format!(
            "the agent's environment (`{AGENT_ENVIRONMENT_FILE}`) reads backend descriptors from `{agent_dir}`, and this report read `{}`",
            inputs.descriptor_dir.display()
        ));
    }
    faults.extend(profiles.iter().filter_map(|probe| {
        interpreter_fault(probe, inputs.probe)
            .map(|fault| format!("runner profile `{}`: {fault}", probe.profile.id))
    }));
    if !faults.is_empty() {
        return Finding::fail(
            FindingId::RunnerProfiles,
            Severity::Critical,
            faults.join("; "),
            Some(REINSTALL_HINT.into()),
        );
    }
    let records: Vec<String> = profiles
        .iter()
        .map(|probe| record(probe, inputs.versions))
        .collect();
    Finding::ok(
        FindingId::RunnerProfiles,
        Severity::Info,
        format!(
            "{} installed, as the sidecar launcher reads {}: {}",
            count(profiles.len(), "runner profile"),
            if profiles.len() == 1 { "it" } else { "them" },
            records.join("; ")
        ),
        None,
    )
}

fn count(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

/// Why the launcher would not start this profile's sidecar in the declared
/// environment, if it would not.
fn interpreter_fault(probe: &RunnerProfileProbe, opts: &ProbeOptions) -> Option<String> {
    use BackendProbeState as S;
    let profile = &probe.profile;
    match &probe.state {
        S::Runnable => {}
        S::PythonInterpreterMissing { .. } => {
            return Some(format!(
                "interpreter `{}` is not an executable file that runs",
                profile.interpreter
            ))
        }
        S::PythonVersionMismatch {
            observed, required, ..
        } => {
            return Some(format!(
                "interpreter `{}` is Python {observed}; the backend needs {required}",
                profile.interpreter
            ))
        }
        S::PythonModuleImportFailed { module, detail } => {
            return Some(format!(
                "`{module}` does not import in `{}`: {detail}",
                profile.interpreter
            ))
        }
        // Never a profile's state: they belong to the descriptor.
        S::DescriptorMissing
        | S::DescriptorMalformed { .. }
        | S::RunnerProfilePackageMissing { .. }
        | S::RuntimeVersionMismatch { .. }
        | S::PytorchMissing { .. }
        | S::PytorchVersionMismatch { .. } => return Some("is not runnable".into()),
    }
    // The dynamic loader splits its variable on both characters.
    if let Some(dir) = profile
        .library_search_paths
        .iter()
        .find(|dir| dir.contains([':', ';']))
    {
        return Some(format!(
            "library search path `{dir}` holds `:` or `;`, which the launcher refuses"
        ));
    }
    let observed = probe.observed.as_ref()?;
    (!inside(&observed.prefix, &profile.environment_root, opts)
        || !inside(&profile.environment_root, &observed.prefix, opts))
    .then(|| {
        format!(
            "interpreter `{}` runs in `{}`, not in the declared environment root `{}`",
            profile.interpreter, observed.prefix, profile.environment_root
        )
    })
}

/// Whether `path` is `root` or below it, as declared or as resolved.
fn inside(path: &str, root: &str, opts: &ProbeOptions) -> bool {
    let resolved = |p: &str| std::fs::canonicalize(staged(opts, p)).ok();
    Path::new(path).starts_with(root)
        || matches!((resolved(path), resolved(root)), (Some(p), Some(r)) if p.starts_with(&r))
}

fn record(probe: &RunnerProfileProbe, versions: &BTreeMap<String, String>) -> String {
    let profile = &probe.profile;
    let python = probe.observed.as_ref().map_or_else(
        || "not observed".to_string(),
        |o| {
            o.sys_version
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        },
    );
    let packages: Vec<String> = profile
        .packages
        .iter()
        .map(|name| {
            versions.get(name).map_or_else(
                || format!("{name} (version not read)"),
                |version| format!("{name} {version}"),
            )
        })
        .collect();
    format!(
        "`{}`: interpreter `{}` (Python {python}), environment root `{}`, compute types {}, packages {}",
        profile.id,
        profile.interpreter,
        profile.environment_root,
        compute_types(profile).join(", "),
        packages.join(", ")
    )
}

fn compute_types(profile: &RunnerProfile) -> Vec<String> {
    profile
        .compute_types
        .iter()
        .filter_map(|t| serde_json::to_value(t).ok()?.as_str().map(str::to_string))
        .collect()
}

/// What a shipped profile's engines import and load by name at run time.
struct Requirements {
    modules: &'static [&'static str],
    libraries: &'static [&'static str],
}

fn requirements(profile_id: &str) -> Option<Requirements> {
    match profile_id {
        "faster_whisper" => Some(Requirements {
            modules: &["ctranslate2", "faster_whisper", "av"],
            libraries: &["libcublas.so.12"],
        }),
        "kokoro" => Some(Requirements {
            modules: &[
                "torch",
                "kokoro",
                "misaki",
                "en_core_web_sm",
                "espeakng_loader",
            ],
            libraries: &[],
        }),
        _ => None,
    }
}

/// What `runner_profile_dependencies.py` prints.
#[derive(Debug, Default, Deserialize)]
struct DependencyFacts {
    #[serde(default)]
    modules: BTreeMap<String, Outcome>,
    #[serde(default)]
    libraries: BTreeMap<String, Outcome>,
    ctranslate2: Option<Outcome>,
    torch: Option<Outcome>,
    espeak_ng: Option<Outcome>,
}

#[derive(Debug, Default, Deserialize)]
struct Outcome {
    error: Option<String>,
    version: Option<String>,
    mapped: Option<Vec<String>>,
    cuda_devices: Option<u64>,
    compute_types: Option<Vec<String>>,
    library: Option<String>,
}

#[derive(Debug, Default, PartialEq)]
struct Verdict {
    faults: Vec<String>,
    warnings: Vec<String>,
    facts: Vec<String>,
}

fn dependency_finding(inputs: &Inputs<'_>) -> Finding {
    let temp_dir = sidecar_temp_dir(inputs.agent_environment);
    let verdicts: Vec<(&str, Verdict)> = inputs
        .report
        .runner_profiles
        .iter()
        .map(|probe| {
            let profile = &probe.profile;
            let verdict = match requirements(&profile.id) {
                _ if probe.state != BackendProbeState::Runnable => Verdict {
                    faults: vec!["not checked: its interpreter does not run".into()],
                    ..Verdict::default()
                },
                None => Verdict {
                    facts: vec![
                        "no dependency list for this profile; only its interpreter was checked"
                            .into(),
                    ],
                    ..Verdict::default()
                },
                Some(needs) => {
                    match observe_dependencies(profile, &needs, &temp_dir, inputs.probe) {
                        Ok(facts) => evaluate(profile, &needs, &facts, inputs.probe),
                        Err(fault) => Verdict {
                            faults: vec![fault],
                            ..Verdict::default()
                        },
                    }
                }
            };
            (profile.id.as_str(), verdict)
        })
        .collect();
    dependency_summary(&verdicts)
}

/// One finding over every profile's verdict: a fault in any profile fails
/// it, and a warning in any, with no fault, makes it a warning.
fn dependency_summary(verdicts: &[(&str, Verdict)]) -> Finding {
    let mut worst = Severity::Info;
    let mut lines = Vec::new();
    for (id, verdict) in verdicts {
        let (severity, said) = if !verdict.faults.is_empty() {
            (Severity::Critical, &verdict.faults)
        } else if !verdict.warnings.is_empty() {
            (Severity::Warning, &verdict.warnings)
        } else {
            (Severity::Info, &verdict.facts)
        };
        if severity == Severity::Critical || worst == Severity::Info {
            worst = severity;
        }
        lines.push(format!("`{id}`: {}", said.join(", ")));
    }
    let message = lines.join("; ");
    let id = FindingId::RunnerProfileDependencies;
    match worst {
        Severity::Critical => Finding::fail(
            id,
            worst,
            message,
            Some(format!(
                "{REINSTALL_HINT}; a host with no NVIDIA driver shows no CUDA device (see `cuda_runtime`)"
            )),
        ),
        Severity::Warning => Finding::warn(id, worst, message, None),
        Severity::Info => Finding::ok(id, worst, message, None),
    }
}

/// The dependency script's one argument: what to import and load.
fn query_spec(needs: &Requirements) -> String {
    serde_json::json!({"modules": needs.modules, "libraries": needs.libraries}).to_string()
}

fn observe_dependencies(
    profile: &RunnerProfile,
    needs: &Requirements,
    temp_dir: &Path,
    opts: &ProbeOptions,
) -> Result<DependencyFacts, String> {
    let mut command = Command::new(staged(opts, &profile.interpreter));
    command
        .arg("-c")
        .arg(DEPENDENCY_SCRIPT)
        .arg(query_spec(needs))
        .envs(runner_launch_environment(profile, temp_dir))
        // Not the caller's directory: `-c` puts it first on the module path.
        .current_dir("/");
    let output = run_bounded(&mut command, DEPENDENCY_TIMEOUT)
        .map_err(|e| format!("the dependency check {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "the dependency check failed ({}): {}",
            output.status,
            stderr.lines().last().unwrap_or_default()
        ));
    }
    // An engine may print while it is imported; the facts are the last line.
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .last()
        .and_then(|line| serde_json::from_str(line).ok())
        .ok_or_else(|| "the dependency check printed no facts".to_string())
}

fn evaluate(
    profile: &RunnerProfile,
    needs: &Requirements,
    facts: &DependencyFacts,
    opts: &ProbeOptions,
) -> Verdict {
    let mut verdict = Verdict::default();
    let imported = |name: &str| facts.modules.get(name).is_some_and(|m| m.error.is_none());
    for name in needs.modules {
        match facts.modules.get(*name) {
            Some(Outcome { error: Some(e), .. }) => verdict
                .faults
                .push(format!("`{name}` does not import: {e}")),
            Some(Outcome {
                version: Some(v), ..
            }) if v == "unknown" => verdict.facts.push((*name).to_string()),
            Some(Outcome {
                version: Some(v), ..
            }) => verdict.facts.push(format!("{name} {v}")),
            _ => verdict.faults.push(format!("`{name}` was not checked")),
        }
    }
    for soname in needs.libraries {
        match facts.libraries.get(*soname) {
            Some(Outcome { error: Some(e), .. }) => verdict.faults.push(format!(
                "`{soname}` does not load with the profile's library search path: {e}"
            )),
            Some(Outcome {
                mapped: Some(mapped),
                ..
            }) if !mapped.is_empty() => {
                for path in mapped {
                    if inside(path, &profile.environment_root, opts) {
                        verdict.facts.push(format!("`{soname}` from `{path}`"));
                    } else {
                        verdict.faults.push(format!(
                            "`{soname}` resolves to `{path}`, outside the environment root `{}`: the sidecar would run on a library the profile does not ship",
                            profile.environment_root
                        ));
                    }
                }
            }
            _ => verdict.faults.push(format!("`{soname}` was not checked")),
        }
    }
    if imported("ctranslate2") {
        let declared = compute_types(profile);
        match facts.ctranslate2.as_ref() {
            Some(Outcome { error: Some(e), .. }) => verdict
                .faults
                .push(format!("CTranslate2 could not query CUDA: {e}")),
            Some(Outcome {
                cuda_devices: Some(devices),
                compute_types,
                ..
            }) => {
                let supported = compute_types.as_deref().unwrap_or_default();
                let unsupported: Vec<&str> = declared
                    .iter()
                    .filter(|t| !supported.contains(t))
                    .map(String::as_str)
                    .collect();
                if *devices == 0 {
                    verdict.faults.push(format!(
                        "CTranslate2 sees no CUDA device, so compute types {} cannot be loaded",
                        declared.join(", ")
                    ));
                } else if unsupported.is_empty() {
                    verdict.facts.push(format!(
                        "CTranslate2 sees {} and supports {}",
                        count(
                            usize::try_from(*devices).unwrap_or(usize::MAX),
                            "CUDA device"
                        ),
                        declared.join(", ")
                    ));
                } else {
                    verdict.faults.push(format!(
                        "declared compute types {} are not supported on this host's CUDA device (supported: {})",
                        unsupported.join(", "),
                        supported.join(", ")
                    ));
                }
            }
            _ => verdict
                .faults
                .push("CTranslate2's CUDA support was not checked".into()),
        }
    }
    if imported("torch") {
        match facts.torch.as_ref() {
            Some(Outcome {
                cuda_devices: Some(0),
                ..
            }) => verdict.warnings.push(
                "PyTorch sees no CUDA device: a bundle whose runner entry selects `cuda` will not load".into(),
            ),
            Some(Outcome {
                cuda_devices: Some(devices),
                ..
            }) => verdict.facts.push(format!(
                "PyTorch sees {}",
                count(usize::try_from(*devices).unwrap_or(usize::MAX), "CUDA device")
            )),
            Some(Outcome { error: Some(e), .. }) => verdict
                .faults
                .push(format!("PyTorch could not query CUDA: {e}")),
            _ => verdict.faults.push("PyTorch's CUDA support was not checked".into()),
        }
    }
    if imported("espeakng_loader") {
        match facts.espeak_ng.as_ref() {
            Some(Outcome {
                version: Some(version),
                library: Some(library),
                ..
            }) => verdict
                .facts
                .push(format!("espeak-ng {version} from `{library}`")),
            Some(Outcome { error: Some(e), .. }) => {
                verdict.faults.push(format!("espeak-ng does not load: {e}"));
            }
            _ => verdict.faults.push("espeak-ng was not checked".into()),
        }
    }
    verdict
}

fn launch_environment_finding(inputs: &Inputs<'_>) -> Finding {
    let id = FindingId::RunnerLaunchEnvironment;
    let environment = inputs.agent_environment;
    let temp_dir = sidecar_temp_dir(environment);
    let origin = ["TMPDIR", "TMP", "TEMP", "TEMPDIR"]
        .into_iter()
        .find(|name| environment.get(name).is_some())
        .map_or_else(
            || "the agent unit's private `/tmp`".to_string(),
            |name| format!("`{name}` in `{AGENT_ENVIRONMENT_FILE}`"),
        );
    let launcher_refuses = "the launcher refuses every runner profile";
    let refused = |why: &str| {
        Finding::fail(
            id,
            Severity::Critical,
            format!(
                "the sidecar temporary directory `{}` ({origin}) {why}",
                temp_dir.display()
            ),
            Some(format!(
                "set `TMPDIR` in `{AGENT_ENVIRONMENT_FILE}` to a directory the agent can write to on a filesystem that allows execution, then restart tensorplate-agent"
            )),
        )
    };
    if temp_dir.as_os_str().is_empty() {
        return refused(&format!("is empty: {launcher_refuses}"));
    }
    // The worker resolves a relative one against its own working directory.
    if !temp_dir.is_absolute() {
        return refused(
            "is not an absolute path, so the directory the sidecar would get cannot be checked",
        );
    }
    let on_disk = staged(inputs.probe, &temp_dir.display().to_string());
    if !on_disk.is_dir() {
        return refused(&format!("is not a directory: {launcher_refuses}"));
    }
    let resolved = if inputs.probe.staged_root.is_some() {
        temp_dir.clone()
    } else {
        std::fs::canonicalize(&on_disk).unwrap_or(on_disk)
    };
    let Some((mount_point, options)) = inputs
        .mountinfo
        .and_then(|text| mount_options(text, &resolved))
    else {
        return Finding::warn(
            id,
            Severity::Warning,
            format!(
                "could not read how the filesystem of the sidecar temporary directory `{}` ({origin}) is mounted; the launcher refuses a `noexec` one",
                temp_dir.display()
            ),
            None,
        );
    };
    if options.iter().any(|option| option == "noexec") {
        return refused(&format!(
            "is on `{mount_point}`, which is mounted `noexec`: {launcher_refuses}"
        ));
    }
    let variables: Vec<String> = inputs
        .report
        .runner_profiles
        .iter()
        .map(|probe| {
            let set: Vec<String> = runner_launch_environment(&probe.profile, &temp_dir)
                .into_iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect();
            format!("`{}` {}", probe.profile.id, set.join(" "))
        })
        .collect();
    Finding::ok(
        id,
        Severity::Info,
        format!(
            "sidecar temporary directory `{}` ({origin}; `{mount_point}` allows execution as mounted for this shell); the launcher sets, over the agent's environment: {}",
            temp_dir.display(),
            variables.join("; ")
        ),
        None,
    )
}

/// The mount point holding `path` and its options, from `mountinfo` text.
/// The longest mount point wins, and the later of two mounts on one point.
fn mount_options(mountinfo: &str, path: &Path) -> Option<(String, Vec<String>)> {
    let unescape = |field: &str| {
        field
            .replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\134", "\\")
    };
    mountinfo
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(' ').skip(4);
            let point = unescape(fields.next()?);
            let options = fields.next()?.split(',').map(str::to_string).collect();
            path.starts_with(&point).then_some((point, options))
        })
        .fold(
            None,
            |best: Option<(String, Vec<String>)>, found| match best {
                Some(best) if best.0.len() > found.0.len() => Some(best),
                _ => Some(found),
            },
        )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn the_agent_environment_file_is_read_as_systemd_reads_it() {
        let environment = AgentEnvironment::parse(
            "# comment\n; another\n#TP_D=commented\n;TP_E=commented\n\nTMPDIR=/var/lib/tensorplate/tmp\n  TP_A = \"quoted value\" \nTP_B='single'\nnot an assignment\nTP_C=a=b\n",
        );
        assert_eq!(environment.get("TMPDIR"), Some("/var/lib/tensorplate/tmp"));
        assert_eq!(environment.get("TP_A"), Some("quoted value"));
        assert_eq!(environment.get("TP_B"), Some("single"));
        assert_eq!(environment.get("TP_C"), Some("a=b"));
        assert_eq!(environment.get("# comment"), None);
        assert_eq!(environment.variables.len(), 4);
        assert_eq!(
            sidecar_temp_dir(&environment),
            PathBuf::from("/var/lib/tensorplate/tmp")
        );
        assert_eq!(
            sidecar_temp_dir(&AgentEnvironment::default()),
            PathBuf::from("/tmp")
        );
    }

    #[test]
    fn mount_options_are_those_of_the_innermost_and_latest_mount() {
        let mountinfo = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/root rw
30 22 0:26 / /tmp rw,nosuid,nodev shared:5 - tmpfs tmpfs rw
31 22 0:27 / /var/tmp\\040dir rw,noexec - tmpfs tmpfs rw
40 30 0:30 / /tmp rw,nosuid,nodev,noexec shared:9 - tmpfs tmpfs rw
";
        let options = |path: &str| mount_options(mountinfo, Path::new(path)).unwrap();
        assert_eq!(options("/var/lib").0, "/");
        assert!(!options("/var/lib").1.contains(&"noexec".to_string()));
        assert_eq!(options("/tmp/x").0, "/tmp");
        assert!(options("/tmp/x").1.contains(&"noexec".to_string()));
        assert_eq!(options("/var/tmp dir/x").0, "/var/tmp dir");
        // A sibling whose name only starts like the mount point is not on it.
        assert_eq!(options("/tmpfile").0, "/");
        assert_eq!(mount_options("", Path::new("/tmp")), None);
    }

    fn profile(id: &str, compute_types: &[&str], search_paths: &[&str]) -> RunnerProfile {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "interpreter": "/opt/env/bin/python",
            "environment_root": "/opt/env",
            "library_search_paths": search_paths,
            "packages": ["speech-runtime"],
            "compute_types": compute_types,
        }))
        .unwrap()
    }

    fn runnable(profile: RunnerProfile) -> RunnerProfileProbe {
        RunnerProfileProbe {
            profile,
            state: BackendProbeState::Runnable,
            observed: Some(tensorplate_protocol::backend_probe::ObservedInterpreter {
                version: "3.12.3".into(),
                sys_version: "3.12.3 (main) [GCC]".into(),
                prefix: "/opt/env".into(),
            }),
        }
    }

    /// What a host that can serve `faster_whisper` answers, with `ctranslate2`
    /// replaced by `ctranslate2_facts`.
    fn faster_whisper_verdict(ctranslate2_facts: serde_json::Value) -> Verdict {
        let facts: DependencyFacts = serde_json::from_value(serde_json::json!({
            "modules": {
                "ctranslate2": {"version": "4.8.2"},
                "faster_whisper": {"version": "1.2.1"},
                "av": {"version": "18.1.0"},
            },
            "libraries": {"libcublas.so.12": {"mapped": ["/opt/env/lib/libcublas.so.12"]}},
            "ctranslate2": ctranslate2_facts,
        }))
        .unwrap();
        evaluate(
            &profile("faster_whisper", &["float16", "int8_float16"], &[]),
            &requirements("faster_whisper").unwrap(),
            &facts,
            &ProbeOptions::default(),
        )
    }

    #[test]
    fn a_declared_compute_type_the_cuda_device_does_not_support_is_a_fault() {
        let supported = faster_whisper_verdict(serde_json::json!({
            "cuda_devices": 1,
            "compute_types": ["float16", "float32", "int8_float16"],
        }));
        assert_eq!(supported.faults, Vec::<String>::new());
        assert_eq!(supported.warnings, Vec::<String>::new());
        assert_eq!(
            supported.facts,
            [
                "ctranslate2 4.8.2",
                "faster_whisper 1.2.1",
                "av 18.1.0",
                "`libcublas.so.12` from `/opt/env/lib/libcublas.so.12`",
                "CTranslate2 sees 1 CUDA device and supports float16, int8_float16",
            ]
        );

        let one_missing = faster_whisper_verdict(serde_json::json!({
            "cuda_devices": 2,
            "compute_types": ["float32", "int8_float16"],
        }));
        assert_eq!(
            one_missing.faults,
            ["declared compute types float16 are not supported on this host's CUDA device (supported: float32, int8_float16)"]
        );

        let no_device = faster_whisper_verdict(serde_json::json!({
            "cuda_devices": 0,
            "compute_types": [],
        }));
        assert_eq!(
            no_device.faults,
            ["CTranslate2 sees no CUDA device, so compute types float16, int8_float16 cannot be loaded"]
        );

        let query_failed = faster_whisper_verdict(serde_json::json!({"error": "RuntimeError: x"}));
        assert_eq!(
            query_failed.faults,
            ["CTranslate2 could not query CUDA: RuntimeError: x"]
        );
        let not_answered = faster_whisper_verdict(serde_json::Value::Null);
        assert_eq!(
            not_answered.faults,
            ["CTranslate2's CUDA support was not checked"]
        );
    }

    /// What a host that can serve `kokoro` answers, with `torch` replaced by
    /// `torch_facts`.
    fn kokoro_verdict(torch_facts: serde_json::Value) -> Verdict {
        let facts: DependencyFacts = serde_json::from_value(serde_json::json!({
            "modules": {
                "torch": {"version": "2.9.0+cu129"},
                "kokoro": {"version": "0.9.4"},
                "misaki": {"version": "0.9.4"},
                "en_core_web_sm": {"version": "3.8.0"},
                "espeakng_loader": {"version": "unknown"},
            },
            "libraries": {},
            "torch": torch_facts,
            "espeak_ng": {"library": "/usr/lib/libespeak-ng.so.1", "version": "1.51"},
        }))
        .unwrap();
        evaluate(
            &profile("kokoro", &["float32"], &[]),
            &requirements("kokoro").unwrap(),
            &facts,
            &ProbeOptions::default(),
        )
    }

    #[test]
    fn pytorch_without_a_cuda_device_is_a_warning_and_not_a_fault() {
        let no_device = kokoro_verdict(serde_json::json!({"cuda_devices": 0}));
        assert_eq!(no_device.faults, Vec::<String>::new());
        assert_eq!(
            no_device.warnings,
            ["PyTorch sees no CUDA device: a bundle whose runner entry selects `cuda` will not load"]
        );

        let one_device = kokoro_verdict(serde_json::json!({"cuda_devices": 1}));
        assert_eq!(one_device.faults, Vec::<String>::new());
        assert_eq!(one_device.warnings, Vec::<String>::new());
        assert_eq!(
            one_device.facts,
            [
                "torch 2.9.0+cu129",
                "kokoro 0.9.4",
                "misaki 0.9.4",
                "en_core_web_sm 3.8.0",
                "espeakng_loader",
                "PyTorch sees 1 CUDA device",
                "espeak-ng 1.51 from `/usr/lib/libespeak-ng.so.1`",
            ]
        );

        let query_failed = kokoro_verdict(serde_json::json!({"error": "RuntimeError: x"}));
        assert_eq!(
            query_failed.faults,
            ["PyTorch could not query CUDA: RuntimeError: x"]
        );
        let not_answered = kokoro_verdict(serde_json::Value::Null);
        assert_eq!(
            not_answered.faults,
            ["PyTorch's CUDA support was not checked"]
        );
    }

    #[test]
    fn an_imported_loader_with_no_answer_about_the_library_is_a_fault() {
        let facts: DependencyFacts = serde_json::from_value(serde_json::json!({
            "modules": {"espeakng_loader": {"version": "unknown"}},
        }))
        .unwrap();
        let verdict = evaluate(
            &profile("kokoro", &["float32"], &[]),
            &Requirements {
                modules: &["espeakng_loader"],
                libraries: &[],
            },
            &facts,
            &ProbeOptions::default(),
        );
        assert_eq!(verdict.faults, ["espeak-ng was not checked"]);
    }

    #[test]
    fn a_module_or_library_the_check_did_not_answer_for_is_a_fault() {
        let facts: DependencyFacts = serde_json::from_value(serde_json::json!({
            "modules": {"ctranslate2": {}, "faster_whisper": {"version": "1.2.1"}},
            "libraries": {"libcublas.so.12": {"mapped": []}},
        }))
        .unwrap();
        let verdict = evaluate(
            &profile("faster_whisper", &["float16"], &[]),
            &requirements("faster_whisper").unwrap(),
            &facts,
            &ProbeOptions::default(),
        );
        assert_eq!(
            verdict.faults,
            [
                "`ctranslate2` was not checked",
                "`av` was not checked",
                "`libcublas.so.12` was not checked",
                "CTranslate2's CUDA support was not checked",
            ]
        );
    }

    fn verdict(faults: &[&str], warnings: &[&str], facts: &[&str]) -> Verdict {
        let owned = |items: &[&str]| items.iter().map(|item| (*item).to_string()).collect();
        Verdict {
            faults: owned(faults),
            warnings: owned(warnings),
            facts: owned(facts),
        }
    }

    #[test]
    fn a_fault_in_any_profile_fails_the_dependency_finding_and_a_warning_warns() {
        let fault = || verdict(&["broken"], &["slow"], &["fact"]);
        let warning = || verdict(&[], &["slow"], &["fact"]);
        let fine = || verdict(&[], &[], &["fact"]);
        let status = |verdicts: &[(&str, Verdict)]| {
            let finding = dependency_summary(verdicts);
            (finding.status_label(), finding.severity_label())
        };
        assert_eq!(status(&[("a", fine()), ("b", fine())]), ("ok", "info"));
        for order in [
            [("a", warning()), ("b", fine())],
            [("a", fine()), ("b", warning())],
        ] {
            assert_eq!(status(&order), ("warning", "warning"));
        }
        for order in [
            [("a", fault()), ("b", warning())],
            [("a", warning()), ("b", fault())],
            [("a", fault()), ("b", fine())],
            [("a", fine()), ("b", fault())],
        ] {
            assert_eq!(status(&order), ("fail", "critical"));
        }

        let finding = dependency_summary(&[("a", fault()), ("b", warning()), ("c", fine())]);
        assert_eq!(finding.message, "`a`: broken; `b`: slow; `c`: fact");
        assert!(finding.hint.is_some());
        assert!(dependency_summary(&[("b", warning())]).hint.is_none());
    }

    fn inputs_for<'a>(
        report: &'a BackendProbeReport,
        probe: &'a ProbeOptions,
        versions: &'a BTreeMap<String, String>,
        agent_environment: &'a AgentEnvironment,
    ) -> Inputs<'a> {
        Inputs {
            report,
            probe,
            versions,
            agent_environment,
            descriptor_dir: Path::new(BACKEND_DESCRIPTOR_DIR),
            systemd: true,
            mountinfo: None,
        }
    }

    fn report_of(runner_profiles: Vec<RunnerProfileProbe>) -> BackendProbeReport {
        BackendProbeReport {
            backend_name: "python_pytorch".into(),
            descriptor_path: PathBuf::from("backend.json"),
            state: BackendProbeState::Runnable,
            install_hint: None,
            runner_profiles,
        }
    }

    #[test]
    fn a_profile_with_no_dependency_list_reports_that_only_its_interpreter_was_checked() {
        let mut stopped = runnable(profile("kokoro", &["float32"], &[]));
        stopped.state = BackendProbeState::PythonInterpreterMissing {
            interpreter: "/opt/env/bin/python".into(),
        };
        let (probe, versions, environment) = (
            ProbeOptions::default(),
            BTreeMap::new(),
            AgentEnvironment::default(),
        );

        let unknown = report_of(vec![runnable(profile("another_family", &["int8"], &[]))]);
        let finding = dependency_finding(&inputs_for(&unknown, &probe, &versions, &environment));
        assert_eq!(finding.status_label(), "ok");
        assert_eq!(
            finding.message,
            "`another_family`: no dependency list for this profile; only its interpreter was checked"
        );

        // A profile whose interpreter does not run is never asked.
        let both = report_of(vec![
            runnable(profile("another_family", &["int8"], &[])),
            stopped,
        ]);
        let finding = dependency_finding(&inputs_for(&both, &probe, &versions, &environment));
        assert_eq!(finding.status_label(), "fail");
        assert!(
            finding
                .message
                .ends_with("`kokoro`: not checked: its interpreter does not run"),
            "{}",
            finding.message
        );
    }

    #[test]
    fn the_record_fails_for_what_the_launcher_refuses() {
        let (probe, versions) = (ProbeOptions::default(), BTreeMap::new());
        let record = |report: &BackendProbeReport, environment: &AgentEnvironment| {
            let finding = record_finding(&inputs_for(report, &probe, &versions, environment));
            (finding.status_label(), finding.message)
        };
        let whisper = || profile("faster_whisper", &["float16"], &["/opt/env/lib"]);
        let packaged = AgentEnvironment::default();

        let (status, message) = record(&report_of(vec![runnable(whisper())]), &packaged);
        assert_eq!(status, "ok");
        assert_eq!(
            message,
            "1 runner profile installed, as the sidecar launcher reads it: `faster_whisper`: interpreter `/opt/env/bin/python` (Python 3.12.3 (main) [GCC]), environment root `/opt/env`, compute types float16, packages speech-runtime (version not read)"
        );

        for separator in [':', ';'] {
            let dir = format!("/opt/env/lib{separator}x");
            let split = profile("faster_whisper", &["float16"], &[dir.as_str()]);
            let (status, message) = record(&report_of(vec![runnable(split)]), &packaged);
            assert_eq!(status, "fail");
            assert_eq!(
                message,
                format!("runner profile `faster_whisper`: library search path `{dir}` holds `:` or `;`, which the launcher refuses")
            );
        }

        for (state, said) in [
            (
                BackendProbeState::PythonInterpreterMissing {
                    interpreter: "/opt/env/bin/python".into(),
                },
                "interpreter `/opt/env/bin/python` is not an executable file that runs",
            ),
            (
                BackendProbeState::PythonVersionMismatch {
                    interpreter: "/opt/env/bin/python".into(),
                    observed: "3.9.18".into(),
                    required: "3.10".into(),
                },
                "interpreter `/opt/env/bin/python` is Python 3.9.18; the backend needs 3.10",
            ),
            (
                BackendProbeState::PythonModuleImportFailed {
                    module: "tensorplate_pytorch_backend".into(),
                    detail: "ModuleNotFoundError".into(),
                },
                "`tensorplate_pytorch_backend` does not import in `/opt/env/bin/python`: ModuleNotFoundError",
            ),
        ] {
            let mut stopped = runnable(whisper());
            stopped.state = state;
            let (status, message) = record(&report_of(vec![stopped]), &packaged);
            assert_eq!(status, "fail");
            assert_eq!(message, format!("runner profile `faster_whisper`: {said}"));
        }

        let mut elsewhere = runnable(whisper());
        elsewhere.observed.as_mut().unwrap().prefix = "/opt/env/nested".into();
        assert_eq!(record(&report_of(vec![elsewhere]), &packaged).0, "fail");
        let mut above = runnable(whisper());
        above.observed.as_mut().unwrap().prefix = "/opt".into();
        assert_eq!(record(&report_of(vec![above]), &packaged).0, "fail");

        // The agent reads another descriptor directory than this report did.
        let moved = AgentEnvironment::parse("TP_BACKEND_DESCRIPTOR_DIR=/opt/backends\n");
        let (status, message) = record(&report_of(vec![runnable(whisper())]), &moved);
        assert_eq!(status, "fail");
        assert!(message.contains("reads backend descriptors from `/opt/backends`"));
        // Off systemd the file is not the agent's environment.
        let report = report_of(vec![runnable(whisper())]);
        let mut unmanaged = inputs_for(&report, &probe, &versions, &moved);
        unmanaged.systemd = false;
        assert_eq!(record_finding(&unmanaged).status_label(), "ok");
        let same = AgentEnvironment::parse(&format!(
            "TP_BACKEND_DESCRIPTOR_DIR={BACKEND_DESCRIPTOR_DIR}\n"
        ));
        assert_eq!(record(&report_of(vec![runnable(whisper())]), &same).0, "ok");
    }

    #[test]
    fn a_path_reached_through_a_link_is_inside_the_root_it_resolves_under() {
        let td = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(td.path().join("real/env/lib")).unwrap();
        std::fs::create_dir_all(td.path().join("real/other")).unwrap();
        std::os::unix::fs::symlink(td.path().join("real"), td.path().join("link")).unwrap();
        let staged = ProbeOptions {
            staged_root: Some(td.path().to_path_buf()),
            ..ProbeOptions::default()
        };
        assert!(inside("/real/env/lib", "/link/env", &staged));
        assert!(!inside("/real/other", "/link/env", &staged));
        // As declared, with nothing on disk to resolve.
        assert!(inside("/opt/env/lib", "/opt/env", &ProbeOptions::default()));
        assert!(!inside(
            "/opt/environment",
            "/opt/env",
            &ProbeOptions::default()
        ));
    }

    #[test]
    fn a_temporary_directory_the_launcher_refuses_fails_the_launch_environment() {
        let (probe, versions) = (ProbeOptions::default(), BTreeMap::new());
        let report = report_of(vec![runnable(profile("kokoro", &["float32"], &[]))]);
        let finding = |environment: &str, mountinfo: Option<&str>| {
            let environment = AgentEnvironment::parse(environment);
            let mut inputs = inputs_for(&report, &probe, &versions, &environment);
            inputs.mountinfo = mountinfo;
            let finding = launch_environment_finding(&inputs);
            (finding.status_label(), finding.message)
        };
        let exec = "1 0 0:1 / / rw,relatime - ext4 /dev/root rw\n";

        let (status, message) = finding("", Some(exec));
        assert_eq!(status, "ok");
        assert_eq!(
            message,
            "sidecar temporary directory `/tmp` (the agent unit's private `/tmp`; `/` allows execution as mounted for this shell); the launcher sets, over the agent's environment: `kokoro` LD_LIBRARY_PATH= ORT_DISABLE_TELEMETRY=1 TMPDIR=/tmp"
        );

        // `.` is a directory wherever this test runs.
        let (status, message) = finding("TMPDIR=.\n", Some(exec));
        assert_eq!(status, "fail");
        assert!(message.contains("is not an absolute path"), "{message}");
        let (status, message) = finding("TMP=/nonexistent/tensorplate-tmp\n", Some(exec));
        assert_eq!(status, "fail");
        assert!(
            message.ends_with("is not a directory: the launcher refuses every runner profile"),
            "{message}"
        );
        assert!(finding("TMP=/nonexistent/tensorplate-tmp\n", Some(exec))
            .1
            .contains("(`TMP` in `/etc/default/tensorplate-agent`)"));

        let noexec = "1 0 0:1 / / rw,noexec,relatime - ext4 /dev/root rw\n";
        assert_eq!(finding("", Some(noexec)).0, "fail");
        // Not readable is not the same as allowed.
        assert_eq!(finding("", None).0, "warning");
        assert_eq!(finding("", Some("")).0, "warning");
    }

    /// The dependency check against a staged interpreter that is this
    /// shell script.
    fn observed_from(script: &str) -> Result<DependencyFacts, String> {
        use std::os::unix::fs::PermissionsExt;
        let td = tempfile::TempDir::new().unwrap();
        let interpreter = td.path().join("opt/env/bin/python");
        std::fs::create_dir_all(interpreter.parent().unwrap()).unwrap();
        std::fs::write(&interpreter, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&interpreter, std::fs::Permissions::from_mode(0o755)).unwrap();
        observe_dependencies(
            &profile("kokoro", &["float32"], &[]),
            &requirements("kokoro").unwrap(),
            Path::new("/tmp"),
            &ProbeOptions {
                staged_root: Some(td.path().to_path_buf()),
                ..ProbeOptions::default()
            },
        )
    }

    #[test]
    fn the_dependency_facts_are_the_last_line_a_successful_check_prints() {
        let facts = observed_from(
            r#"echo 'an engine printed this while importing'
echo '{"modules": {"torch": {"version": "2.9.0"}}, "libraries": {}}'"#,
        )
        .unwrap();
        assert_eq!(facts.modules["torch"].version.as_deref(), Some("2.9.0"));

        // What the script was asked: the profile's lists, as its one argument.
        let asked = observed_from(r#"printf '{"modules": {"asked": {"version": "%s"}}}\n' "$(printf '%s' "$3" | tr -d '"')""#)
            .unwrap();
        assert_eq!(
            asked.modules["asked"].version.as_deref(),
            Some("{libraries:[],modules:[torch,kokoro,misaki,en_core_web_sm,espeakng_loader]}")
        );

        assert_eq!(
            observed_from("echo 'not facts'").unwrap_err(),
            "the dependency check printed no facts"
        );
        assert_eq!(
            observed_from("exit 0").unwrap_err(),
            "the dependency check printed no facts"
        );
        // Facts printed by a check that then failed are not facts.
        assert_eq!(
            observed_from(
                r#"echo '{"modules": {}, "libraries": {}}'
echo 'Traceback' >&2
echo 'MemoryError' >&2
exit 3"#
            )
            .unwrap_err(),
            "the dependency check failed (exit status: 3): MemoryError"
        );
    }

    #[test]
    fn an_interpreter_override_in_the_agent_environment_is_noted_when_it_differs() {
        let descriptor: BackendDescriptor = serde_json::from_str(include_str!(
            "../../../../packaging/backend-metadata/python_pytorch.json"
        ))
        .unwrap();
        let note = |environment: &str| {
            own_interpreter_override(&descriptor, &AgentEnvironment::parse(environment))
        };
        assert_eq!(note(""), None);
        assert_eq!(
            note("TP_PYTHON_PYTORCH_EXECUTABLE=/usr/bin/python3\n"),
            None
        );
        assert_eq!(
            note("TP_PYTHON_PYTORCH_EXECUTABLE=/opt/venv/bin/python\n").unwrap(),
            "; note: `TP_PYTHON_PYTORCH_EXECUTABLE=/opt/venv/bin/python` in `/etc/default/tensorplate-agent` starts a bundle that names no runner profile in that interpreter, which this check did not run (it ran `/usr/bin/python3`)"
        );
        // The launcher takes the first of the three that is set and not empty.
        let first =
            note("TP_TEST_PYTHON=/c\nTP_TEST_PYTHON_EXE=/b\nTP_PYTHON_PYTORCH_EXECUTABLE=\n");
        assert!(first
            .unwrap()
            .starts_with("; note: `TP_TEST_PYTHON_EXE=/b`"));
        assert_eq!(
            note("TP_PYTHON_PYTORCH_EXECUTABLE=/usr/bin/python3\nTP_TEST_PYTHON=/c\n"),
            None
        );
    }

    #[test]
    fn an_environment_file_that_cannot_be_read_is_a_warning_and_a_missing_one_is_not() {
        let td = tempfile::TempDir::new().unwrap();
        let missing = AgentEnvironment::read(&td.path().join("absent"));
        assert!(missing.unreadable.is_none() && missing.variables.is_empty());
        // A directory is there and cannot be read as a file by any user.
        let unreadable = AgentEnvironment::read(td.path());
        let why = unreadable.unreadable.clone().expect("a read error");

        let (probe, versions) = (ProbeOptions::default(), BTreeMap::new());
        let report = report_of(vec![runnable(profile("kokoro", &["float32"], &[]))]);
        let all = findings(&inputs_for(&report, &probe, &versions, &unreadable));
        let launch = &all[2];
        assert_eq!(launch.id_label(), "runner_launch_environment");
        assert_eq!(launch.status_label(), "warning");
        assert_eq!(
            launch.message,
            format!("could not read `/etc/default/tensorplate-agent` ({why}): the temporary directory, backend descriptor directory and interpreter override it may set for the agent are not known to this report, which assumed none")
        );
        let all = findings(&inputs_for(&report, &probe, &versions, &missing));
        assert!(
            !all[2].message.contains(AGENT_ENVIRONMENT_FILE),
            "{}",
            all[2].message
        );
    }

    #[test]
    fn an_empty_descriptor_directory_or_temporary_directory_is_read_as_the_agent_reads_it() {
        let (probe, versions) = (ProbeOptions::default(), BTreeMap::new());
        let report = report_of(vec![runnable(profile("kokoro", &["float32"], &[]))]);
        // Empty is unset for the descriptor directory.
        let unset = AgentEnvironment::parse("TP_BACKEND_DESCRIPTOR_DIR=\n");
        let record = record_finding(&inputs_for(&report, &probe, &versions, &unset));
        assert_eq!(record.status_label(), "ok");
        // Empty is set for the temporary directory, and the launcher refuses it.
        let empty = AgentEnvironment::parse("TMPDIR=\nTMP=/tmp\n");
        let launch = launch_environment_finding(&inputs_for(&report, &probe, &versions, &empty));
        assert_eq!(launch.status_label(), "fail");
        assert_eq!(
            launch.message,
            "the sidecar temporary directory `` (`TMPDIR` in `/etc/default/tensorplate-agent`) is empty: the launcher refuses every runner profile"
        );
    }

    #[test]
    fn a_temporary_directory_behind_a_link_is_looked_up_where_it_resolves() {
        let td = tempfile::TempDir::new().unwrap();
        let real = td.path().canonicalize().unwrap().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = td.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mountinfo = format!(
            "1 0 0:1 / / rw,relatime - ext4 /dev/root rw\n2 1 0:2 / {} rw,noexec - tmpfs tmpfs rw\n",
            real.display()
        );
        let (probe, versions) = (ProbeOptions::default(), BTreeMap::new());
        let report = report_of(vec![runnable(profile("kokoro", &["float32"], &[]))]);
        let environment = AgentEnvironment::parse(&format!("TMPDIR={}\n", link.display()));
        let mut inputs = inputs_for(&report, &probe, &versions, &environment);
        inputs.mountinfo = Some(&mountinfo);
        let finding = launch_environment_finding(&inputs);
        assert_eq!(finding.status_label(), "fail");
        assert!(finding.message.contains("which is mounted `noexec`"));
    }

    #[test]
    fn the_recorder_asks_what_doctor_asks() {
        let recorder = include_str!("../../../tests/fixtures/doctor_runner_profiles/record.sh");
        for id in ["faster_whisper", "kokoro"] {
            let line = format!("{id}='{}'\n", query_spec(&requirements(id).unwrap()));
            assert!(recorder.contains(&line), "record.sh does not set {line}");
        }
    }

    /// The script in a real interpreter, where this host has one: a module
    /// and a library that are there, and one of each that is not.
    #[test]
    fn the_dependency_script_runs_in_a_real_python() {
        let python = Path::new("/usr/bin/python3");
        if !cfg!(target_os = "linux") || !python.exists() {
            eprintln!("skipped: needs Linux and /usr/bin/python3");
            return;
        }
        let spec = r#"{"modules": ["json", "tensorplate_absent_module"], "libraries": ["libz.so.1", "libtensorplate-absent.so.9"]}"#;
        let output = run_bounded(
            Command::new(python)
                .arg("-c")
                .arg(DEPENDENCY_SCRIPT)
                .arg(spec)
                .current_dir("/"),
            Duration::from_secs(60),
        )
        .unwrap();
        assert!(output.status.success(), "{output:?}");
        let facts: DependencyFacts = serde_json::from_slice(&output.stdout).unwrap();

        assert!(facts.modules["json"].version.is_some());
        let absent = facts.modules["tensorplate_absent_module"].error.as_deref();
        assert!(absent.unwrap().starts_with("ModuleNotFoundError: "));
        let mapped = facts.libraries["libz.so.1"].mapped.clone().unwrap();
        assert!(!mapped.is_empty());
        for path in &mapped {
            let path = Path::new(path);
            let name = path.file_name().unwrap().to_string_lossy();
            assert!(path.is_file() && name.starts_with("libz.so.1"), "{path:?}");
        }
        let absent = facts.libraries["libtensorplate-absent.so.9"]
            .error
            .as_deref();
        assert!(absent.unwrap().starts_with("OSError: "));
        // No engine asked for, so no engine fact.
        assert!(facts.ctranslate2.is_none() && facts.torch.is_none() && facts.espeak_ng.is_none());
    }

    #[test]
    fn only_the_two_shipped_profiles_have_a_dependency_list() {
        assert!(requirements("faster_whisper").is_some());
        assert!(requirements("kokoro").is_some());
        assert!(requirements("another_family").is_none());
    }
}
