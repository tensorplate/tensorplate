// SPDX-License-Identifier: Apache-2.0
//
// Reading host identity sources off a live machine.
//
// This is the only part of detection that touches the world. It gathers
// raw source content and hands it to [`crate::detect::identify`], which is
// pure — so the interesting logic stays testable from fixtures and this
// module stays small enough to review by eye.
//
// A source that is not there yields `None`, because absence is how
// platforms are told apart — no `/etc/os-release` on macOS, no `sw_vers`
// on Linux. A source that *is* there but cannot be read is an error.
// Those two must never look alike: collapsing them would report a machine
// whose `/etc/os-release` is unreadable as a machine that has no OS
// identity, which reaches the operator as "your platform is unsupported"
// when the truth is "I could not read it".

use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::detect::{identify, HostReport, HostSources};
use crate::error::PlatformProbeError;
use crate::identity::{HostIdentity, HostProbe};

/// The GCE metadata service, addressed by its link-local IP rather than by
/// name so a broken resolver cannot turn detection into a DNS timeout.
const METADATA_ADDR: &str = "169.254.169.254:80";
const METADATA_PATH: &str = "/computeMetadata/v1/instance/machine-type";

/// How long the metadata service gets. It is on the local link and answers
/// in single-digit milliseconds; anything slower is a machine that is not
/// on GCE, and detection must not stall a service start over it.
const METADATA_TIMEOUT: Duration = Duration::from_millis(250);

/// Reads host identity from the running machine.
#[derive(Clone, Debug, Default)]
pub struct SystemHostProbe {
    /// Root to read system files under. Empty in production; tests point
    /// it at a staged tree.
    root: Option<std::path::PathBuf>,
}

impl SystemHostProbe {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Read system files under `root` instead of `/`.
    #[must_use]
    pub fn with_root(root: impl Into<std::path::PathBuf>) -> Self {
        Self {
            root: Some(root.into()),
        }
    }

    fn path(&self, absolute: &str) -> std::path::PathBuf {
        match &self.root {
            Some(root) => root.join(absolute.trim_start_matches('/')),
            None => std::path::PathBuf::from(absolute),
        }
    }

    fn read(&self, absolute: &str) -> Result<Option<String>, PlatformProbeError> {
        read_lossy(&self.path(absolute))
    }

    /// Gather every source this machine offers.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformProbeError::Unreadable`] when a source exists but
    /// cannot be read — a permission error, an unreadable device node, a
    /// tool that is present but not executable, or a command that fails in
    /// a way it has no documented meaning for. Only genuine *absence*
    /// becomes `None`, because absence is how platforms are told apart and
    /// a source that is merely unreadable must never be mistaken for a
    /// platform that does not have it. Getting this wrong reports a
    /// supported machine as unsupported.
    ///
    /// Commands are asked only of the platforms they belong to. `sysctl`
    /// exists on Linux too and exits non-zero for a macOS-only key, so
    /// running it everywhere would turn every Linux host into a detection
    /// failure the moment unexpected exits stopped being ignored.
    pub fn sources(&self) -> Result<HostSources, PlatformProbeError> {
        // Staged trees exercise the file-backed sources only; the command
        // ones would describe the machine running the test, not the tree.
        let commands = self.root.is_none();
        let apple = commands && cfg!(target_os = "macos");
        let linux = commands && cfg!(target_os = "linux");

        Ok(HostSources {
            uname_machine: run("uname", &["-m"], ExitPolicy::Strict)?,
            os_release: self.read("/etc/os-release")?,
            cpuinfo: self.read("/proc/cpuinfo")?,
            nv_tegra_release: self.read("/etc/nv_tegra_release")?,
            nvidia_jetpack_version: if linux {
                run(
                    "dpkg-query",
                    &["-W", "-f=${Version}", "nvidia-jetpack"],
                    DPKG_QUERY_NO_MATCH,
                )?
            } else {
                None
            },
            device_tree_model: self.read("/proc/device-tree/model")?,
            sw_vers_product_name: if apple {
                run("sw_vers", &["-productName"], ExitPolicy::Strict)?
            } else {
                None
            },
            sw_vers_product_version: if apple {
                run("sw_vers", &["-productVersion"], ExitPolicy::Strict)?
            } else {
                None
            },
            sw_vers_build_version: if apple {
                run("sw_vers", &["-buildVersion"], ExitPolicy::Strict)?
            } else {
                None
            },
            cpu_brand: if apple {
                run(
                    "sysctl",
                    &["-n", "machdep.cpu.brand_string"],
                    ExitPolicy::Strict,
                )?
            } else {
                None
            },
            gce_machine_type: self.gce_machine_type()?,
        })
    }

    /// Detect the host, keeping the exact facts alongside the identity.
    ///
    /// # Errors
    ///
    /// As [`crate::detect::identify`].
    pub fn detect(&self) -> Result<HostReport, PlatformProbeError> {
        identify(&self.sources()?)
    }

