// SPDX-License-Identifier: Apache-2.0
//
// `doctor`'s runner profile findings, rendered for the shipped backend
// descriptor and runner profile declarations staged under a temporary root.
//
// The staged interpreters replay what a real Python 3.12 printed for each
// query, recorded by `fixtures/doctor_runner_profiles/record.sh`. No
// recording has a GPU, so every case here is one where the profiles cannot
// serve; the case on a host that can is hardware evidence, not a fixture.
//
// Regenerate with:
//   UPDATE_GOLDEN=1 cargo test -p tensorplate-cli --test doctor_runner_profiles

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use tensorplate_cli::commands::doctor::finding::Finding;
use tensorplate_cli::commands::doctor::install::{
    python_pytorch_findings, InstallProbeOptions, PackageSource,
};

const ENVIRONMENT_ROOT: &str = "usr/lib/tensorplate/speech-runtime";
const DESCRIPTOR_DIR: &str = "usr/share/tensorplate/backends/python_pytorch";
const CUBLAS: &str =
    "/usr/lib/tensorplate/speech-runtime/lib/python3.12/site-packages/nvidia/cublas/lib";

struct Case {
    name: &'static str,
    profiles: &'static [&'static str],
    recording: &'static str,
    mountinfo: &'static str,
    agent_environment: Option<&'static str>,
    /// Statuses of `python_pytorch_runtime`, `runner_profiles`,
    /// `runner_profile_dependencies` and `runner_launch_environment`.
    statuses: [&'static str; 4],
}

const BOTH: &[&str] = &["faster_whisper", "kokoro"];

const CASES: &[Case] = &[
    Case {
        name: "environment_without_engines",
        profiles: BOTH,
        recording: "environment",
        mountinfo: "exec",
        agent_environment: None,
        statuses: ["missing", "ok", "fail", "ok"],
    },
    Case {
        name: "interpreter_outside_its_environment",
        profiles: BOTH,
        recording: "environment_without_marker",
        mountinfo: "exec",
        agent_environment: None,
        statuses: ["missing", "fail", "fail", "ok"],
    },
    Case {
        name: "espeak_library_absent",
        profiles: BOTH,
        recording: "environment_without_espeak",
        mountinfo: "exec",
        agent_environment: None,
        statuses: ["missing", "ok", "fail", "ok"],
    },
    Case {
        name: "cublas_not_on_the_search_path",
        profiles: BOTH,
        recording: "ct2",
        mountinfo: "exec",
        agent_environment: None,
        statuses: ["missing", "ok", "fail", "ok"],
    },
    Case {
        name: "cublas_resolved_from_the_system",
        profiles: BOTH,
        recording: "ct2_system_library",
        mountinfo: "exec",
        agent_environment: None,
        statuses: ["missing", "ok", "fail", "ok"],
    },
    Case {
        name: "cublas_from_the_profile_and_no_cuda_device",
        profiles: BOTH,
        recording: "ct2_profile_library",
        mountinfo: "exec",
        agent_environment: None,
        statuses: ["missing", "ok", "fail", "ok"],
    },
    Case {
        name: "temporary_directory_mounted_noexec",
        profiles: BOTH,
        recording: "ct2_profile_library",
        mountinfo: "noexec",
        agent_environment: None,
        statuses: ["missing", "ok", "fail", "fail"],
    },
    Case {
        name: "agent_environment_file",
        profiles: BOTH,
        recording: "ct2_profile_library",
        mountinfo: "noexec",
        agent_environment: Some(
            "TMPDIR=/var/lib/tensorplate/tmp\nTP_PYTHON_PYTORCH_EXECUTABLE=/opt/venv/bin/python\nTP_BACKEND_DESCRIPTOR_DIR=/opt/backends\n",
        ),
        statuses: ["missing", "fail", "fail", "ok"],
    },
];

fn repo_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(relative)
}

fn fixture(relative: &str) -> PathBuf {
    repo_path("cli/tests/fixtures/doctor_runner_profiles").join(relative)
}

