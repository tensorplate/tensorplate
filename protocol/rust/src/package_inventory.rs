// SPDX-License-Identifier: Apache-2.0
//
// packaging: which OS packages are installed.
//
// The backend descriptor reader refuses a runner profile that names a
// package which is not installed. It asks through [`PackageInventory`];
// [`DpkgInventory`] answers from dpkg's database with one bounded
// `dpkg-query` call.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Which OS packages are installed.
pub trait PackageInventory {
    /// The members of `names` whose files are installed.
    ///
    /// # Errors
    ///
    /// Why the package database could not be asked. A name the database
    /// does not know is not an error; it is absent from the answer.
    fn installed(&self, names: &BTreeSet<&str>) -> Result<BTreeSet<String>, String>;
}

/// The dpkg states in which every file of a package is on disk: `unpacked`
/// and each state after it.
///
/// `installed` alone would refuse a healthy install. The speech runtime
/// restarts the agent from its base package's trigger processing, where
/// dpkg reports that package `half-configured`, and after a run that only
/// unpacks, where the new packages are `unpacked`. Each state is recorded
/// under `tests/fixtures/dpkg_query_status/`.
pub const DPKG_UNPACKED_STATES: [&str; 5] = [
    "unpacked",
    "half-configured",
    "triggers-awaited",
    "triggers-pending",
    "installed",
];

const DPKG_QUERY_FORMAT: &str = "-f=${Package}\\t${db:Status-Status}\\n";

/// [`PackageInventory`] over `dpkg-query`.
#[derive(Clone, Debug)]
pub struct DpkgInventory {
    program: PathBuf,
    timeout: Duration,
}

impl Default for DpkgInventory {
    fn default() -> Self {
        Self::with_program("dpkg-query", Duration::from_secs(5))
    }
}

impl DpkgInventory {
    /// An inventory that runs `program` in place of `dpkg-query` and gives
    /// it `timeout` to answer.
    #[must_use]
    pub fn with_program(program: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            program: program.into(),
            timeout,
        }
    }
}

impl PackageInventory for DpkgInventory {
    fn installed(&self, names: &BTreeSet<&str>) -> Result<BTreeSet<String>, String> {
        // `dpkg-query -W` reads its arguments as glob patterns; only a
        // literal package name is ever asked about, and nothing else can
        // be installed under that name.
        let asked: BTreeSet<&str> = names
            .iter()
            .copied()
            .filter(|name| is_package_name(name))
            .collect();
        if asked.is_empty() {
            return Ok(BTreeSet::new());
        }
        let program = self.program.display();
        let mut child = Command::new(&self.program)
            .arg("-W")
            .arg(DPKG_QUERY_FORMAT)
            .arg("--")
            .args(&asked)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run `{program}`: {e}"))?;
        let status = wait_bounded(&mut child, self.timeout)
            .map_err(|e| format!("`{program}` gave no answer: {e}"))?;
        let stdout = drain(child.stdout.take());
        match status.code() {
            // 1: at least one name matched no package; it is absent from
            // the output.
            Some(0 | 1) => {
                parse_dpkg_status(&stdout, &asked).map_err(|e| format!("`{program}` printed {e}"))
            }
            _ => Err(format!(
                "`{program}` failed ({status}): {}",
                String::from_utf8_lossy(&drain(child.stderr.take())).trim()
            )),
        }
    }
}

/// The members of `asked` that `dpkg-query` output reports in one of
/// [`DPKG_UNPACKED_STATES`].
///
/// # Errors
///
/// A description of the first line that is not `<package>\t<state>`.
pub fn parse_dpkg_status(
    stdout: &[u8],
    asked: &BTreeSet<&str>,
) -> Result<BTreeSet<String>, String> {
    let text = std::str::from_utf8(stdout).map_err(|_| "output that is not UTF-8".to_string())?;
    let mut installed = BTreeSet::new();
    for line in text.lines() {
        let (name, state) = line
            .split_once('\t')
            .filter(|(name, state)| !name.is_empty() && !state.is_empty())
            .ok_or_else(|| format!("a line that is not `<package>\\t<state>`: `{line}`"))?;
        if asked.contains(name) && DPKG_UNPACKED_STATES.contains(&state) {
            installed.insert(name.to_string());
        }
    }
    Ok(installed)
}

/// A Debian package name: lowercase alphanumerics, `+`, `-` and `.`, at
/// least two characters, starting with an alphanumeric.
fn is_package_name(name: &str) -> bool {
    let valid = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    name.len() >= 2
        && name.starts_with(valid)
        && name
            .chars()
            .all(|c| valid(c) || matches!(c, '+' | '-' | '.'))
}