    /// The machine type, on machines that have one.
    ///
    /// The metadata service is only contacted when the host already looks
    /// like a Compute Engine instance. A physical workstation must come
    /// back with no machine type — its row declares none — and must never
    /// pay a network timeout to find that out.
    fn gce_machine_type(&self) -> Result<Option<String>, PlatformProbeError> {
        if !self.looks_like_gce()? {
            return Ok(None);
        }
        // A machine that says it is an instance but will not answer is a
        // broken source, not a machine without a shape: reporting `None`
        // would strip it of the very field its row is scoped to and quietly
        // make that row unmatchable.
        query_metadata(METADATA_ADDR, METADATA_PATH, METADATA_TIMEOUT)
            .ok_or_else(|| PlatformProbeError::Unreadable {
                source_name: "GCE metadata service".to_string(),
                detail: format!(
                    "host reports as a Compute Engine instance but {METADATA_PATH} did not answer within {}ms",
                    METADATA_TIMEOUT.as_millis()
                ),
            })
            .map(Some)
    }

    fn looks_like_gce(&self) -> Result<bool, PlatformProbeError> {
        // Set by the firmware, so it is readable without privileges and
        // without asking the network anything.
        Ok(self
            .read("/sys/class/dmi/id/product_name")?
            .is_some_and(|name| name.trim() == "Google Compute Engine"))
    }
}

impl HostProbe for SystemHostProbe {
    fn detect_host(&self) -> Result<HostIdentity, PlatformProbeError> {
        self.detect().map(|report| report.identity)
    }
}

/// Read a source file. A file that is not there is `None`; a file that is
/// there but unreadable is an error.
fn read_lossy(path: &Path) -> Result<Option<String>, PlatformProbeError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(PlatformProbeError::Unreadable {
            source_name: path.display().to_string(),
            detail: err.to_string(),
        }),
    }
}

/// What a non-zero exit from a detection command means.
///
/// It is not the same answer for every command. `dpkg-query` exits 1 to
/// say a package is not installed, which is a fact about the machine.
/// `uname` exiting non-zero says nothing about the machine except that
/// something is wrong with it. Reading the second as the first is how a
/// broken source turns into "unsupported platform".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExitPolicy {
    /// Any non-zero exit is a broken source.
    Strict,
    /// These exit codes are the command's way of reporting absence;
    /// anything else is a broken source.
    AbsentOn(&'static [i32]),
}

/// `dpkg-query` exits 1 when no package matches, which is how a Jetson
/// without the `nvidia-jetpack` package answers.
const DPKG_QUERY_NO_MATCH: ExitPolicy = ExitPolicy::AbsentOn(&[1]);

/// Run a command and return its trimmed stdout.
///
/// A tool that does not exist is `None` — that is how a platform says it
/// has no `sw_vers`. Everything else is a failure unless `policy` names
/// the exit code as this command's way of saying "absent": a tool present
/// but not executable, killed by a signal, or exiting a code it has no
/// documented meaning for is a broken machine, not a different one.
///
/// Commands are only invoked on platforms they belong to (see
/// [`SystemHostProbe::sources`]), so an unexpected exit here really is
/// unexpected rather than a Linux box being asked a macOS question.
fn run(
    program: &str,
    args: &[&str],
    policy: ExitPolicy,
) -> Result<Option<String>, PlatformProbeError> {
    let output = match Command::new(program).args(args).output() {
        Ok(output) => output,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(PlatformProbeError::Unreadable {
                source_name: program.to_string(),
                detail: err.to_string(),
            })
        }
    };
    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Ok((!text.is_empty()).then_some(text));
    }
    if let (ExitPolicy::AbsentOn(accepted), Some(code)) = (policy, output.status.code()) {
        if accepted.contains(&code) {
            return Ok(None);
        }
    }
    Err(PlatformProbeError::Unreadable {
        source_name: program.to_string(),
        detail: if let Some(code) = output.status.code() {
            format!(
                "`{program} {}` exited {code}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )
        } else {
            format!("`{program} {}` was terminated by a signal", args.join(" "))
        },
    })
}