fn write(path: &Path, body: &str, mode: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// A shell script standing at an interpreter's path that answers each query
/// with the recorded output and exit status, and logs, per query, the
/// environment and the directory it was started in.
fn replaying_interpreter(recording: &Path, log: &Path, queries: &str) -> String {
    format!(
        "#!/bin/sh\nrec='{}'\nlog='{}'\nanswer() {{\n  env > \"$log.$1.environment\"\n  pwd > \"$log.$1.directory\"\n  [ -f \"$rec/$1.stdout\" ] && cat \"$rec/$1.stdout\"\n  [ -f \"$rec/$1.stderr\" ] && cat \"$rec/$1.stderr\" >&2\n  exit \"$(cat \"$rec/$1.status\")\"\n}}\ncase \"$2\" in\n{queries}esac\nexit 97\n",
        recording.display(),
        log.display()
    )
}

/// The queries a staged interpreter answered, from its per-query logs.
fn answered(root: &Path, log: &str) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(root)
        .unwrap()
        .filter_map(|entry| entry.unwrap().file_name().into_string().ok())
        .filter_map(|name| {
            Some(
                name.strip_prefix(&format!("{log}."))?
                    .strip_suffix(".environment")?
                    .to_string(),
            )
        })
        .collect();
    names.sort();
    names
}

/// The queries a recording directory holds an answer for.
fn recorded(recording: &str) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(fixture("recordings").join(recording))
        .unwrap()
        .filter_map(|entry| entry.unwrap().file_name().into_string().ok())
        .filter_map(|name| Some(name.rsplit_once('.')?.0.to_string()))
        .collect();
    names.sort();
    names.dedup();
    names
}

const PROFILE_QUERIES: &str = "  '# tensorplate probe: interpreter'*) answer interpreter ;;\n  'import tensorplate_pytorch_backend') answer import ;;\n  '# tensorplate doctor: runner profile dependencies'*)\n    case \"$3\" in\n      *ctranslate2*) answer dependencies.faster_whisper ;;\n      *kokoro*) answer dependencies.kokoro ;;\n    esac ;;\n";

const OWN_QUERIES: &str = "  'import sys;print('*) answer version ;;\n  'import tensorplate_pytorch_backend') answer import ;;\n  'import torch; print('*) answer torch ;;\n";

fn stage(root: &Path, case: &Case) {
    let copy = |from: PathBuf, to: PathBuf| {
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::copy(&from, &to).unwrap_or_else(|e| panic!("copy {}: {e}", from.display()));
    };
    copy(
        repo_path("packaging/backend-metadata/python_pytorch.json"),
        root.join(DESCRIPTOR_DIR).join("backend.json"),
    );
    for profile in case.profiles {
        copy(
            repo_path(&format!(
                "packaging/backend-metadata/runner_profiles/{profile}.json"
            )),
            root.join(DESCRIPTOR_DIR)
                .join("runner_profiles.d")
                .join(format!("{profile}.json")),
        );
    }
    write(
        &root.join(ENVIRONMENT_ROOT).join("bin/python"),
        &replaying_interpreter(
            &fixture("recordings").join(case.recording),
            &root.join("sidecar"),
            PROFILE_QUERIES,
        ),
        0o755,
    );
    write(
        &root.join("usr/bin/python3"),
        &replaying_interpreter(&fixture("recordings/own"), &root.join("own"), OWN_QUERIES),
        0o755,
    );
    fs::create_dir_all(root.join("tmp")).unwrap();
    fs::create_dir_all(root.join("var/lib/tensorplate/tmp")).unwrap();
    copy(
        fixture(&format!("mountinfo/{}.txt", case.mountinfo)),
        root.join("proc/self/mountinfo"),
    );
    if let Some(body) = case.agent_environment {
        write(&root.join("etc/default/tensorplate-agent"), body, 0o644);
    }
}

/// Every package either declaration names, at one version.
fn installed_packages() -> BTreeMap<String, String> {
    ["base", "ct2", "vad", "cublas", "kokoro", "torch", "cuda"]
        .into_iter()
        .map(|part| {
            (
                format!("tensorplate-speech-runtime-{part}"),
                "0.3.1-1".to_string(),
            )
        })
        .collect()
}

