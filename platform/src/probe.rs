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
use std::time::{Duration, Instant};

use crate::detect::{identify, identify_platform, HostReport, HostSources, PlatformReport};
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

/// Ceiling on the metadata response. The answer is a short resource name;
/// an unbounded read from an unauthenticated endpoint is not on offer.
const MAX_METADATA_RESPONSE: u64 = 8 * 1024;

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

    /// Read system **files** under `root` instead of `/`.
    ///
    /// This stages the file-backed sources only. Commands and the metadata
    /// service describe the machine running the test, not the tree, so
    /// under a root they are not consulted at all — including `uname`.
    /// [`Self::detect`] therefore fails on a staged tree rather than
    /// returning an identity that is part fixture and part host; fixture
    /// -driven detection goes through [`crate::detect::identify`] with
    /// recorded [`HostSources`] instead.
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
    /// A command is only ever run when this machine needs its answer —
    /// `sw_vers` on macOS, the package query on a host that turned out to
    /// be a Jetson. That is what makes every command failure meaningful:
    /// a tool that is missing, unreadable, or failing is a broken source
    /// rather than evidence of some other platform, because nothing is
    /// asked speculatively. (`sysctl` exists on Linux too and exits
    /// non-zero for a macOS-only key, so asking everywhere would make
    /// every Linux host look broken.)
    pub fn sources(&self) -> Result<HostSources, PlatformProbeError> {
        // Files first: one of them decides which commands are relevant.
        let os_release = self.read("/etc/os-release")?;
        let cpuinfo = self.read("/proc/cpuinfo")?;
        let nv_tegra_release = self.read("/etc/nv_tegra_release")?;
        let device_tree_model = self.read("/proc/device-tree/model")?;

        // Staged trees exercise the file-backed sources only; the command
        // ones would describe the machine running the test, not the tree.
        let commands = self.root.is_none();
        let apple = commands && cfg!(target_os = "macos");
        // The JetPack package version is only ever read for a machine that
        // already identified itself as a Jetson, so it is only asked for
        // there — and a Jetson that cannot answer it is broken.
        let jetson = commands && cfg!(target_os = "linux") && nv_tegra_release.is_some();

        Ok(HostSources {
            // Deliberately absent under a staged root: borrowing the test
            // host's architecture would let a staged arm64 tree detect as
            // x86_64 and quietly prove nothing. Staged detection fails
            // loudly instead.
            uname_machine: if commands {
                run("uname", &["-m"], ExitPolicy::Strict)?
            } else {
                None
            },
            os_release,
            cpuinfo,
            nv_tegra_release,
            nvidia_jetpack_version: if jetson {
                run(
                    "dpkg-query",
                    &["-W", "-f=${Version}", "nvidia-jetpack"],
                    DPKG_QUERY_NO_MATCH,
                )?
            } else {
                None
            },
            device_tree_model,
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
            hw_memsize: if apple {
                run("sysctl", &["-n", "hw.memsize"], ExitPolicy::Strict)?
            } else {
                None
            },
            gce_machine_type: if commands {
                self.gce_machine_type()?
            } else {
                None
            },
            proc_meminfo: self.read("/proc/meminfo")?,
            pci_devices: self.pci_devices()?,
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

    /// Detect the host and accelerator from one source-gathering pass.
    ///
    /// # Errors
    ///
    /// As [`crate::detect::identify_platform`].
    pub fn detect_platform(&self) -> Result<PlatformReport, PlatformProbeError> {
        identify_platform(&self.sources()?)
    }

    /// The PCI bus as one line per function: `<address> <vendor> <device>
    /// <class>`.
    ///
    /// The first directory enumeration in this module, so it repeats the
    /// discipline every single-file read here already follows: a bus that
    /// is not there is `None` (a Mac and a Jetson have no
    /// `/sys/bus/pci/devices`, and that is a signal), while a bus that is
    /// there and cannot be read is an error. Collapsing those would report
    /// a machine whose sysfs is unreadable as a machine with no devices.
    ///
    /// A single function that disappears mid-enumeration is skipped rather
    /// than failing: hot-unplug is real, and this fact is evidence rather
    /// than something matching depends on.
    fn pci_devices(&self) -> Result<Option<String>, PlatformProbeError> {
        let root = self.path("/sys/bus/pci/devices");
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(PlatformProbeError::Unreadable {
                    source_name: root.display().to_string(),
                    detail: err.to_string(),
                })
            }
        };
        let mut lines = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|err| PlatformProbeError::Unreadable {
                source_name: root.display().to_string(),
                detail: err.to_string(),
            })?;
            let address = entry.file_name().to_string_lossy().into_owned();
            // Only a vanished entry is skipped. Anything else — a denied
            // read, an I/O error, a value that is not UTF-8 — is a source
            // that exists and cannot be read, which this module raises
            // rather than reports as absent. Swallowing them here would
            // hand back an inventory that is quietly incomplete, and an
            // incomplete inventory is exactly the "no accelerator present"
            // answer this reading exists to stop being wrong about.
            let field = |name: &str| -> Result<Option<String>, PlatformProbeError> {
                let path = entry.path().join(name);
                match std::fs::read(&path) {
                    Ok(bytes) => match String::from_utf8(bytes) {
                        Ok(value) => Ok(Some(value.trim().to_string())),
                        Err(err) => Err(PlatformProbeError::Unreadable {
                            source_name: path.display().to_string(),
                            detail: err.to_string(),
                        }),
                    },
                    Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
                    Err(err) => Err(PlatformProbeError::Unreadable {
                        source_name: path.display().to_string(),
                        detail: err.to_string(),
                    }),
                }
            };
            let (Some(vendor), Some(device), Some(class)) =
                (field("vendor")?, field("device")?, field("class")?)
            else {
                // A function that vanished between listing and reading:
                // hot-unplug is real, and it is genuinely absent now.
                continue;
            };
            lines.push(format!("{address} {vendor} {device} {class}"));
        }
        lines.sort();
        Ok(Some(lines.join("\n")))
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
            .map(Some)
            .map_err(|failure| PlatformProbeError::Unreadable {
                source_name: "GCE metadata service".to_string(),
                detail: format!(
                    "host reports as a Compute Engine instance but {METADATA_PATH} gave no machine type ({failure}; budget {}ms)",
                    METADATA_TIMEOUT.as_millis()
                ),
            })
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
/// Every command reaching here is one this machine needs — callers ask
/// only for what the platform actually requires (see
/// [`SystemHostProbe::sources`]). So **a missing binary is a failure**,
/// not absence: `sw_vers` is not optional on macOS, and a service started
/// with a restricted `PATH` must be told its tooling is unreachable
/// rather than quietly reporting the host as an unsupported platform.
///
/// The only non-failure outcome besides success is an exit code `policy`
/// names as this command's way of saying "the thing you asked about is
/// not installed". A tool present but not executable, killed by a signal,
/// or exiting an undocumented code is a broken machine, not a different
/// one.
fn run(
    program: &str,
    args: &[&str],
    policy: ExitPolicy,
) -> Result<Option<String>, PlatformProbeError> {
    let output = match Command::new(program).args(args).output() {
        Ok(output) => output,
        Err(err) => {
            return Err(PlatformProbeError::Unreadable {
                source_name: program.to_string(),
                detail: if err.kind() == ErrorKind::NotFound {
                    format!("`{program}` is not on PATH")
                } else {
                    err.to_string()
                },
            })
        }
    };
    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !text.is_empty() {
            return Ok(Some(text));
        }
        // Succeeded and said nothing. For a command we only asked because
        // this machine needs its answer, silence is a broken source — an
        // empty `sw_vers -productName` would otherwise erase the macOS
        // branch and report an ordinary Mac as an unsupported platform.
        if policy == ExitPolicy::Strict {
            return Err(PlatformProbeError::Unreadable {
                source_name: program.to_string(),
                detail: format!(
                    "`{program} {}` succeeded but printed nothing",
                    args.join(" ")
                ),
            });
        }
        return Ok(None);
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

/// Why a metadata query did not produce a machine type.
///
/// The distinction reaches the operator: a service that answered `403`
/// instantly and a service that never answered need different fixes, and
/// reporting both as a timeout sends the second search in the wrong
/// direction.
#[derive(Debug)]
enum MetadataFailure {
    /// The budget ran out before a complete answer arrived.
    Timeout,
    /// The service answered, but not with a machine type.
    Answered(String),
}

impl std::fmt::Display for MetadataFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => write!(f, "no complete answer within the budget"),
            Self::Answered(detail) => write!(f, "{detail}"),
        }
    }
}

