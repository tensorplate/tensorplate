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

use tensorplate_protocol::install_paths::MACHINE_TYPE_RECORD_PATH;

use crate::detect::{identify, is_compute_engine, HostReport, HostSources};
use crate::error::PlatformProbeError;
use crate::identity::{HostIdentity, HostProbe};
use crate::machine_type_record::{MachineTypeRecord, RecordWrite};

/// Firmware product name. Readable without privileges, and how a Compute
/// Engine instance is recognized without asking the network anything.
const DMI_PRODUCT_NAME_PATH: &str = "/sys/class/dmi/id/product_name";

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

/// Ceiling on the machine-type record. A record is a few hundred bytes.
const MAX_MACHINE_TYPE_RECORD: u64 = 4 * 1024;

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
    /// under a root they are not consulted at all — including `uname` — and
    /// neither is the machine-type record, which is only ever read when the
    /// metadata service was asked and could not be reached. Writing the
    /// record does honour the root.
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
        let dmi_product_name = self.read(DMI_PRODUCT_NAME_PATH)?;

        // Staged trees exercise the file-backed sources only; the command
        // ones would describe the machine running the test, not the tree.
        let commands = self.root.is_none();
        let apple = commands && cfg!(target_os = "macos");
        // The JetPack package version is only ever read for a machine that
        // already identified itself as a Jetson, so it is only asked for
        // there — and a Jetson that cannot answer it is broken.
        let jetson = commands && cfg!(target_os = "linux") && nv_tegra_release.is_some();
        let (gce_machine_type, machine_type_record) = if commands {
            self.machine_type_sources(dmi_product_name.as_deref(), || {
                query_metadata(METADATA_ADDR, METADATA_PATH, METADATA_TIMEOUT)
            })?
        } else {
            (None, None)
        };

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
            boot_id: if dmi_product_name.as_deref().is_some_and(is_compute_engine) {
                self.read("/proc/sys/kernel/random/boot_id")?
            } else {
                None
            },
            dmi_product_name,
            gce_machine_type,
            machine_type_record,
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

    /// The machine-type sources: `(live answer, recorded machine type)`.
    ///
    /// `query` asks the metadata service, and is only called when the
    /// firmware product name says this is a Compute Engine instance. A
    /// physical workstation must come back with no machine type — its row
    /// declares none — and must never pay a network timeout to find that
    /// out.
    ///
    /// Only a service that could not be reached — the connect or the request
    /// failed, or nothing at all came back within the budget — lets the
    /// record be read. A service that sent anything, closed, or reset, but
    /// did not answer with a machine type, is a broken source and stays one:
    /// falling back there would let a record outvote the authority that just
    /// answered. A record that is absent is `None` here;
    /// [`crate::detect::identify`] turns that into an error, because an
    /// instance with no machine type is admitted as an unvalidated shape.
    fn machine_type_sources(
        &self,
        dmi_product_name: Option<&str>,
        query: impl FnOnce() -> Result<String, MetadataFailure>,
    ) -> Result<(Option<String>, Option<String>), PlatformProbeError> {
        if !dmi_product_name.is_some_and(is_compute_engine) {
            return Ok((None, None));
        }
        match query() {
            Ok(body) => Ok((Some(body), None)),
            Err(MetadataFailure::Timeout) => Ok((
                None,
                read_machine_type_record(&self.path(MACHINE_TYPE_RECORD_PATH))?,
            )),
            Err(failure @ MetadataFailure::Answered(_)) => Err(PlatformProbeError::Unreadable {
                source_name: "GCE metadata service".to_string(),
                detail: format!(
                    "host reports as a Compute Engine instance but {METADATA_PATH} gave no machine type ({failure}; budget {}ms)",
                    METADATA_TIMEOUT.as_millis()
                ),
            }),
        }
    }

    /// Record the machine type a live metadata answer in `sources` names,
    /// bound to the local facts it was answered on, for detection to use
    /// when the metadata service cannot be reached.
    ///
    /// Writes nothing, and reports [`RecordWrite::NotApplicable`], when
    /// `sources` carry no live answer — including sources whose machine type
    /// was itself resolved from a record. Writes nothing, and reports
    /// [`RecordWrite::Unchanged`], when the file already holds exactly these
    /// bytes. Writes nothing, and reports [`RecordWrite::FactsUnavailable`],
    /// when there is a live answer but a fact it would be bound to cannot be
    /// read. Otherwise replaces the file atomically, created `0640`: never
    /// world-readable, and group-readable under the agent unit's umask.
    ///
    /// # Errors
    ///
    /// [`PlatformProbeError::Unreadable`] naming the record when it cannot be
    /// written. The state directory is never created here: it belongs to the
    /// installer, and a missing one is a broken install.
    pub fn write_machine_type_record(
        &self,
        sources: &HostSources,
    ) -> Result<RecordWrite, PlatformProbeError> {
        let record = match MachineTypeRecord::for_live_sources(sources) {
            Ok(Some(record)) => record,
            Ok(None) => return Ok(RecordWrite::NotApplicable),
            Err(fact) => return Ok(RecordWrite::FactsUnavailable(fact)),
        };
        let target = self.path(MACHINE_TYPE_RECORD_PATH);
        let failed = |detail: String| PlatformProbeError::Unreadable {
            source_name: target.display().to_string(),
            detail,
        };
        let body = record
            .to_json()
            .map_err(|err| failed(format!("cannot serialize the record: {err}")))?;
        // Pin the directory for the comparison, temporary creation, and rename.
        // No component of an agent-writable path is followed as a symlink.
        let directory = RecordDirectory::open(&target)
            .map_err(|err| failed(format!("cannot open the record directory: {err}")))?;
        if read_machine_type_record_in(&directory, &target)
            .ok()
            .flatten()
            .as_deref()
            == Some(body.as_str())
        {
            return Ok(RecordWrite::Unchanged);
        }
        directory
            .replace(body.as_bytes())
            .map_err(|err| failed(format!("cannot write the machine-type record: {err}")))?;
        Ok(RecordWrite::Written)
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

/// Read the machine-type record without following a link and without
/// reading more than any record can be.
///
/// Absent is `None`, and a record that cannot be read -- typically an
/// operator outside the `tensorplate` group, which owns the state directory
/// -- is [`PlatformProbeError::Unreadable`], as for every other source. A
/// path that is not a regular file, or a file larger than any record, is
/// [`PlatformProbeError::IdentityUnestablished`]: there is something there,
/// and it is not a record detection will use.
///
/// Each directory component is opened relative to its pinned predecessor,
/// refusing symlinks. The final file is opened with `O_NOFOLLOW` and checked
/// through its descriptor before any bytes are read. Replacing a pathname
/// while an elevated doctor is recording can therefore never redirect reads.
fn read_machine_type_record(path: &Path) -> Result<Option<String>, PlatformProbeError> {
    match RecordDirectory::open(path) {
        Ok(directory) => read_machine_type_record_in(&directory, path),
        Err(err) => record_open_error(path, &err),
    }
}

fn record_open_error(
    path: &Path,
    err: &std::io::Error,
) -> Result<Option<String>, PlatformProbeError> {
    if err.kind() == ErrorKind::NotFound {
        return Ok(None);
    }
    #[cfg(unix)]
    if [rustix::io::Errno::LOOP, rustix::io::Errno::NOTDIR]
        .iter()
        .any(|code| err.raw_os_error() == Some(code.raw_os_error()))
    {
        return Err(unusable_record(
            path,
            "is not a regular file or has a symlinked directory component",
        ));
    }
    Err(PlatformProbeError::Unreadable {
        source_name: path.display().to_string(),
        detail: err.to_string(),
    })
}

fn unusable_record(path: &Path, what: &str) -> PlatformProbeError {
    PlatformProbeError::IdentityUnestablished {
        source_name: path.display().to_string(),
        detail: format!(
            "the machine-type record {what}; start tensorplate-agent once while the metadata \
             service is reachable to record it again"
        ),
    }
}

fn read_machine_type_record_in(
    directory: &RecordDirectory,
    path: &Path,
) -> Result<Option<String>, PlatformProbeError> {
    let unreadable = |err: std::io::Error| PlatformProbeError::Unreadable {
        source_name: path.display().to_string(),
        detail: err.to_string(),
    };
    let file = match directory.read() {
        Ok(file) => file,
        Err(err) => return record_open_error(path, &err),
    };
    let metadata = file.metadata().map_err(unreadable)?;
    if !metadata.file_type().is_file() {
        return Err(unusable_record(path, "is not a regular file"));
    }
    if metadata.len() > MAX_MACHINE_TYPE_RECORD {
        return Err(unusable_record(
            path,
            &format!("is larger than {MAX_MACHINE_TYPE_RECORD} bytes"),
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_MACHINE_TYPE_RECORD + 1)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    if u64::try_from(bytes.len()).map_or(true, |len| len > MAX_MACHINE_TYPE_RECORD) {
        return Err(unusable_record(
            path,
            &format!("is larger than {MAX_MACHINE_TYPE_RECORD} bytes"),
        ));
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// A pinned parent directory and one final component. Path traversal happens
/// exactly once; subsequent opens, unlinks, and renames use this descriptor.
struct RecordDirectory {
    #[cfg(unix)]
    directory: std::fs::File,
    #[cfg(unix)]
    name: std::ffi::OsString,
}

#[cfg(unix)]
impl RecordDirectory {
    fn open(path: &Path) -> std::io::Result<Self> {
        use rustix::fs::{open, openat, Mode, OFlags};
        use std::path::Component;

        let invalid = || std::io::Error::new(ErrorKind::InvalidInput, "invalid record path");
        let name = path.file_name().ok_or_else(invalid)?.to_os_string();
        let parent = path.parent().ok_or_else(invalid)?;
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mut directory = open(
            if path.is_absolute() { "/" } else { "." },
            flags,
            Mode::empty(),
        )?;
        for component in parent.components() {
            match component {
                Component::Normal(name) => {
                    directory = openat(&directory, name, flags, Mode::empty())?;
                }
                Component::RootDir | Component::CurDir => {}
                Component::ParentDir | Component::Prefix(_) => return Err(invalid()),
            }
        }
        Ok(Self {
            directory: directory.into(),
            name,
        })
    }

    fn read(&self) -> std::io::Result<std::fs::File> {
        use rustix::fs::{openat, Mode, OFlags};

        // NONBLOCK keeps a swapped FIFO from waiting for a writer. Its
        // descriptor is rejected as nonregular without reading from it.
        Ok(openat(
            &self.directory,
            &self.name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?
        .into())
    }

    fn replace(&self, body: &[u8]) -> std::io::Result<()> {
        use rustix::fs::{openat, renameat, unlinkat, AtFlags, Mode, OFlags};

        let mut temporary = self.name.clone();
        temporary.push(".tmp");
        // Exclusive creation never opens an existing file or follows a link.
        let create = || {
            openat(
                &self.directory,
                &temporary,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o640),
            )
        };
        let mut file: std::fs::File = match create() {
            Err(rustix::io::Errno::EXIST) => {
                unlinkat(&self.directory, &temporary, AtFlags::empty())?;
                create()?
            }
            created => created?,
        }
        .into();
        file.write_all(body)?;
        file.sync_all()?;
        renameat(&self.directory, &temporary, &self.directory, &self.name)?;
        // Match the agent state store's best-effort directory durability.
        let _ = self.directory.sync_all();
        Ok(())
    }
}

// GCE records are only used on Unix. Other platforms must fail closed rather
// than silently use a pathname implementation with weaker link guarantees.
#[cfg(not(unix))]
impl RecordDirectory {
    fn open(_path: &Path) -> std::io::Result<Self> {
        Err(std::io::Error::new(
            ErrorKind::Unsupported,
            "machine-type record IO requires Unix",
        ))
    }

    fn read(&self) -> std::io::Result<std::fs::File> {
        Err(std::io::Error::new(
            ErrorKind::Unsupported,
            "machine-type record IO requires Unix",
        ))
    }

    fn replace(&self, _body: &[u8]) -> std::io::Result<()> {
        Err(std::io::Error::new(
            ErrorKind::Unsupported,
            "machine-type record IO requires Unix",
        ))
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
    /// The service was not reached: the connect or the request failed, or
    /// nothing at all came back before the budget ran out.
    Timeout,
    /// The service was reached -- it sent something, closed, or reset --
    /// but did not answer with a machine type.
    Answered(String),
}

impl std::fmt::Display for MetadataFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => write!(f, "no answer within the budget"),
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
    // The extra byte distinguishes the cap from an orderly EOF. Treating
    // `Take`'s synthetic EOF as a close could accept a truncated response.
    let mut reader = stream.take(MAX_METADATA_RESPONSE + 1);
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 512];
    // Out of budget, EOF (or the size cap), or a read error all stop the
    // loop; anything already received still counts if it is a complete,
    // self-describing answer.
    let mut out_of_budget = false;
    let mut reached_eof = false;
    loop {
        let Some(left) = remaining(deadline) else {
            out_of_budget = true;
            break;
        };
        reader.get_mut().set_read_timeout(Some(left)).ok();
        match reader.read(&mut chunk) {
            Ok(0) => {
                reached_eof = true;
                break;
            }
            Ok(n) => {
                buffer.extend_from_slice(&chunk[..n]);
                // Stop as soon as the response is complete rather than
                // waiting for the peer to close. Waiting for EOF would
                // throw away a correct answer whenever the close lags.
                if buffer.len() as u64 > MAX_METADATA_RESPONSE || response_is_complete(&buffer) {
                    break;
                }
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) => {
                out_of_budget = matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut);
                break;
            }
        }
    }

    // Unreachable means nothing came back at all before the budget ran out.
    // A peer that closed, reset, or sent anything was reached, and whatever
    // it sent that is not a machine type is its answer: only the first case
    // may let a recorded machine type stand in for the live one.
    if buffer.is_empty() {
        return Err(if out_of_budget {
            MetadataFailure::Timeout
        } else {
            MetadataFailure::Answered(
                "metadata service closed the connection without answering".to_string(),
            )
        });
    }
    let incomplete = || {
        MetadataFailure::Answered(
            "metadata service sent an incomplete or unparseable response".to_string(),
        )
    };
    if buffer.len() as u64 > MAX_METADATA_RESPONSE {
        return Err(incomplete());
    }
    let response = std::str::from_utf8(&buffer).map_err(|_| incomplete())?;
    let Some((head, body)) = split_response(response) else {
        return Err(incomplete());
    };
    let status = head.lines().next().unwrap_or_default();
    let mut status_fields = status.split(' ');
    let version = status_fields.next();
    let code = status_fields.next();
    if !matches!(version, Some("HTTP/1.0" | "HTTP/1.1"))
        || code != Some("200")
        || status.bytes().any(|byte| byte.is_ascii_control())
    {
        // One journal line: whatever the peer put in its status line is
        // escaped rather than printed.
        return Err(MetadataFailure::Answered(format!(
            "metadata service answered `{}`",
            status.trim().escape_debug()
        )));
    }
    // A length-framed body must match its one valid declared length. An
    // unframed body is complete only at an orderly close, never merely
    // because the deadline expired after a plausible resource name.
    match content_length(head).map_err(|()| incomplete())? {
        Some(declared) if body.len() == declared => {}
        None if reached_eof => {}
        Some(_) | None => return Err(incomplete()),
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

fn content_length(head: &str) -> Result<Option<usize>, ()> {
    let mut declared = None;
    for line in head.lines().skip(1) {
        let (name, value) = line.split_once(':').ok_or(())?;
        // No folded lines or whitespace before the colon: permissive
        // parsing here can disagree with a peer about response framing.
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
            || value
                .bytes()
                .any(|byte| byte.is_ascii_control() && byte != b'\t')
        {
            return Err(());
        }
        // This HTTP/1.0 client does not decode transfer encodings. In
        // particular, a simultaneous Content-Length must not hide one.
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(());
        }
        if name.eq_ignore_ascii_case("content-length") {
            let value = value.trim_matches([' ', '\t']);
            if declared.is_some()
                || value.is_empty()
                || !value.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(());
            }
            let length = value.parse::<usize>().map_err(|_| ())?;
            if length as u64 > MAX_METADATA_RESPONSE {
                return Err(());
            }
            declared = Some(length);
        }
    }
    Ok(declared)
}

/// Whether the bytes so far are a complete response, so reading can stop
/// without waiting for the peer to close the socket.
fn response_is_complete(buffer: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(buffer) else {
        return false;
    };
    let Some((head, body)) = split_response(text) else {
        return false;
    };
    matches!(content_length(head), Ok(Some(declared)) if body.len() >= declared)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Sources for a live Compute Engine answer on a small synthetic shape.
    fn live_gce_sources(machine_type: &str) -> HostSources {
        HostSources {
            boot_id: Some("12345678-1234-4234-8234-123456789abc\n".to_string()),
            cpuinfo: Some(
                "processor\t: 0\nvendor_id\t: GenuineIntel\nprocessor\t: 1\n".to_string(),
            ),
            proc_meminfo: Some("MemTotal:        2048 kB\n".to_string()),
            pci_devices: Some("0000:00:03.0 0x10de 0x27b8 0x030200".to_string()),
            dmi_product_name: Some("Google Compute Engine\n".to_string()),
            gce_machine_type: Some(format!("projects/REDACTED/machineTypes/{machine_type}")),
            ..HostSources::default()
        }
    }

    fn staged_record(root: &tempfile::TempDir) -> std::path::PathBuf {
        root.path()
            .join(MACHINE_TYPE_RECORD_PATH.trim_start_matches('/'))
    }

    /// A staged root with the state directory the installer creates.
    fn staged_state_root() -> tempfile::TempDir {
        // macOS exposes its temporary directory through /var -> /private/var.
        // Only the trusted test root is resolved; record path components are
        // still opened one at a time without following links.
        let temporary = std::env::temp_dir().canonicalize().expect("temporary root");
        let root = tempfile::tempdir_in(temporary).expect("tempdir");
        std::fs::create_dir_all(
            staged_record(&root)
                .parent()
                .expect("the record has a parent"),
        )
        .expect("stage the state directory");
        root
    }

    #[test]
    fn the_firmware_product_name_is_a_source_and_only_the_exact_gce_name_counts() {
        let root = staged_state_root();
        std::fs::create_dir_all(root.path().join("sys/class/dmi/id")).expect("stage");
        std::fs::write(
            root.path().join("sys/class/dmi/id/product_name"),
            "Google Compute Engine\n",
        )
        .expect("write product name");
        std::fs::write(staged_record(&root), "{}").expect("stage a record");

        let sources = SystemHostProbe::with_root(root.path())
            .sources()
            .expect("staged sources read");
        assert_eq!(
            sources.dmi_product_name.as_deref(),
            Some("Google Compute Engine\n"),
            "the product name is carried as a source, so the gate is fixture-driven"
        );
        assert_eq!(
            (sources.gce_machine_type, sources.machine_type_record),
            (None, None),
            "a staged tree never asks the metadata service, so it never reads the record"
        );

        assert!(is_compute_engine("Google Compute Engine\n"));
        for other in [
            "Precision 7960 Tower\n",
            "Google Compute Engine Beta\n",
            "google compute engine\n",
            "",
        ] {
            assert!(!is_compute_engine(other), "`{other}` is not an instance");
        }
    }

    const GCE: Option<&str> = Some("Google Compute Engine\n");

    #[test]
    fn a_machine_that_is_not_gce_is_never_asked_for_a_machine_type() {
        // The row for the physical workstation declares no machine type, so
        // detection must produce none -- without a network round trip, and
        // without reading a record (staged here as a directory, so reading
        // it would be an error).
        let root = staged_state_root();
        std::fs::create_dir_all(staged_record(&root)).expect("stage a directory");
        let probe = SystemHostProbe::with_root(root.path());
        for dmi in [None, Some("Precision 7960 Tower\n")] {
            assert_eq!(
                probe
                    .machine_type_sources(dmi, || panic!("{dmi:?}: the metadata service was asked"))
                    .expect("no query attempted"),
                (None, None),
                "{dmi:?}"
            );
        }
    }

    #[test]
    fn a_live_answer_is_used_and_the_record_is_never_read() {
        // The record path is a directory, so reading it would be an error:
        // the only way this passes is by not reading it at all.
        let root = staged_state_root();
        std::fs::create_dir_all(staged_record(&root)).expect("stage a directory");
        let answer = "projects/REDACTED/machineTypes/g2-standard-8".to_string();
        assert_eq!(
            SystemHostProbe::with_root(root.path())
                .machine_type_sources(GCE, || Ok(answer.clone()))
                .expect("a live answer needs no record"),
            (Some(answer), None)
        );
    }

    #[test]
    fn an_unreachable_service_reads_the_record() {
        let root = staged_state_root();
        let probe = SystemHostProbe::with_root(root.path());
        assert_eq!(
            probe
                .machine_type_sources(GCE, || Err(MetadataFailure::Timeout))
                .expect("an absent record is not an error here"),
            (None, None),
            "no record is carried as absence; identify refuses it"
        );

        std::fs::write(staged_record(&root), "recorded body").expect("stage a record");
        assert_eq!(
            probe
                .machine_type_sources(GCE, || Err(MetadataFailure::Timeout))
                .expect("a readable record"),
            (None, Some("recorded body".to_string()))
        );
    }

    #[test]
    fn a_service_that_answered_badly_is_never_outvoted_by_the_record() {
        let root = staged_state_root();
        let record = MachineTypeRecord::for_live_sources(&live_gce_sources("g2-standard-8"))
            .expect("the facts are readable")
            .expect("a live answer records")
            .to_json()
            .expect("serializes");
        std::fs::write(staged_record(&root), record).expect("stage a valid record");

        let err = SystemHostProbe::with_root(root.path())
            .machine_type_sources(GCE, || {
                Err(MetadataFailure::Answered(
                    "metadata service answered `HTTP/1.0 403`".to_string(),
                ))
            })
            .expect_err("an answer that is not a machine type is a broken source");
        match err {
            PlatformProbeError::Unreadable { detail, .. } => {
                assert!(
                    detail.contains("403"),
                    "names what the service said: {detail}"
                );
            }
            other => panic!("expected Unreadable, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_live_answer_is_recorded_group_readable_and_detection_accepts_it() {
        use std::os::unix::fs::PermissionsExt;

        let root = staged_state_root();
        let path = staged_record(&root);
        // A world-readable temporary left behind by an interrupted write.
        let leftover = path.with_extension("json.tmp");
        std::fs::write(&leftover, "partial").expect("stage a leftover temporary");
        std::fs::set_permissions(&leftover, std::fs::Permissions::from_mode(0o644))
            .expect("make it world-readable");

        let live = live_gce_sources("g2-standard-8");
        assert_eq!(
            SystemHostProbe::with_root(root.path())
                .write_machine_type_record(&live)
                .expect("writes"),
            RecordWrite::Written
        );

        let mode = std::fs::metadata(&path)
            .expect("written")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(
            mode, 0o640,
            "group-readable, never world-readable: {mode:o}"
        );
        assert!(!leftover.exists(), "the temporary is renamed into place");

        let mut offline = live;
        offline.gce_machine_type = None;
        offline.machine_type_record = Some(std::fs::read_to_string(&path).expect("readable"));
        assert_eq!(
            crate::machine_type_record::establish_machine_type(&offline).expect("accepted"),
            Some((
                "g2-standard-8".to_string(),
                crate::MachineTypeSource::RecordedFromMetadata
            ))
        );
    }

    #[test]
    fn only_a_live_answer_is_ever_recorded() {
        let root = staged_state_root();
        let live = live_gce_sources("g2-standard-8");
        // A record detection would accept: the facts still match.
        let mut resolved_from_a_record = live.clone();
        resolved_from_a_record.gce_machine_type = None;
        resolved_from_a_record.machine_type_record = Some(
            MachineTypeRecord::for_live_sources(&live)
                .expect("the facts are readable")
                .expect("a live answer records")
                .to_json()
                .expect("serializes"),
        );
        assert!(
            crate::machine_type_record::establish_machine_type(&resolved_from_a_record)
                .expect("the record is accepted")
                .is_some()
        );
        assert_eq!(
            SystemHostProbe::with_root(root.path())
                .write_machine_type_record(&resolved_from_a_record)
                .expect("nothing to do is not an error"),
            RecordWrite::NotApplicable
        );
        assert!(!staged_record(&root).exists(), "nothing is written");
    }

    #[cfg(unix)]
    #[test]
    fn an_identical_record_is_not_rewritten_and_a_changed_one_is() {
        use std::os::unix::fs::MetadataExt;

        let root = staged_state_root();
        let probe = SystemHostProbe::with_root(root.path());
        let path = staged_record(&root);
        probe
            .write_machine_type_record(&live_gce_sources("g2-standard-8"))
            .expect("first write");
        let (inode, bytes) = (
            std::fs::metadata(&path).expect("written").ino(),
            std::fs::read(&path).expect("readable"),
        );

        assert_eq!(
            probe
                .write_machine_type_record(&live_gce_sources("g2-standard-8"))
                .expect("second write"),
            RecordWrite::Unchanged
        );
        assert_eq!(
            std::fs::metadata(&path).expect("still there").ino(),
            inode,
            "an unchanged record is not replaced"
        );
        assert_eq!(std::fs::read(&path).expect("readable"), bytes);

        assert_eq!(
            probe
                .write_machine_type_record(&live_gce_sources("g2-standard-24"))
                .expect("changed write"),
            RecordWrite::Written
        );
        assert!(std::fs::read_to_string(&path)
            .expect("readable")
            .contains("g2-standard-24"));
    }

    #[test]
    fn the_writer_never_creates_the_state_directory() {
        let root = tempfile::tempdir().expect("tempdir");
        let err = SystemHostProbe::with_root(root.path())
            .write_machine_type_record(&live_gce_sources("g2-standard-8"))
            .expect_err("a missing state directory is a broken install");
        assert!(
            err.to_string().contains("machine-type.json"),
            "names the record: {err}"
        );
        assert!(
            !staged_record(&root)
                .parent()
                .expect("the record has a parent")
                .exists(),
            "the state directory belongs to the installer"
        );
    }

    #[test]
    fn a_live_answer_on_facts_that_cannot_be_read_says_it_was_not_recorded() {
        // Not "there was no live answer": there was one, and the record is
        // missing for a different reason the operator can act on.
        let root = staged_state_root();
        let live = HostSources {
            pci_devices: None,
            ..live_gce_sources("g2-standard-8")
        };
        match SystemHostProbe::with_root(root.path())
            .write_machine_type_record(&live)
            .expect("not an error")
        {
            RecordWrite::FactsUnavailable(fact) => {
                assert!(fact.contains("NVIDIA display devices"), "{fact}");
            }
            other => panic!("expected FactsUnavailable, got {other:?}"),
        }
        assert!(!staged_record(&root).exists(), "nothing is written");
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_record_is_an_error_not_absence() {
        // An operator outside the tensorplate group, offline. "I could not
        // read it" must not become "nothing was recorded".
        use std::os::unix::fs::PermissionsExt;

        let root = staged_valid_record_root();
        let state = staged_record(&root)
            .parent()
            .expect("the record has a parent")
            .to_path_buf();
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let denied = std::fs::read_dir(&state).is_err();
        let result = SystemHostProbe::with_root(root.path())
            .machine_type_sources(GCE, || Err(MetadataFailure::Timeout));
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o750)).expect("chmod");
        // Running as root defeats the permission bit; skip rather than
        // assert something the environment cannot produce.
        if denied {
            match result {
                Err(PlatformProbeError::Unreadable { source_name, .. }) => {
                    assert!(
                        source_name.ends_with(MACHINE_TYPE_RECORD_PATH),
                        "{source_name}"
                    );
                }
                other => panic!("an unreadable record must not read as absent: {other:?}"),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_record_that_is_not_a_regular_file_is_never_followed() {
        let root = staged_state_root();
        let elsewhere = root.path().join("elsewhere.json");
        std::fs::write(
            &elsewhere,
            MachineTypeRecord::for_live_sources(&live_gce_sources("g2-standard-8"))
                .expect("the facts are readable")
                .expect("records")
                .to_json()
                .expect("serializes"),
        )
        .expect("stage a valid record outside the state directory");
        std::os::unix::fs::symlink(&elsewhere, staged_record(&root)).expect("symlink");
        match SystemHostProbe::with_root(root.path())
            .machine_type_sources(GCE, || Err(MetadataFailure::Timeout))
        {
            Err(PlatformProbeError::IdentityUnestablished { detail, .. }) => {
                assert!(detail.contains("not a regular file"), "{detail}");
            }
            other => panic!("a symlinked record must not be read: {other:?}"),
        }

        std::fs::remove_file(staged_record(&root)).expect("unlink");
        std::fs::create_dir(staged_record(&root)).expect("stage a directory");
        assert!(
            matches!(
                SystemHostProbe::with_root(root.path())
                    .machine_type_sources(GCE, || Err(MetadataFailure::Timeout)),
                Err(PlatformProbeError::IdentityUnestablished { .. })
            ),
            "a directory is not a record"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_record_under_any_symlinked_state_ancestor_is_refused() {
        for ancestor in ["var/lib/tensorplate/state", "var/lib/tensorplate"] {
            let root = staged_state_root();
            let record = staged_record(&root);
            let elsewhere = root.path().join("elsewhere");
            let relative_record = record
                .strip_prefix(root.path().join(ancestor))
                .expect("record beneath the tested ancestor");
            let outside_record = elsewhere.join(relative_record);
            std::fs::create_dir_all(outside_record.parent().expect("parent"))
                .expect("create outside directory");
            std::fs::write(&outside_record, "outside fixture contents").expect("outside fixture");
            std::fs::remove_dir_all(root.path().join(ancestor)).expect("remove original ancestor");
            std::os::unix::fs::symlink(&elsewhere, root.path().join(ancestor)).expect("symlink");

            assert!(
                matches!(
                    read_machine_type_record(&record),
                    Err(PlatformProbeError::IdentityUnestablished { .. })
                ),
                "the reader must refuse symlinked {ancestor}"
            );
            assert!(
                SystemHostProbe::with_root(root.path())
                    .write_machine_type_record(&live_gce_sources("g2-standard-8"))
                    .is_err(),
                "the writer must refuse symlinked {ancestor}"
            );
            assert_eq!(
                std::fs::read_to_string(&outside_record).expect("outside fixture"),
                "outside fixture contents",
                "neither operation touches the symlink destination"
            );
            assert!(!outside_record.with_extension("json.tmp").exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn replacing_a_pinned_parent_does_not_redirect_record_io() {
        let root = staged_state_root();
        let record = staged_record(&root);
        let state = record.parent().expect("state directory");
        std::fs::write(&record, "original record").expect("original record");
        let directory = RecordDirectory::open(&record).expect("pin the original directory");

        let moved = root.path().join("original-state");
        let elsewhere = root.path().join("replacement-state");
        std::fs::create_dir(&elsewhere).expect("replacement directory");
        let outside_record = elsewhere.join("machine-type.json");
        std::fs::write(&outside_record, "outside fixture contents").expect("outside fixture");
        std::fs::rename(state, &moved).expect("move the original directory");
        std::os::unix::fs::symlink(&elsewhere, state).expect("replace the path with a link");

        assert_eq!(
            read_machine_type_record_in(&directory, &record)
                .expect("read from the pinned directory"),
            Some("original record".to_string())
        );
        directory
            .replace(b"updated record")
            .expect("write to the pinned directory");
        assert_eq!(
            std::fs::read_to_string(moved.join("machine-type.json")).expect("original directory"),
            "updated record"
        );
        assert_eq!(
            std::fs::read_to_string(&outside_record).expect("outside fixture"),
            "outside fixture contents"
        );
        assert!(!outside_record.with_extension("json.tmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn replacing_an_open_record_does_not_change_the_read_descriptor() {
        let root = staged_state_root();
        let record = staged_record(&root);
        let elsewhere = root.path().join("outside.json");
        std::fs::write(&record, "original record").expect("original record");
        std::fs::write(&elsewhere, "outside fixture contents").expect("outside fixture");
        let directory = RecordDirectory::open(&record).expect("pin the directory");
        let mut file = directory.read().expect("open the record atomically");

        std::fs::remove_file(&record).expect("remove the original name");
        std::os::unix::fs::symlink(&elsewhere, &record).expect("replace the name with a link");
        assert!(file.metadata().expect("opened descriptor").is_file());
        let mut body = String::new();
        file.read_to_string(&mut body)
            .expect("read the original descriptor");
        assert_eq!(body, "original record");
        assert!(matches!(
            read_machine_type_record_in(&directory, &record),
            Err(PlatformProbeError::IdentityUnestablished { .. })
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_fifo_record_is_refused_without_waiting_for_a_writer() {
        use rustix::fs::{mknodat, FileType, Mode, CWD};

        let root = staged_state_root();
        let record = staged_record(&root);
        mknodat(CWD, &record, FileType::Fifo, Mode::from_raw_mode(0o600), 0).expect("stage a FIFO");
        assert!(matches!(
            read_machine_type_record(&record),
            Err(PlatformProbeError::IdentityUnestablished { .. })
        ));
    }

    #[test]
    fn an_oversized_record_is_refused_without_reading_it_all() {
        let root = staged_state_root();
        std::fs::write(
            staged_record(&root),
            vec![b' '; usize::try_from(MAX_MACHINE_TYPE_RECORD).expect("fits") + 1],
        )
        .expect("stage an oversized record");
        match SystemHostProbe::with_root(root.path())
            .machine_type_sources(GCE, || Err(MetadataFailure::Timeout))
        {
            Err(PlatformProbeError::IdentityUnestablished { detail, .. }) => {
                assert!(detail.contains("larger than"), "{detail}");
            }
            other => panic!("an oversized record must be refused: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_writer_never_writes_through_a_planted_link() {
        use std::os::unix::fs::PermissionsExt;

        let root = staged_state_root();
        let path = staged_record(&root);
        let victim = root.path().join("victim.txt");
        std::fs::write(&victim, "precious").expect("stage a victim");
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        std::os::unix::fs::symlink(&victim, path.with_extension("json.tmp")).expect("symlink");

        let probe = SystemHostProbe::with_root(root.path());
        let live = live_gce_sources("g2-standard-8");
        assert_eq!(
            probe.write_machine_type_record(&live).expect("writes"),
            RecordWrite::Written
        );
        assert_eq!(
            std::fs::read_to_string(&victim).expect("victim"),
            "precious"
        );
        assert_eq!(
            std::fs::metadata(&victim)
                .expect("victim")
                .permissions()
                .mode()
                & 0o7777,
            0o600
        );
        assert!(std::fs::symlink_metadata(&path)
            .expect("written")
            .file_type()
            .is_file());

        // A record that is a link to identical bytes is still replaced: the
        // reader refuses links, so leaving it would refuse every offline start.
        let body = std::fs::read(&path).expect("readable");
        std::fs::write(&victim, body).expect("identical bytes elsewhere");
        std::fs::remove_file(&path).expect("unlink");
        std::os::unix::fs::symlink(&victim, &path).expect("symlink");
        assert_eq!(
            probe.write_machine_type_record(&live).expect("writes"),
            RecordWrite::Written
        );
        assert!(std::fs::symlink_metadata(&path)
            .expect("written")
            .file_type()
            .is_file());
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
    fn an_unreachable_metadata_service_yields_no_machine_type() {
        // Port zero cannot have a listener. Keep this bounded-connect
        // check on loopback rather than contacting a real metadata route.
        let started = std::time::Instant::now();
        let result = query_metadata("127.0.0.1:0", METADATA_PATH, Duration::from_millis(100));
        assert!(
            matches!(result, Err(MetadataFailure::Timeout)),
            "nothing answered: {result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "detection must stay bounded, took {:?}",
            started.elapsed()
        );
    }

    /// Serve one fixed reply on loopback, optionally holding the socket
    /// open afterwards, and return the address.
    ///
    /// The request is read first, so closing the socket afterwards is an
    /// orderly close rather than a reset over unread bytes.
    fn serve_once(reply: impl AsRef<[u8]> + Send + 'static, linger: Duration) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                let _ = socket.set_read_timeout(Some(Duration::from_secs(2)));
                let mut request = Vec::new();
                let mut chunk = [0_u8; 256];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match socket.read(&mut chunk) {
                        Ok(n) if n > 0 => request.extend_from_slice(&chunk[..n]),
                        _ => break,
                    }
                }
                let _ = socket.write_all(reply.as_ref());
                let _ = socket.flush();
                std::thread::sleep(linger);
            }
        });
        addr
    }

    /// A GCE-looking probe whose record is valid for `live_gce_sources`, so
    /// any fallback to it would succeed.
    fn staged_valid_record_root() -> tempfile::TempDir {
        let root = staged_state_root();
        let record = MachineTypeRecord::for_live_sources(&live_gce_sources("g2-standard-8"))
            .expect("the facts are readable")
            .expect("a live answer records")
            .to_json()
            .expect("serializes");
        std::fs::write(staged_record(&root), record).expect("stage a valid record");
        root
    }

    #[test]
    fn a_connect_that_fails_is_the_unreachable_case() {
        // What a unit denied network access sees: the connect itself fails.
        // This is the one failure the record may stand in for, so it must
        // stay classified as unreachable rather than as an answer. Port 0
        // can never be listened on, so unlike a port freed by a dropped
        // listener, no concurrently running test can end up answering it.
        let addr = "127.0.0.1:0";
        let result = query_metadata(addr, METADATA_PATH, Duration::from_millis(500));
        assert!(
            matches!(result, Err(MetadataFailure::Timeout)),
            "a refused connect is unreachable: {result:?}"
        );
        let root = staged_valid_record_root();
        assert!(
            matches!(
                SystemHostProbe::with_root(root.path()).machine_type_sources(GCE, || {
                    query_metadata(addr, METADATA_PATH, Duration::from_millis(500))
                }),
                Ok((None, Some(_)))
            ),
            "and it reads the record"
        );
    }

    #[test]
    fn a_peer_that_never_says_anything_is_the_unreachable_case() {
        let addr = serve_once("", Duration::from_secs(30));
        let started = std::time::Instant::now();
        let result = query_metadata(&addr, METADATA_PATH, Duration::from_millis(200));
        assert!(
            matches!(result, Err(MetadataFailure::Timeout)),
            "nothing arrived within the budget: {result:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_peer_that_sent_anything_but_a_complete_answer_answered() {
        // Each of these is a service that was reached. None of them may be
        // mistaken for an unreachable one, because only that lets the record
        // stand in for the live answer.
        let oversized_head = format!(
            "HTTP/1.0 200 OK\r\nX-Padding: {}\r\n\r\nprojects/1/machineTypes/g2-standard-8",
            "a".repeat(9000)
        );
        for (label, reply, linger) in [
            ("non-HTTP bytes then close", "garbage\n".to_string(), 0),
            (
                "a status line whose head never ends",
                "HTTP/1.0 403 Forbidden\r\nContent-Type: text/plain\r\n".to_string(),
                0,
            ),
            ("an accept then close", String::new(), 0),
            ("a head past the size cap", oversized_head, 0),
            (
                "a body shorter than its length, then close",
                "HTTP/1.0 200 OK\r\nContent-Length: 999\r\n\r\nprojects/1/machineTypes/g2"
                    .to_string(),
                0,
            ),
            (
                "a body shorter than its length, held open",
                "HTTP/1.0 200 OK\r\nContent-Length: 999\r\n\r\nprojects/1/machineTypes/g2"
                    .to_string(),
                30_000,
            ),
            (
                "a plausible unframed body held open",
                "HTTP/1.0 200 OK\r\n\r\nprojects/1/machineTypes/g2-standard-8".to_string(),
                30_000,
            ),
            (
                "an invalid length held open",
                "HTTP/1.0 200 OK\r\nContent-Length: nope\r\n\r\nprojects/1/machineTypes/g2-standard-8"
                    .to_string(),
                30_000,
            ),
            (
                "an invalid length followed by an orderly close",
                "HTTP/1.0 200 OK\r\nContent-Length: nope\r\n\r\nprojects/1/machineTypes/g2-standard-8"
                    .to_string(),
                0,
            ),
            (
                "non-HTTP status with a successful code and complete body",
                "garbage 200\r\nContent-Length: 37\r\n\r\nprojects/1/machineTypes/g2-standard-8"
                    .to_string(),
                0,
            ),
            (
                "an unsupported HTTP version",
                "HTTP/2 200 OK\r\nContent-Length: 37\r\n\r\nprojects/1/machineTypes/g2-standard-8"
                    .to_string(),
                0,
            ),
            (
                "another protocol's banner",
                "SSH-2.0-OpenSSH_9.6\r\n".to_string(),
                0,
            ),
            (
                "a 200 that is not the status",
                "HTTP/1.0 404 200\r\nContent-Length: 37\r\n\r\nprojects/1/machineTypes/g2-standard-8"
                    .to_string(),
                0,
            ),
        ] {
            let addr = serve_once(reply.clone(), Duration::from_millis(linger));
            let result = query_metadata(&addr, METADATA_PATH, Duration::from_millis(300));
            assert!(
                matches!(result, Err(MetadataFailure::Answered(_))),
                "{label}: {result:?}"
            );

            let addr = serve_once(reply, Duration::from_millis(linger));
            let root = staged_valid_record_root();
            match SystemHostProbe::with_root(root.path()).machine_type_sources(GCE, || {
                query_metadata(&addr, METADATA_PATH, Duration::from_millis(300))
            }) {
                Err(PlatformProbeError::Unreadable { source_name, .. }) => {
                    assert_eq!(source_name, "GCE metadata service", "{label}");
                }
                other => panic!("{label}: the record must not outvote an answer: {other:?}"),
            }
        }
    }

    #[test]
    fn ambiguous_or_unsupported_metadata_framing_is_never_an_answer() {
        let body = "projects/1/machineTypes/g2-standard-8";
        for headers in [
            "Content-Length: 37\r\nContent-Length: 37",
            "Content-Length: 37\r\nContent-Length: 99",
            "Content-Length: 37, 37",
            "Content-Length: +37",
            "Content-Length: -1",
            "Content-Length:",
            "Content-Length: 99999999999999999999999999999999999999999",
            "Content-Length: 8193",
            "Content-Length: 36",
            "Content-Length : 37",
            " Content-Length: 37",
            "Content-Length: 37\r\n folded header",
            "Transfer-Encoding: chunked",
            "Transfer-Encoding: identity",
            "Content-Length: 37\r\nTransfer-Encoding: chunked",
        ] {
            let reply = format!("HTTP/1.0 200 OK\r\n{headers}\r\n\r\n{body}");
            let addr = serve_once(reply, Duration::ZERO);
            let root = staged_valid_record_root();
            let result = SystemHostProbe::with_root(root.path()).machine_type_sources(GCE, || {
                query_metadata(&addr, METADATA_PATH, Duration::from_millis(300))
            });
            assert!(
                matches!(result, Err(PlatformProbeError::Unreadable { .. })),
                "invalid framing must neither produce a live identity nor read the valid record: \
                 {headers:?}: {result:?}"
            );
        }
    }

    #[test]
    fn an_orderly_close_completes_a_body_without_a_declared_length() {
        for version in ["HTTP/1.0", "HTTP/1.1"] {
            let addr = serve_once(
                format!("{version} 200 OK\r\nMetadata-Flavor: Google\r\n\r\nprojects/1/machineTypes/g2-standard-8"),
                Duration::ZERO,
            );
            let root = staged_valid_record_root();
            let (live, recorded) = SystemHostProbe::with_root(root.path())
                .machine_type_sources(GCE, || {
                    query_metadata(&addr, METADATA_PATH, Duration::from_millis(300))
                })
                .expect("the orderly close completes the response");
            assert_eq!(
                live.as_deref(),
                Some("projects/1/machineTypes/g2-standard-8")
            );
            assert!(recorded.is_none(), "the live answer takes precedence");
        }
    }

    #[test]
    fn the_response_size_cap_does_not_masquerade_as_an_orderly_close() {
        let reply = format!("HTTP/1.0 200 OK\r\n\r\n{}", "x".repeat(8192));
        let addr = serve_once(reply, Duration::ZERO);
        let result = query_metadata(&addr, METADATA_PATH, Duration::from_millis(300));
        assert!(
            matches!(result, Err(MetadataFailure::Answered(_))),
            "the response exceeds the byte cap: {result:?}"
        );
    }

    #[test]
    fn invalid_utf8_is_not_replaced_to_make_a_metadata_answer() {
        let addr = serve_once(
            b"HTTP/1.0 200 OK\r\nContent-Length: 1\r\n\r\n\xff",
            Duration::ZERO,
        );
        let result = query_metadata(&addr, METADATA_PATH, Duration::from_millis(300));
        assert!(
            matches!(result, Err(MetadataFailure::Answered(_))),
            "invalid response bytes are rejected: {result:?}"
        );
    }

    #[test]
    fn what_the_service_said_reaches_the_journal_as_one_line() {
        let addr = serve_once(
            "HTTP/1.0 403 Forbidden\rplatform admission: row=x evidence=validated\r\n\r\n",
            Duration::from_millis(10),
        );
        let err = query_metadata(&addr, METADATA_PATH, Duration::from_millis(500))
            .expect_err("403 is not a machine type");
        let text = err.to_string();
        assert!(text.contains("403"), "{text}");
        assert!(
            !text.contains('\r') && !text.contains('\n'),
            "control characters are escaped: {text:?}"
        );
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
        let result = query_metadata(&addr, METADATA_PATH, Duration::from_millis(300));
        assert!(
            matches!(result, Err(MetadataFailure::Answered(_))),
            "a body shorter than its declared length is rejected, and the service was reached: \
             {result:?}"
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
        let result = query_metadata(&addr, METADATA_PATH, Duration::from_millis(200));
        assert!(
            matches!(result, Err(MetadataFailure::Answered(_))),
            "a peer that sent bytes was reached, however slowly: {result:?}"
        );
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
            matches!(err, MetadataFailure::Answered(_)) && err.to_string().contains("403"),
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
            other @ (PlatformProbeError::Unrecognized { .. }
            | PlatformProbeError::IdentityUnestablished { .. }) => {
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
                other @ (PlatformProbeError::Unrecognized { .. }
                | PlatformProbeError::IdentityUnestablished { .. }) => {
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