fn findings_for(case: &Case) -> (TempDir, Vec<Finding>) {
    let td = TempDir::new().unwrap();
    stage(td.path(), case);
    let opts = InstallProbeOptions {
        prefix: Some(td.path().to_path_buf()),
        probe_backends: true,
        skip_systemd: false,
        cli_config_rejection: None,
    };
    let findings = python_pytorch_findings(&opts, PackageSource::Staged(&installed_packages()));
    (td, findings)
}

fn render(findings: &[Finding]) -> String {
    let mut out = String::new();
    for f in findings {
        out.push_str(&format!(
            "[{}] {} {} — {}\n",
            f.status_label(),
            f.severity_label(),
            f.id_label(),
            f.message
        ));
        if let Some(hint) = &f.hint {
            out.push_str(&format!("    hint: {hint}\n"));
        }
    }
    out
}

#[test]
fn the_runner_profile_findings_match_the_golden() {
    let mut rendered = String::new();
    for case in CASES {
        let (td, findings) = findings_for(case);
        let text = render(&findings);
        assert!(
            !text.contains(&td.path().display().to_string()),
            "{}: a staged path leaked into a message:\n{text}",
            case.name
        );
        let ids: Vec<&str> = findings.iter().map(Finding::id_label).collect();
        assert_eq!(
            ids,
            [
                "python_pytorch_backend",
                "python_pytorch_runtime",
                "runner_profiles",
                "runner_profile_dependencies",
                "runner_launch_environment"
            ],
            "{}",
            case.name
        );
        let statuses: Vec<&str> = findings[1..].iter().map(Finding::status_label).collect();
        assert_eq!(statuses, case.statuses, "{}:\n{text}", case.name);
        assert_eq!(
            answered(td.path(), "sidecar"),
            recorded(case.recording),
            "{}: recordings no query replayed, or the reverse",
            case.name
        );
        assert_eq!(answered(td.path(), "own"), recorded("own"), "{}", case.name);
        rendered.push_str(&format!("## {}\n{text}\n", case.name));
    }

    let golden = repo_path("test/platform/doctor_runner_profiles.golden.txt");
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        fs::write(&golden, &rendered).unwrap();
    }
    let expected = fs::read_to_string(&golden).unwrap_or_default();
    assert_eq!(
        rendered, expected,
        "golden differs; rerun with UPDATE_GOLDEN=1"
    );
}

#[test]
fn every_query_runs_in_the_environment_the_launcher_sets_and_from_the_root_directory() {
    let logged = |td: &TempDir, query: &str, what: &str| {
        fs::read_to_string(td.path().join(format!("sidecar.{query}.{what}"))).unwrap()
    };
    let expect = |td: &TempDir, query: &str, search: &str, temp_dir: &str| {
        let environment = logged(td, query, "environment");
        for expected in [
            format!("LD_LIBRARY_PATH={search}"),
            "ORT_DISABLE_TELEMETRY=1".to_string(),
            format!("TMPDIR={temp_dir}"),
        ] {
            assert!(
                environment.lines().any(|line| line == expected),
                "{query}: `{expected}` missing from:\n{environment}"
            );
        }
        assert_eq!(logged(td, query, "directory"), "/\n", "{query}");
    };

    let (td, _) = findings_for(&CASES[5]);
    expect(&td, "dependencies.faster_whisper", CUBLAS, "/tmp");
    expect(&td, "dependencies.kokoro", "", "/tmp");
    assert_eq!(logged(&td, "interpreter", "directory"), "/\n");
    assert_eq!(logged(&td, "import", "directory"), "/\n");

    // The temporary directory is the agent's, from its environment file.
    let from_file = CASES.iter().find(|case| case.agent_environment.is_some());
    let (td, _) = findings_for(from_file.unwrap());
    expect(
        &td,
        "dependencies.faster_whisper",
        CUBLAS,
        "/var/lib/tensorplate/tmp",
    );
    expect(&td, "dependencies.kokoro", "", "/var/lib/tensorplate/tmp");
}