/// One bounded HTTP/1.0 GET against the metadata service.
///
/// Hand-rolled rather than pulling in an HTTP client: this is a single
/// fixed request to a link-local address. `budget` is an **overall
/// deadline**, not a per-read timeout — a peer that trickles one byte at a
/// time must not be able to hold a service start open indefinitely by
/// resetting the clock on every read, and anything answering on an
/// unauthenticated link-local address should be assumed willing to try.
fn query_metadata(addr: &str, path: &str, budget: Duration) -> Result<String, MetadataFailure> {
    let deadline = Instant::now() + budget;
    let remaining = |deadline: Instant| deadline.checked_duration_since(Instant::now());

    let socket = addr
        .parse()
        .map_err(|_| MetadataFailure::Answered(format!("`{addr}` is not an address")))?;
    let mut stream = std::net::TcpStream::connect_timeout(&socket, budget)
        .map_err(|_| MetadataFailure::Timeout)?;

    let left = remaining(deadline).ok_or(MetadataFailure::Timeout)?;
    stream.set_write_timeout(Some(left)).ok();
    let request = format!(
        "GET {path} HTTP/1.0\r\nHost: metadata.google.internal\r\nMetadata-Flavor: Google\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|_| MetadataFailure::Timeout)?;

    // Bounded read: the answer is a short resource name, and an unbounded
    // read from an unauthenticated endpoint is not something to offer.
    let mut reader = stream.take(MAX_METADATA_RESPONSE);
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 512];
    // Out of budget, EOF, or a read error all stop the loop; anything
    // already received still counts if it is a complete, self-describing
    // answer.
    while let Some(left) = remaining(deadline) {
        reader.get_mut().set_read_timeout(Some(left)).ok();
        match reader.read(&mut chunk) {
            Ok(n) if n > 0 => {
                buffer.extend_from_slice(&chunk[..n]);
                // Stop as soon as the response is complete rather than
                // waiting for the peer to close. Waiting for EOF would
                // throw away a correct answer whenever the close lags.
                if response_is_complete(&buffer) {
                    break;
                }
            }
            _ => break,
        }
    }

    let response = String::from_utf8_lossy(&buffer);
    let Some((head, body)) = split_response(&response) else {
        return Err(MetadataFailure::Timeout);
    };
    let status = head.lines().next().unwrap_or_default();
    if !status.split_whitespace().any(|token| token == "200") {
        return Err(MetadataFailure::Answered(format!(
            "metadata service answered `{}`",
            status.trim()
        )));
    }
    // A declared length that the body does not reach means a truncated
    // answer. Accepting it would hand matching a half machine type, which
    // silently makes a supported instance unsupported.
    if let Some(declared) = content_length(head) {
        if body.len() < declared {
            return Err(MetadataFailure::Timeout);
        }
    }
    let body = body.trim();
    if body.is_empty() {
        return Err(MetadataFailure::Answered(
            "metadata service answered 200 with an empty body".to_string(),
        ));
    }
    Ok(body.to_string())
}