/// Wait for `child` to exit. Past `timeout` it is killed and reaped, so no
/// caller hangs on a package database that does not answer.
fn wait_bounded(child: &mut Child, timeout: Duration) -> Result<ExitStatus, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let failure = match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            Ok(None) => format!("no exit within {timeout:?}"),
            Err(e) => e.to_string(),
        };
        let _ = child.kill();
        let _ = child.wait();
        return Err(failure);
    }
}

fn drain(pipe: Option<impl Read>) -> Vec<u8> {
    let mut bytes = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut bytes);
    }
    bytes
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    macro_rules! recorded {
        ($name:literal) => {
            include_bytes!(concat!("../tests/fixtures/dpkg_query_status/", $name)).as_slice()
        };
    }

    const NAMES: [&str; 11] = [
        "tprec-base",
        "tprec-leaf",
        "tprec-awaiting",
        "tprec-installed",
        "tprec-unpacked",
        "tprec-half-configured",
        "tprec-half-installed",
        "tprec-config-files",
        "tprec-removed",
        "tprec-purged",
        "tprec-never-existed",
    ];

    fn installed_in(stdout: &[u8]) -> Vec<String> {
        parse_dpkg_status(stdout, &NAMES.into_iter().collect())
            .expect("recorded output parses")
            .into_iter()
            .collect()
    }

    #[test]
    fn a_package_is_installed_from_unpacked_onwards() {
        assert_eq!(
            installed_in(recorded!("each-state.stdout")),
            [
                "tprec-base",
                "tprec-half-configured",
                "tprec-installed",
                "tprec-leaf",
                "tprec-unpacked"
            ]
        );
        assert_eq!(
            installed_in(recorded!("triggers-deferred.stdout")),
            [
                "tprec-awaiting",
                "tprec-base",
                "tprec-half-configured",
                "tprec-installed",
                "tprec-leaf",
                "tprec-unpacked"
            ]
        );
        assert!(installed_in(recorded!("nothing-installed.stdout")).is_empty());
    }

    #[test]
    fn the_recordings_cover_every_dpkg_state() {
        let mut states = BTreeSet::new();
        for stdout in [
            recorded!("each-state.stdout"),
            recorded!("after-unpack-run.stdout"),
            recorded!("triggers-deferred.stdout"),
            recorded!("inside-trigger.stdout"),
            recorded!("inside-trigger-during-purge.stdout"),
        ] {
            for line in std::str::from_utf8(stdout).unwrap().lines() {
                states.insert(line.split_once('\t').expect("tab").1.to_string());
            }
        }
        let absent = ["config-files", "half-installed", "not-installed"];
        let all: BTreeSet<String> = DPKG_UNPACKED_STATES
            .iter()
            .chain(&absent)
            .map(ToString::to_string)
            .collect();
        assert_eq!(states, all);
    }

    #[test]
    fn the_triggered_base_package_counts_while_its_trigger_runs() {
        // The agent is restarted from here; dpkg calls the package
        // half-configured until the trigger's script returns.
        let stdout = recorded!("inside-trigger.stdout");
        assert_eq!(
            stdout,
            b"tprec-base\thalf-configured\ntprec-leaf\tinstalled\n"
        );
        assert_eq!(installed_in(stdout), ["tprec-base", "tprec-leaf"]);
        assert_eq!(
            installed_in(recorded!("inside-trigger-during-purge.stdout")),
            ["tprec-base"]
        );
    }

    #[test]
    fn packages_count_after_a_run_that_only_unpacks() {
        let installed = installed_in(recorded!("after-unpack-run.stdout"));
        assert!(installed.contains(&"tprec-base".to_string()));
        assert!(installed.contains(&"tprec-leaf".to_string()));
    }

    #[test]
    fn only_a_name_that_was_asked_about_is_reported() {
        let asked = BTreeSet::from(["tprec-leaf"]);
        let installed = parse_dpkg_status(recorded!("each-state.stdout"), &asked).unwrap();
        assert_eq!(installed, BTreeSet::from(["tprec-leaf".to_string()]));
    }

    #[test]
    fn output_of_another_shape_is_refused() {
        let asked = BTreeSet::from(["tprec-leaf"]);
        for stdout in [
            &b"tprec-leaf installed\n"[..],
            b"tprec-leaf\t\n",
            b"\tinstalled\n",
            b"tprec-leaf\tinstalled\n\n",
            b"tprec-leaf\tinstalled\n\xff\n",
        ] {
            assert!(
                parse_dpkg_status(stdout, &asked).is_err(),
                "accepted {:?}",
                String::from_utf8_lossy(stdout)
            );
        }
    }

    #[test]
    fn an_unknown_state_is_not_installed() {
        let asked = BTreeSet::from(["tprec-leaf"]);
        let installed = parse_dpkg_status(b"tprec-leaf\tInstalled\n", &asked).unwrap();
        assert!(installed.is_empty());
    }

    #[test]
    fn package_names_follow_debian_policy() {
        for name in [
            "tensorplate-speech-runtime-ct2",
            "libstdc++6",
            "g2",
            "python3.12",
        ] {
            assert!(is_package_name(name), "{name}");
        }
        for name in [
            "",
            "a",
            "-W",
            "tensorplate-*",
            "Tensorplate",
            "a b",
            "a\tb",
            "+a",
            "a:amd64",
        ] {
            assert!(!is_package_name(name), "{name}");
        }
    }

    /// An executable in `dir` standing in for `dpkg-query`.
    fn stub(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("dpkg-query-stub");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn ask(program: &Path, names: &[&str]) -> Result<BTreeSet<String>, String> {
        DpkgInventory::with_program(program, Duration::from_secs(5))
            .installed(&names.iter().copied().collect())
    }

    #[test]
    fn the_query_names_the_format_and_only_literal_package_names() {
        let dir = tempfile::tempdir().unwrap();
        let arguments = dir.path().join("arguments");
        let program = stub(
            dir.path(),
            &format!(
                "printf '%s\\n' \"$@\" >{}\nprintf 'tprec-leaf\\tinstalled\\n'\nexit 1",
                arguments.display()
            ),
        );
        let installed = ask(
            &program,
            &["tprec-leaf", "tprec-*", "--admindir=/tmp", "tprec-base"],
        );
        assert_eq!(
            installed.unwrap(),
            BTreeSet::from(["tprec-leaf".to_string()])
        );
        assert_eq!(
            std::fs::read_to_string(&arguments).unwrap(),
            "-W\n-f=${Package}\\t${db:Status-Status}\\n\n--\ntprec-base\ntprec-leaf\n"
        );
    }

    #[test]
    fn no_literal_name_asks_nothing() {
        let installed = ask(Path::new("/nonexistent/dpkg-query"), &["tprec-*"]);
        assert_eq!(installed.unwrap(), BTreeSet::new());
    }

    #[test]
    fn a_missing_or_failing_dpkg_query_is_an_error_not_an_empty_answer() {
        let err = ask(Path::new("/nonexistent/dpkg-query"), &["tprec-leaf"]).unwrap_err();
        assert!(err.contains("cannot run"), "{err}");

        let dir = tempfile::tempdir().unwrap();
        let program = stub(
            dir.path(),
            "printf 'tprec-leaf\\tinstalled\\n'\necho 'database is locked' >&2\nexit 2",
        );
        let err = ask(&program, &["tprec-leaf"]).unwrap_err();
        assert!(
            err.contains("failed") && err.contains("database is locked"),
            "{err}"
        );
    }

    #[test]
    fn a_query_that_does_not_answer_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let program = stub(
            dir.path(),
            &format!("echo $$ >{}\nexec sleep 60", pid_file.display()),
        );
        let started = Instant::now();
        let err = DpkgInventory::with_program(program, Duration::from_secs(1))
            .installed(&BTreeSet::from(["tprec-leaf"]))
            .unwrap_err();
        assert!(err.contains("no exit within"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the call was not bounded"
        );
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let alive = Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(
            !alive,
            "the query process {} outlived the timeout",
            pid.trim()
        );
    }

    #[test]
    fn the_hosts_own_dpkg_query_answers_or_is_reported_missing() {
        // The real database where there is one; where there is no
        // dpkg-query the same call must say it could not ask.
        let names = ["dpkg", "tprec-never-existed"];
        let on_path = std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| dir.join("dpkg-query").is_file())
        });
        let answer = DpkgInventory::default().installed(&names.into_iter().collect());
        if !on_path {
            assert!(answer.unwrap_err().contains("cannot run"));
            return;
        }
        let installed = answer.unwrap();
        assert!(!installed.contains("tprec-never-existed"));
        if Path::new("/var/lib/dpkg/status").is_file() {
            assert!(installed.contains("dpkg"), "dpkg manages this host");
        }
    }
}