#[test]
fn a_host_without_a_runner_profile_skips_the_three_findings() {
    let case = Case {
        profiles: &[],
        ..CASES[0]
    };
    let (_td, findings) = findings_for(&case);
    let runtime = &findings[1];
    // With no profile to run in, a missing PyTorch refuses every bundle.
    assert_eq!(runtime.status_label(), "fail");
    for finding in &findings[2..] {
        assert_eq!(finding.status_label(), "skipped");
        assert_eq!(finding.message, "skipped: no runner profile is installed");
    }
}

#[test]
fn a_declaration_whose_package_is_not_installed_names_the_package_to_install() {
    let td = TempDir::new().unwrap();
    stage(td.path(), &CASES[0]);
    let opts = InstallProbeOptions {
        prefix: Some(td.path().to_path_buf()),
        probe_backends: true,
        skip_systemd: false,
        cli_config_rejection: None,
    };
    let mut packages = installed_packages();
    packages.remove("tensorplate-speech-runtime-cuda");
    let findings = python_pytorch_findings(&opts, PackageSource::Staged(&packages));
    assert_eq!(findings[0].status_label(), "fail");
    assert_eq!(
        findings[0].hint.as_deref(),
        Some("install tensorplate-speech-runtime-cuda, or remove the package that declares `kokoro`, then restart tensorplate-agent")
    );
    assert!(findings[1..].iter().all(|f| f.status_label() == "skipped"));

    // A declaration that does not parse belongs to the package that ships it.
    let declaration = td
        .path()
        .join(DESCRIPTOR_DIR)
        .join("runner_profiles.d/kokoro.json");
    fs::write(declaration, "{").unwrap();
    let findings = python_pytorch_findings(&opts, PackageSource::Staged(&installed_packages()));
    let hint = findings[0].hint.as_deref().unwrap();
    assert!(
        hint.starts_with("reinstall the package that installs `")
            && hint.contains("runner_profiles.d/kokoro.json"),
        "{hint}"
    );
}

#[test]
fn the_three_findings_are_skipped_where_nothing_can_be_probed() {
    let skipped = |findings: &[Finding], why: &str| {
        let ids: Vec<&str> = findings.iter().map(Finding::id_label).collect();
        assert_eq!(
            &ids[ids.len() - 3..],
            [
                "runner_profiles",
                "runner_profile_dependencies",
                "runner_launch_environment"
            ]
        );
        for finding in &findings[findings.len() - 3..] {
            assert_eq!(finding.status_label(), "skipped");
            assert_eq!(finding.message, format!("skipped: {why}"));
        }
    };
    let td = TempDir::new().unwrap();
    stage(td.path(), &CASES[0]);
    let mut opts = InstallProbeOptions {
        prefix: Some(td.path().to_path_buf()),
        probe_backends: false,
        skip_systemd: false,
        cli_config_rejection: None,
    };
    let packages = installed_packages();
    let run = |opts: &InstallProbeOptions| {
        python_pytorch_findings(opts, PackageSource::Staged(&packages))
    };
    skipped(&run(&opts), "backend runtime probe disabled on this host");
    assert!(answered(td.path(), "sidecar").is_empty());

    // A runtime below the descriptor's minimum refuses the backend before
    // any interpreter runs.
    opts.probe_backends = true;
    let descriptor = td.path().join(DESCRIPTOR_DIR).join("backend.json");
    let mut document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&descriptor).unwrap()).unwrap();
    document["tensorplate_runtime_range"]["min"] = "999.0.0".into();
    fs::write(&descriptor, document.to_string()).unwrap();
    let findings = run(&opts);
    assert_eq!(findings[1].status_label(), "fail");
    skipped(
        &findings,
        "the backend is refused before any interpreter runs (see python_pytorch_runtime)",
    );
    assert!(answered(td.path(), "sidecar").is_empty());

    fs::remove_file(&descriptor).unwrap();
    skipped(&run(&opts), "backend descriptor absent");
}