fn split_response(response: &str) -> Option<(&str, &str)> {
    response
        .split_once("\r\n\r\n")
        .or_else(|| response.split_once("\n\n"))
}

fn content_length(head: &str) -> Option<usize> {
    head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse().ok())?
    })
}

/// Whether the bytes so far are a complete response, so reading can stop
/// without waiting for the peer to close the socket.
fn response_is_complete(buffer: &[u8]) -> bool {
    let text = String::from_utf8_lossy(buffer);
    let Some((head, body)) = split_response(&text) else {
        return false;
    };
    content_length(head).is_some_and(|declared| body.len() >= declared)
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
    fn an_unreadable_pci_attribute_is_an_error_not_a_missing_device() {
        // The discipline this module opens with, applied to the files
        // inside the directory rather than only to the directory. A denied
        // read must not become "that device is not there": an inventory
        // that is quietly incomplete is the same wrong answer -- "no
        // accelerator present" -- that reading the bus exists to prevent.
        let staging = std::env::temp_dir().join(format!("tp-pci-perm-{}", std::process::id()));
        let device = staging.join("sys/bus/pci/devices/0000:00:04.0");
        std::fs::create_dir_all(&device).expect("stage");
        std::fs::write(device.join("vendor"), "0x10de\n").expect("vendor");
        std::fs::write(device.join("device"), "0x27b8\n").expect("device");
        let class = device.join("class");
        std::fs::write(&class, "0x030000\n").expect("class");

        // Readable first: the device is inventoried.
        let listed = SystemHostProbe::with_root(&staging)
            .pci_devices()
            .expect("readable bus")
            .expect("bus present");
        assert!(listed.contains("0000:00:04.0"), "baseline: {listed}");

        // Now make one attribute unreadable. `.ok()` would have skipped the
        // device and reported an empty bus.
        let mut perms = std::fs::metadata(&class).expect("metadata").permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o000);
        std::fs::set_permissions(&class, perms).expect("chmod");

        let result = SystemHostProbe::with_root(&staging).pci_devices();
        // Running as root defeats the permission bit; skip rather than
        // assert something the environment cannot produce.
        if std::fs::read(&class).is_err() {
            let err = result.expect_err("an unreadable attribute is an error");
            assert!(
                format!("{err}").contains("class"),
                "the error names the attribute path: {err}"
            );
        }

        let mut perms = std::fs::metadata(&class).expect("metadata").permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o644);
        std::fs::set_permissions(&class, perms).ok();
        std::fs::remove_dir_all(&staging).ok();
    }

    #[test]
    fn a_vanished_pci_function_is_skipped_rather_than_fatal() {
        // Hot-unplug between listing and reading is real, and a function
        // that is genuinely gone is absent rather than unreadable.
        let staging = std::env::temp_dir().join(format!("tp-pci-gone-{}", std::process::id()));
        let present = staging.join("sys/bus/pci/devices/0000:00:04.0");
        let partial = staging.join("sys/bus/pci/devices/0000:00:05.0");
        std::fs::create_dir_all(&present).expect("stage");
        std::fs::create_dir_all(partial).expect("stage");
        std::fs::write(present.join("vendor"), "0x10de\n").expect("vendor");
        std::fs::write(present.join("device"), "0x27b8\n").expect("device");
        std::fs::write(present.join("class"), "0x030000\n").expect("class");
        // `partial` has no attribute files at all, as a removed device does.

        let listed = SystemHostProbe::with_root(&staging)
            .pci_devices()
            .expect("a vanished function is not an error")
            .expect("bus present");
        assert!(listed.contains("0000:00:04.0"));
        assert!(!listed.contains("0000:00:05.0"));

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
        assert!(query_metadata(
            "169.254.169.254:9",
            METADATA_PATH,
            Duration::from_millis(100)
        )
        .is_err());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "detection must stay bounded, took {:?}",
            started.elapsed()
        );
    }

    /// Serve one fixed reply on loopback, optionally holding the socket
    /// open afterwards, and return the address.
    fn serve_once(reply: &'static str, linger: Duration) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                let _ = socket.write_all(reply.as_bytes());
                let _ = socket.flush();
                std::thread::sleep(linger);
            }
        });
        addr
    }

    #[test]
    fn a_complete_answer_is_used_even_if_the_peer_never_closes() {
        // read_to_end only returns at EOF, so waiting for the close threw
        // away a correct answer whenever the FIN lagged — and because a
        // missing machine type is now a hard error, that failed detection
        // outright on a healthy instance. Content-Length says when the
        // answer is complete, so the close is irrelevant.
        let addr = serve_once(
            "HTTP/1.0 200 OK\r\nContent-Length: 37\r\n\r\nprojects/1/machineTypes/g2-standard-8",
            Duration::from_secs(30),
        );
        let started = std::time::Instant::now();
        let answer = query_metadata(&addr, METADATA_PATH, Duration::from_millis(500));
        assert_eq!(
            answer.expect("a complete answer must be used"),
            "projects/1/machineTypes/g2-standard-8"
        );
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "must not wait for the peer to close, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_truncated_answer_is_never_accepted() {
        // Half a machine type is worse than none: it would match no row and
        // report a healthy instance as unsupported, silently.
        let addr = serve_once(
            "HTTP/1.0 200 OK\r\nContent-Length: 37\r\n\r\nprojects/1/machineTypes/g2-stan",
            Duration::from_secs(30),
        );
        assert!(
            query_metadata(&addr, METADATA_PATH, Duration::from_millis(300)).is_err(),
            "a body shorter than its declared length must be rejected"
        );
    }

    #[test]
    fn a_trickling_peer_cannot_outlast_the_budget() {
        // The timeout is a deadline, not a per-read allowance: a peer that
        // sends a byte at a time must not be able to reset the clock and
        // hold a service start open indefinitely.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                for _ in 0..4096 {
                    if socket.write_all(b"x").is_err() {
                        return;
                    }
                    let _ = socket.flush();
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        });
        let started = std::time::Instant::now();
        assert!(query_metadata(&addr, METADATA_PATH, Duration::from_millis(200)).is_err());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the budget is an overall deadline, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_non_200_answer_says_so_rather_than_blaming_a_timeout() {
        let addr = serve_once(
            "HTTP/1.0 403 Forbidden\r\nContent-Length: 0\r\n\r\n",
            Duration::from_millis(10),
        );
        let err = query_metadata(&addr, METADATA_PATH, Duration::from_millis(500))
            .expect_err("403 is not a machine type");
        assert!(
            err.to_string().contains("403"),
            "an instant refusal must not read as a timeout: {err}"
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
    fn a_missing_required_command_is_a_broken_source_not_another_platform() {
        // Nothing is asked speculatively, so a tool that is not there is a
        // tool this machine was supposed to have. The realistic case is a
        // service started with a restricted PATH: reporting `None` would
        // turn that into "your platform is unsupported".
        for policy in [ExitPolicy::Strict, DPKG_QUERY_NO_MATCH] {
            let err = run("tp-definitely-not-a-real-binary", &[], policy)
                .expect_err("a required command that is missing must not read as absence");
            match err {
                PlatformProbeError::Unreadable {
                    source_name,
                    detail,
                } => {
                    assert_eq!(source_name, "tp-definitely-not-a-real-binary");
                    assert!(detail.contains("PATH"), "says why: {detail}");
                }
                other @ PlatformProbeError::Unrecognized { .. } => {
                    panic!("expected Unreadable, got {other:?}")
                }
            }
        }
    }

    #[test]
    fn a_missing_file_is_absence() {
        // Files stay different from commands: no `/etc/os-release` is how
        // macOS is recognized, so file absence remains meaningful.
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