/// One bounded HTTP/1.0 GET against the metadata service.
///
/// Hand-rolled rather than pulling in an HTTP client: this is a single
/// fixed request to a link-local address, and every phase carries a
/// timeout so detection cannot hang a service start.
fn query_metadata(addr: &str, path: &str, timeout: Duration) -> Option<String> {
    let socket = addr.parse().ok()?;
    let mut stream = std::net::TcpStream::connect_timeout(&socket, timeout).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;

    let request = format!(
        "GET {path} HTTP/1.0\r\nHost: metadata.google.internal\r\nMetadata-Flavor: Google\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).ok()?;

    // Bounded read: the answer is a short resource name, and an unbounded
    // read from an unauthenticated endpoint is not something to offer.
    let mut buffer = Vec::new();
    stream.take(8 * 1024).read_to_end(&mut buffer).ok()?;
    let response = String::from_utf8_lossy(&buffer);

    let (head, body) = response
        .split_once("\r\n\r\n")
        .or_else(|| response.split_once("\n\n"))?;
    let status_ok = head
        .lines()
        .next()
        .is_some_and(|line| line.contains(" 200"));
    if !status_ok {
        return None;
    }
    let body = body.trim();
    (!body.is_empty()).then(|| body.to_string())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_machine_that_is_not_gce_is_never_asked_for_a_machine_type() {
        // The row for the physical workstation declares no machine type, so
        // detection must produce none — without a network round trip.
        let staging = std::env::temp_dir().join(format!("tp-probe-{}", std::process::id()));
        std::fs::create_dir_all(staging.join("sys/class/dmi/id")).expect("stage");
        std::fs::write(
            staging.join("sys/class/dmi/id/product_name"),
            "Precision 7960 Tower\n",
        )
        .expect("write product name");

        let probe = SystemHostProbe::with_root(&staging);
        assert!(!probe.looks_like_gce().expect("dmi readable"));
        assert_eq!(probe.gce_machine_type().expect("no query attempted"), None);

        std::fs::remove_dir_all(&staging).ok();
    }

    #[test]
    fn a_gce_host_is_recognized_from_firmware_alone() {
        let staging = std::env::temp_dir().join(format!("tp-probe-gce-{}", std::process::id()));
        std::fs::create_dir_all(staging.join("sys/class/dmi/id")).expect("stage");
        std::fs::write(
            staging.join("sys/class/dmi/id/product_name"),
            "Google Compute Engine\n",
        )
        .expect("write product name");

        assert!(SystemHostProbe::with_root(&staging)
            .looks_like_gce()
            .expect("dmi readable"));

        std::fs::remove_dir_all(&staging).ok();
    }

    #[test]
    fn an_unreachable_metadata_service_yields_no_machine_type() {
        // Port 9 discards; connecting must fail or time out rather than
        // producing a value or hanging.
        let started = std::time::Instant::now();
        assert_eq!(
            query_metadata(
                "169.254.169.254:9",
                METADATA_PATH,
                Duration::from_millis(100)
            ),
            None
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "detection must stay bounded, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_command_failing_in_an_undocumented_way_is_an_error() {
        // `uname`, `sw_vers`, and `sysctl` have no "absent" exit code — if
        // one of them fails, the machine is broken, and swallowing that
        // reports it as an unsupported platform instead.
        let err = run("sh", &["-c", "echo boom >&2; exit 3"], ExitPolicy::Strict)
            .expect_err("a strict command must not swallow a non-zero exit");
        match err {
            PlatformProbeError::Unreadable {
                source_name,
                detail,
            } => {
                assert_eq!(source_name, "sh");
                assert!(detail.contains("exited 3"), "names the exit code: {detail}");
                assert!(detail.contains("boom"), "carries stderr: {detail}");
            }
            other @ PlatformProbeError::Unrecognized { .. } => {
                panic!("expected Unreadable, got {other:?}")
            }
        }
    }

    #[test]
    fn only_the_package_querys_documented_absent_code_reads_as_absence() {
        // dpkg-query exits 1 for "no package matches", which is a fact
        // about the machine. Any other code is a broken source.
        assert_eq!(
            run("sh", &["-c", "exit 1"], DPKG_QUERY_NO_MATCH)
                .expect("the documented absent code is absence"),
            None
        );
        assert!(
            run("sh", &["-c", "exit 2"], DPKG_QUERY_NO_MATCH).is_err(),
            "an undocumented exit code from the package query is still a failure"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_command_killed_by_a_signal_is_an_error() {
        let err = run("sh", &["-c", "kill -TERM $$"], DPKG_QUERY_NO_MATCH)
            .expect_err("a signalled command has no exit code to accept");
        assert!(
            err.to_string().contains("signal"),
            "says what happened: {err}"
        );
    }

    #[test]
    fn missing_commands_and_files_are_absence_not_failure() {
        assert_eq!(
            run("tp-definitely-not-a-real-binary", &[], ExitPolicy::Strict)
                .expect("absent tool is not an error"),
            None
        );
        assert_eq!(
            read_lossy(Path::new("/tp/definitely/not/here")).expect("absent file is not an error"),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_source_is_an_error_not_an_absent_one() {
        // The distinction this whole module turns on: a machine whose
        // `/etc/os-release` cannot be read must not look like a machine that
        // has no `/etc/os-release`. The first is a broken source; the second
        // is macOS. Collapsing them reports a supported host as unsupported.
        use std::os::unix::fs::PermissionsExt;

        let staging = std::env::temp_dir().join(format!("tp-probe-perm-{}", std::process::id()));
        std::fs::create_dir_all(staging.join("etc")).expect("stage");
        let path = staging.join("etc/os-release");
        std::fs::write(&path, "NAME=\"Ubuntu\"\n").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("chmod");

        let result = read_lossy(&path);

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).ok();
        std::fs::remove_dir_all(&staging).ok();

        match result {
            Err(PlatformProbeError::Unreadable { source_name, .. }) => {
                assert!(source_name.contains("os-release"), "names the source");
            }
            other => panic!("an unreadable source must not read as absent: {other:?}"),
        }
    }
}
