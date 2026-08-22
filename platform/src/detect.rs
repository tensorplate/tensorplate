// SPDX-License-Identifier: Apache-2.0
//
// Host identity detection: turning what a machine reports into the exact
// values a support row is written in.
//
// Detection is split in two on purpose. Everything in this module is a
// pure function of recorded source content — the literal bytes of
// `/etc/os-release`, the literal stdout of `sw_vers` — so every row's
// host identity is unit-testable from a fixture with no hardware in the
// room. Reading those sources off a live machine is the only part that
// touches the world, and it lives in [`crate::probe`].
//
// # Normalization
//
// A row records an OS at the granularity the project committed to
// validating, which is coarser than what a machine reports:
//
// | row field | machine reports | row records |
// |---|---|---|
// | `architecture` | `aarch64` | `arm64` |
// | `os_version` (macOS) | `26.5.2` | `26` |
// | `image_identity` (Jetson) | L4T `r36.4.3` | `L4T r36.x (…)` |
//
// Host-field matching is exact string equality, so detection must produce the
// row's spelling or the row can never match — including on the very hardware
// it describes. Accelerator identity is also preserved verbatim so exact rows
// and narrow family policies can evaluate the same observation. Normalizing
// host facts here, once, is what keeps that true. The unnormalized strings are
// not thrown away: they are carried in [`ExactHostFacts`] for evidence
// recording, which needs the precision matching deliberately discards.

use tensorplate_protocol::PlatformMemoryProfileName;

use crate::capability::AcceleratorObservation;
use crate::error::PlatformProbeError;
use crate::identity::{
    AcceleratorIdentity, DetectedArchitecture, DetectedPlatform, DetectedVendor, HostIdentity,
};
use crate::row::{CpuArchitecture, CpuVendor};

/// The recorded content of every source host identity is derived from.
///
/// A field is `None` when its source does not exist on the machine (no
/// `/etc/os-release` on macOS, no `/etc/nv_tegra_release` off Jetson).
/// Absence is a signal, not a failure: it is how the platform is told
/// apart.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HostSources {
    /// `uname -m`.
    pub uname_machine: Option<String>,
    /// `/etc/os-release`.
    pub os_release: Option<String>,
    /// `/proc/cpuinfo`.
    pub cpuinfo: Option<String>,
    /// `/etc/nv_tegra_release`. Present only on Jetson.
    pub nv_tegra_release: Option<String>,
    /// `dpkg-query -W -f='${Version}' nvidia-jetpack`, e.g. `6.2-b77`.
    pub nvidia_jetpack_version: Option<String>,
    /// `/proc/device-tree/model`, NUL-terminated on Linux.
    pub device_tree_model: Option<String>,
    /// `sw_vers -productName`.
    pub sw_vers_product_name: Option<String>,
    /// `sw_vers -productVersion`.
    pub sw_vers_product_version: Option<String>,
    /// `sw_vers -buildVersion`.
    pub sw_vers_build_version: Option<String>,
    /// `sysctl -n machdep.cpu.brand_string`.
    pub cpu_brand: Option<String>,
    /// `sysctl -n hw.memsize`, in bytes.
    pub hw_memsize: Option<String>,
    /// Body of the GCE metadata machine-type response, e.g.
    /// `projects/1234/machineTypes/g2-standard-8`.
    pub gce_machine_type: Option<String>,
    /// `/proc/meminfo`. Read for its `MemTotal` line, which is how a
    /// Jetson's module capacity is told from its sibling's.
    pub proc_meminfo: Option<String>,
    /// The PCI bus, one line per function: `<address> <vendor> <device>
    /// <class>`, assembled from `/sys/bus/pci/devices/*/{vendor,device,
    /// class}`.
    ///
    /// `None` on a machine with no PCI bus at all — a Mac, a Jetson —
    /// which is a signal rather than a failure, the same as every other
    /// source here.
    ///
    /// This is the only way to learn that an accelerator is PHYSICALLY
    /// PRESENT when its driver cannot say so. `nvidia-smi` needs a working
    /// driver to answer, so a card whose driver is missing or broken is
    /// indistinguishable from no card at all — and a GPU host in that state
    /// currently resolves to the CPU-only row and deploys as though it had
    /// no accelerator.
    pub pci_devices: Option<String>,
}

/// What the machine reported before normalization, kept for evidence
/// recording.
///
/// Matching never reads these. They exist because a row's evidence has to
/// name the exact image it was recorded on, and `os_version = "26"` is not
/// enough to reproduce a run.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExactHostFacts {
    /// Full OS version as reported, e.g. `26.5.2`.
    pub os_version: Option<String>,
    /// OS build string where the platform has one, e.g. macOS `25F84`.
    pub os_build: Option<String>,
    /// Exact L4T release including its patch, e.g. `r36.4.3`.
    pub l4t_release: Option<String>,
    /// Machine architecture exactly as reported, e.g. `aarch64`.
    pub reported_machine: Option<String>,
    /// Device model string where the platform has one.
    pub device_model: Option<String>,
    /// PCI addresses of NVIDIA display or 3D controllers physically
    /// present, whether or not a driver can talk to them.
    ///
    /// Empty on a machine with a PCI bus and no such device; also empty
    /// where the bus could not be enumerated, which is why the raw source
    /// is kept in [`HostSources::pci_devices`] rather than only this.
    pub nvidia_pci_functions: Vec<String>,
}

/// A detected host: the row-comparable identity plus the exact facts that
/// identity was normalized from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostReport {
    pub identity: HostIdentity,
    pub exact: ExactHostFacts,
}

/// A complete platform observation from one source-gathering pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlatformReport {
    pub host: HostReport,
    pub accelerator: Option<AcceleratorObservation>,
}

impl PlatformReport {
    /// Project the observation onto the exact identity consumed by the
    /// registry. Memory remains alongside it for capability resolution.
    #[must_use]
    pub fn detected_platform(&self) -> DetectedPlatform {
        match &self.accelerator {
            Some(observed) => DetectedPlatform::with_accelerator(
                self.host.identity.clone(),
                observed.identity.clone(),
            ),
            None => DetectedPlatform::host_only(self.host.identity.clone()),
        }
    }
}

/// Normalize a machine architecture string to the row vocabulary.
///
/// Linux reports `aarch64` where a row records `arm64`; both are the same
/// architecture and a row spelled either way must match the same machine.
/// Anything else is returned verbatim as
/// [`DetectedArchitecture::Other`] — an architecture no row names is an
/// unsupported machine, not an undetectable one.
#[must_use]
pub fn normalize_architecture(reported: &str) -> DetectedArchitecture {
    match reported.trim() {
        "x86_64" | "amd64" => DetectedArchitecture::Known(CpuArchitecture::X86_64),
        "aarch64" | "arm64" => DetectedArchitecture::Known(CpuArchitecture::Arm64),
        other => DetectedArchitecture::Other(other.to_string()),
    }
}

/// Read one `KEY=value` field out of `/etc/os-release` content.
///
/// Values may be quoted (`VERSION_ID="24.04"`); the quotes are shell
/// syntax, not part of the value, and are stripped.
#[must_use]
pub fn os_release_field(content: &str, key: &str) -> Option<String> {
    content.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        if name.trim() != key {
            return None;
        }
        let value = value.trim();
        // Both quote styles are valid shell syntax here, and a quote is
        // never part of the value. Stripped only as a matched pair: a lone
        // leading quote means a truncated file, and silently accepting
        // `"Ubuntu` would compare a mangled name against the row's.
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    })
}

/// The CPU vendor from `/proc/cpuinfo`.
///
/// `vendor_id` is an **x86-only** field; the kernel never emits it on
/// aarch64, which reports `CPU implementer` instead. Readable `cpuinfo`
/// therefore always yields a vendor — a known one where a row names it,
/// otherwise [`DetectedVendor::Other`] carrying whatever the machine did
/// say. Failing instead would report an ordinary arm64 Linux host as
/// *undetectable* rather than *unsupported*, and would make
/// [`crate::PlatformReason::UnsupportedCpuVendor`] unreachable on every
/// architecture except x86.
#[must_use]
pub fn cpuinfo_vendor(content: &str) -> DetectedVendor {
    let field = |name: &str| {
        content.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == name).then(|| value.trim().to_string())
        })
    };
    // The first core's vendor: every core on a host reports the same one,
    // and a row names the host's vendor, not a core's.
    if let Some(raw) = field("vendor_id") {
        return match raw.as_str() {
            "GenuineIntel" => DetectedVendor::Known(CpuVendor::Intel),
            "AuthenticAMD" => DetectedVendor::Known(CpuVendor::Amd),
            _ => DetectedVendor::Other(raw),
        };
    }
    // arm64. No committed row names a bare ARM implementer — Jetson is
    // identified as `nvidia_soc` through its own branch, and Apple silicon
    // never runs this path — so this is reported verbatim and left for the
    // registry to call unsupported.
    if let Some(implementer) = field("CPU implementer") {
        return DetectedVendor::Other(format!("CPU implementer {implementer}"));
    }
    DetectedVendor::Other("unknown".to_string())
}

/// An L4T release parsed out of `/etc/nv_tegra_release`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct L4tRelease {
    pub major: u32,
    pub revision_major: u32,
    pub revision_minor: u32,
}

impl L4tRelease {
    /// The exact release, e.g. `r36.4.3`. Used for evidence, never for
    /// matching.
    #[must_use]
    pub fn exact(&self) -> String {
        format!(
            "r{}.{}.{}",
            self.major, self.revision_major, self.revision_minor
        )
    }

    /// The L4T line at the granularity a row records, e.g. `r36.x`.
    ///
    /// The BSP generation, not the revision: r36.4 and r36.5 are both
    /// JetPack 6 on L4T r36, and an operator who installs an Orin Nano gets
    /// whichever NVIDIA is shipping that week. Pinning the revision meant a
    /// row matched one of them and refused the other on identical hardware
    /// -- and since matching is exact string equality, "refused" is what a
    /// near miss means. The revision is not discarded: `exact()` keeps it,
    /// and `ExactHostFacts` carries it for evidence, which needs the
    /// precision matching deliberately drops.
    #[must_use]
    pub fn row_granularity(&self) -> String {
        format!("r{}.x", self.major)
    }
}

/// Parse `/etc/nv_tegra_release`, whose first line reads
/// `# R36 (release), REVISION: 4.3, GCID: …`.
#[must_use]
pub fn parse_nv_tegra_release(content: &str) -> Option<L4tRelease> {
    let first = content.lines().next()?;
    let major = first
        .split_whitespace()
        .find_map(|token| token.strip_prefix('R')?.parse::<u32>().ok())?;
    let after = first.split("REVISION:").nth(1)?;
    let revision = after.split(',').next()?.trim();
    let (rev_major, rev_minor) = revision.split_once('.')?;
    Some(L4tRelease {
        major,
        revision_major: rev_major.trim().parse().ok()?,
        revision_minor: rev_minor.trim().parse().ok()?,
    })
}

/// The JetPack release at the granularity a row records.
///
/// `6.2.3` and `6.2` are the same feature release, and a row that names
/// `6.2` should match a machine running either -- the same reduction
/// [`macos_row_version`] makes for `26.5.2` -> `26`. Without it, a patch
/// release is a different platform as far as matching is concerned, which
/// is not what a support claim means.
#[must_use]
pub fn jetpack_row_version(version: &str) -> Option<String> {
    let mut parts = version.trim().split('.');
    let major = parts.next()?.trim();
    if major.is_empty() {
        return None;
    }
    Some(
        match parts.next().map(str::trim).filter(|m| !m.is_empty()) {
            Some(minor) => format!("{major}.{minor}"),
            None => major.to_string(),
        },
    )
}

/// The JetPack version a row records, from the `nvidia-jetpack` package
/// version.
///
/// dpkg reports `6.2-b77`; the Debian revision after `-` is a build
/// number, not part of the JetPack version a row names.
#[must_use]
pub fn jetpack_version(package_version: &str) -> Option<String> {
    let trimmed = package_version.trim();
    // The build suffix is separated by `-` on some releases and `+` on
    // others: JetPack 6.2 packages as `6.2-b77`, and NVIDIA's r36.5 channel
    // publishes `6.2.3+b81`. Splitting on only one of them leaves the other
    // carrying its suffix into the row comparison, where it matches nothing.
    let version = trimmed.split(['-', '+']).next()?.trim();
    (!version.is_empty()).then(|| version.to_string())
}

/// The JetPack feature release an L4T line belongs to.
///
/// The `nvidia-jetpack` metapackage states the version directly, but it is
/// not present on every JetPack device: a rootfs flashed from the base BSP,
/// a Yocto/meta-tegra image, or an `l4t-base` container all carry the L4T
/// release without it. Such a device is still the machine its row
/// describes, so the L4T line it does report is mapped here rather than
/// leaving it to resolve as an unsupported OS version.
///
/// Keyed on the L4T minor line and answering at feature-release
/// granularity, which is what a row records: the patch a given revision
/// shipped as is not derivable from the L4T numbers. An L4T line this
/// release has not been told about yields `None` — an honest "unknown",
/// never a guess, because a wrong JetPack version would make a machine
/// match a row it was never validated against.
#[must_use]
pub fn jetpack_for_l4t(release: L4tRelease) -> Option<&'static str> {
    match (release.major, release.revision_major) {
        // The JetPack 6.2 feature release spans both L4T revisions: 36.4.x
        // and 36.5.x are 6.2.x. Which patch a given revision shipped as is
        // deliberately not claimed here -- NVIDIA's archive pairs 36.5.0
        // with 6.2.2 and 36.5.2 with 6.2.3, so naming one of them would be
        // wrong on the other, and a row records the feature release anyway.
        (36, 4 | 5) => Some("6.2"),
        _ => None,
    }
}

/// The macOS version at row granularity.
///
/// `sw_vers` reports `26.5.2`; a row records `26`. macOS major versions
/// are the release the project validates against, and a point update must
/// not stop a validated machine from matching its own row.
#[must_use]
pub fn macos_row_version(product_version: &str) -> Option<String> {
    let major = product_version.trim().split('.').next()?.trim();
    (!major.is_empty()).then(|| major.to_string())
}

/// The machine type from a GCE metadata response.
///
/// The metadata server answers with the fully qualified resource name
/// `projects/<number>/machineTypes/g2-standard-8`; a row records the bare
/// machine type.
#[must_use]
pub fn machine_type_from_metadata(body: &str) -> Option<String> {
    let value = body.trim().rsplit('/').next()?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Strip the NUL terminator and squeeze whitespace in a device-tree
/// string.
#[must_use]
pub fn device_tree_string(raw: &str) -> Option<String> {
    let cleaned = raw.replace('\0', "");
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    (!collapsed.is_empty()).then_some(collapsed)
}

/// Derive a host identity, and the exact facts behind it, from recorded
/// sources.
///
/// # Errors
///
/// Returns [`PlatformProbeError::Unrecognized`] only when the sources do
/// not describe any platform this can interpret — no architecture, or no
/// OS identity at all. A *recognized* platform reporting a value no row
/// names is not an error: it comes back as
/// [`DetectedArchitecture::Other`] / [`DetectedVendor::Other`], or as an
/// OS name and version reported verbatim, so the registry can call it
/// unsupported rather than undetectable.
pub fn identify(sources: &HostSources) -> Result<HostReport, PlatformProbeError> {
    let reported_machine = sources.uname_machine.as_ref().map(|m| m.trim().to_string());
    let architecture = reported_machine
        .as_deref()
        .map(normalize_architecture)
        .ok_or_else(|| PlatformProbeError::Unrecognized {
            source_name: "uname -m".to_string(),
            detail: "no machine architecture reported".to_string(),
        })?;

    let mut exact = ExactHostFacts {
        reported_machine,
        ..ExactHostFacts::default()
    };

    let (os_name, os_version, image_identity, vendor) =
        if let Some(tegra) = sources.nv_tegra_release.as_deref() {
            jetson_os(sources, tegra, &mut exact)?
        } else if let Some(product_name) = sources.sw_vers_product_name.as_deref() {
            macos_os(sources, product_name, &mut exact)?
        } else if let Some(os_release) = sources.os_release.as_deref() {
            linux_os(sources, os_release, &mut exact)?
        } else {
            return Err(PlatformProbeError::Unrecognized {
                source_name: "host".to_string(),
                detail: "no OS identity source is present".to_string(),
            });
        };

    exact.device_model = sources
        .device_tree_model
        .as_deref()
        .and_then(device_tree_string);

    // Recorded, never matched on. It exists so a later change can tell an
    // accelerator that is absent from one whose driver cannot answer;
    // nothing consults it yet, and adding it here rather than to
    // `HostIdentity` is what keeps matching unchanged.
    exact.nvidia_pci_functions = sources
        .pci_devices
        .as_deref()
        .map(nvidia_pci_functions)
        .unwrap_or_default();

    let machine_type = sources
        .gce_machine_type
        .as_deref()
        .and_then(machine_type_from_metadata);

    Ok(HostReport {
        identity: HostIdentity {
            architecture,
            vendor,
            os_name,
            os_version,
            image_identity,
            machine_type,
        },
        exact,
    })
}

/// Detect host identity plus any accelerator facts available from the same
/// sources.
///
/// Apple silicon reports its integrated accelerator identity through the CPU
/// brand string and its shared memory pool through `hw.memsize`. Both sources
/// are required once the host identifies as Apple: missing or malformed
/// values are broken detection inputs, not an accelerator-less Mac.
pub fn identify_platform(sources: &HostSources) -> Result<PlatformReport, PlatformProbeError> {
    let host = identify(sources)?;
    let accelerator = if host.identity.vendor.known() == Some(CpuVendor::Apple) {
        let sku = sources
            .cpu_brand
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| PlatformProbeError::Unrecognized {
                source_name: "machdep.cpu.brand_string".to_string(),
                detail: "Apple silicon reported no chip identity".to_string(),
            })?;
        let raw_memory = sources
            .hw_memsize
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| PlatformProbeError::Unrecognized {
                source_name: "hw.memsize".to_string(),
                detail: "Apple silicon reported no unified-memory size".to_string(),
            })?;
        let memory_bytes =
            raw_memory
                .parse::<u64>()
                .map_err(|_| PlatformProbeError::Unrecognized {
                    source_name: "hw.memsize".to_string(),
                    detail: format!("`{raw_memory}` is not a byte count"),
                })?;
        if memory_bytes == 0 {
            return Err(PlatformProbeError::Unrecognized {
                source_name: "hw.memsize".to_string(),
                detail: "unified-memory size must be greater than zero".to_string(),
            });
        }
        Some(AcceleratorObservation {
            identity: AcceleratorIdentity {
                sku: sku.to_string(),
                partitioned: false,
            },
            memory_bytes: Some(memory_bytes),
            memory_profile: PlatformMemoryProfileName::UnifiedMemory,
        })
    } else {
        identify_jetson_accelerator(sources).map(|identity| {
            // The other integrated part. Apple prints its chip name verbatim in
            // the CPU brand string; a Jetson prints nothing that is its row SKU,
            // so its identity is composed from the board model and capacity.
            // Both land here so callers have one entry point rather than one
            // per vendor.
            //
            // Unlike the Apple arm this cannot fail. A Jetson whose sources are
            // absent still yields an identity — an unmatchable one — because
            // erroring would reach the agent as "hardware unreadable, admission
            // disabled" and take an unknown board from refused to ungated.
            // When that happens the SKU matches no row, so the capacity below
            // is never consumed by a resolved capability.
            AcceleratorObservation {
                identity,
                memory_bytes: sources
                    .proc_meminfo
                    .as_deref()
                    .and_then(mem_total_from_meminfo),
                memory_profile: PlatformMemoryProfileName::UnifiedMemory,
            }
        })
    };

    Ok(PlatformReport { host, accelerator })
}

type OsIdentity = (String, String, Option<String>, DetectedVendor);

/// Jetson: the OS a row names is JetPack, not the Ubuntu underneath it.
fn jetson_os(
    sources: &HostSources,
    tegra: &str,
    exact: &mut ExactHostFacts,
) -> Result<OsIdentity, PlatformProbeError> {
    let release = parse_nv_tegra_release(tegra);
    if let Some(release) = release {
        exact.l4t_release = Some(release.exact());
    }
    // Every committed Jetson row carries an image identity, so once this is
    // a Jetson at all, both halves of that identity are required. Letting
    // an unparsable L4T release or a damaged `/etc/os-release` fall through
    // as `None` would produce an identity that matches no Jetson row and
    // reaches the operator as "unsupported platform" — when the machine is
    // a Jetson whose sources are broken, which is a different problem with
    // a different fix.
    let release = release.ok_or_else(|| PlatformProbeError::Unrecognized {
        source_name: "/etc/nv_tegra_release".to_string(),
        detail: format!(
            "present but no `R<major> … REVISION: <major>.<minor>` could be read from `{}`",
            tegra.lines().next().unwrap_or("").trim()
        ),
    })?;
    // The Ubuntu release under JetPack is part of the image identity, so a
    // row pins the base it was validated on.
    let ubuntu_base = sources
        .os_release
        .as_deref()
        .and_then(|content| os_release_field(content, "VERSION_ID"))
        .ok_or_else(|| PlatformProbeError::Unrecognized {
            source_name: "/etc/os-release".to_string(),
            detail: "Jetson reported an L4T release but no Ubuntu VERSION_ID to base it on"
                .to_string(),
        })?;
    let image_identity = Some(format!(
        "L4T {} (Ubuntu {} base)",
        release.row_granularity(),
        ubuntu_base
    ));

    // JetPack's own version comes from its package, which is the only
    // source that states it directly. Without that package the L4T release
    // is reported verbatim rather than guessed: an honest non-matching
    // value makes the machine unsupported, while a guess would make it
    // wrongly supported.
    // The package states the JetPack version directly; without it the L4T
    // line is mapped to the release it belongs to. Only when neither is
    // available does the L4T string stand in, which will not match any row
    // — correct, because at that point the JetPack version is genuinely
    // unknown and guessing would match a row this device was never
    // validated against.
    exact.os_version = sources
        .nvidia_jetpack_version
        .as_ref()
        .map(|raw| raw.trim().to_string());
    let version = sources
        .nvidia_jetpack_version
        .as_deref()
        .and_then(jetpack_version)
        .or_else(|| jetpack_for_l4t(release).map(str::to_string))
        .and_then(|version| jetpack_row_version(&version))
        .unwrap_or_else(|| release.exact());

    Ok((
        "JetPack".to_string(),
        version,
        image_identity,
        DetectedVendor::Known(CpuVendor::NvidiaSoc),
    ))
}

/// macOS: `sw_vers` names the OS and the version; the chip names the
/// vendor.
fn macos_os(
    sources: &HostSources,
    product_name: &str,
    exact: &mut ExactHostFacts,
) -> Result<OsIdentity, PlatformProbeError> {
    let product_version = sources.sw_vers_product_version.as_deref().ok_or_else(|| {
        PlatformProbeError::Unrecognized {
            source_name: "sw_vers".to_string(),
            detail: "macOS reported a product name but no product version".to_string(),
        }
    })?;
    exact.os_version = Some(product_version.trim().to_string());
    exact.os_build = sources
        .sw_vers_build_version
        .as_ref()
        .map(|b| b.trim().to_string());

    let version =
        macos_row_version(product_version).ok_or_else(|| PlatformProbeError::Unrecognized {
            source_name: "sw_vers -productVersion".to_string(),
            detail: format!("macOS product version `{product_version}` has no major component"),
        })?;

    // Every Mac this runs on is Apple silicon or Intel; the brand string is
    // what says which.
    let vendor = match sources.cpu_brand.as_deref().map(str::trim) {
        Some(brand) if brand.starts_with("Apple") => DetectedVendor::Known(CpuVendor::Apple),
        Some(brand) if brand.contains("Intel") => DetectedVendor::Known(CpuVendor::Intel),
        Some(brand) => DetectedVendor::Other(brand.to_string()),
        None => {
            return Err(PlatformProbeError::Unrecognized {
                source_name: "machdep.cpu.brand_string".to_string(),
                detail: "macOS reported no CPU brand string".to_string(),
            })
        }
    };

    Ok((product_name.trim().to_string(), version, None, vendor))
}

/// Linux that is not Jetson: the distribution names itself.
fn linux_os(
    sources: &HostSources,
    os_release: &str,
    exact: &mut ExactHostFacts,
) -> Result<OsIdentity, PlatformProbeError> {
    // `NAME`, not `ID`: a row records `Ubuntu`, and `ID` is the lowercase
    // machine token `ubuntu`.
    let name =
        os_release_field(os_release, "NAME").ok_or_else(|| PlatformProbeError::Unrecognized {
            source_name: "/etc/os-release".to_string(),
            detail: "/etc/os-release declares no NAME".to_string(),
        })?;
    let version = os_release_field(os_release, "VERSION_ID").ok_or_else(|| {
        PlatformProbeError::Unrecognized {
            source_name: "/etc/os-release".to_string(),
            detail: "/etc/os-release declares no VERSION_ID".to_string(),
        }
    })?;
    exact.os_version = os_release_field(os_release, "VERSION").or_else(|| Some(version.clone()));

    // An absent `/proc/cpuinfo` is a Linux host that could not describe its
    // CPU at all, which is a broken source rather than an unnamed vendor.
    // A *readable* one always yields a vendor, even if no row names it.
    let vendor = sources
        .cpuinfo
        .as_deref()
        .map(cpuinfo_vendor)
        .ok_or_else(|| PlatformProbeError::Unreadable {
            source_name: "/proc/cpuinfo".to_string(),
            detail: "a Linux host reported no /proc/cpuinfo".to_string(),
        })?;

    Ok((name, version, None, vendor))
}

/// The Jetson module's accelerator identity, derived from what the board
/// reports about itself.
///
/// A Jetson's accelerator is part of the SoC: there is no second device to
/// enumerate and `nvidia-smi` is not shipped, so nothing produces a SKU the
/// way it does for a discrete card. Two sources together do identify the
/// module, and both are already read for other reasons —
/// `/proc/device-tree/model` names the board, and `MemTotal` separates two
/// modules of one board family that report the same model string.
///
/// Unlike the Apple path, which carries `machdep.cpu.brand_string` through
/// verbatim, this composes the SKU. That is forced: no Jetson source prints
/// `Jetson Orin Nano 8GB Super`. The composition is safe in the direction
/// that matters — a SKU this derives wrongly matches no row and the machine
/// is refused, because the result is still compared verbatim by
/// `accelerator_matches`. It can fail to identify a board; it cannot hand
/// one board another board's row.
///
/// Returns `None` only for a host that is not a Jetson. **A Jetson always
/// gets an identity, and this never fails.**
///
/// That is the whole safety argument, and it is deliberately not a
/// judgement call about which inputs are "bad enough" to error on. The
/// caller reads a probe error as "hardware unreadable, admission disabled",
/// so ANY error here takes a Jetson from *refused* to *not gated at all* —
/// on exactly the hardware the gate exists for. Refusing to name a board
/// must never be able to skip the gate, so there is no path that can.
///
/// A board this cannot name yields an identity describing what it saw. No
/// row is written in dev-kit names, raw byte counts, or parentheses, so
/// such an identity is unmatchable by construction and the machine is
/// refused with `unsupported_accelerator_sku` — the same answer an
/// off-matrix discrete card gets.
///
/// Note this cannot mask a genuinely unreadable source: [`SystemHostProbe`]
/// maps a file it cannot read to [`PlatformProbeError::Unreadable`] and
/// propagates it before these sources are assembled. A `None` reaching here
/// means the source was *absent*, which [`HostSources`] documents as a
/// signal rather than a failure.
///
/// [`SystemHostProbe`]: crate::probe::SystemHostProbe
#[must_use]
pub fn identify_jetson_accelerator(sources: &HostSources) -> Option<AcceleratorIdentity> {
    // `/etc/nv_tegra_release` is the same signal `jetson_os` keys on: it is
    // present only on Jetson.
    sources.nv_tegra_release.as_ref()?;

    let model = sources
        .device_tree_model
        .as_deref()
        .and_then(device_tree_string);
    let reported = sources
        .proc_meminfo
        .as_deref()
        .and_then(mem_total_from_meminfo);

    let sku = if let (Some(model), Some(reported)) = (model.as_deref(), reported) {
        recognized_jetson_sku(model, reported)
            .unwrap_or_else(|| format!("{model} ({reported} bytes)"))
    } else {
        // One or both sources absent. Still an accelerator, still not one
        // this can name, so still unmatchable — never an error.
        format!(
            "Jetson (unidentified: model={}, memory={})",
            model.as_deref().unwrap_or("absent"),
            reported.map_or_else(|| "absent".to_string(), |bytes| format!("{bytes} bytes")),
        )
    };

    Some(AcceleratorIdentity {
        sku,
        // Jetson modules do not partition. The row records this as
        // `not_applicable`; reporting `true` here would reject every board.
        partitioned: false,
    })
}

/// The row SKU for a board this recognizes, or `None` for one it does not.
fn recognized_jetson_sku(model: &str, reported_bytes: u64) -> Option<String> {
    let (module, capacity_gb) = (
        jetson_module(model)?,
        jetson_module_capacity_gb(reported_bytes)?,
    );
    {
        // `Super` is a module variant and the row spells it after the
        // capacity. Matched as a trailing word rather than anywhere in
        // the string, so a board whose name merely contains it does not
        // acquire the variant.
        let suffix = if model.split_whitespace().last() == Some("Super") {
            " Super"
        } else {
            ""
        };
        Some(format!("Jetson {module} {capacity_gb}GB{suffix}"))
    }
}

/// The module name inside a Jetson board model string.
///
/// The model reads like `NVIDIA Jetson Orin Nano Engineering Reference
/// Developer Kit Super`, and the row names the module: `Orin Nano`. So the
/// words between `Jetson` and the kit description are the module, and the
/// kit description is what the row does not carry.
fn jetson_module(model: &str) -> Option<String> {
    let after = model.split("Jetson ").nth(1)?;
    let module: Vec<&str> = after
        .split_whitespace()
        .take_while(|word| !matches!(*word, "Engineering" | "Developer" | "Reference" | "Kit"))
        .collect();
    (!module.is_empty()).then(|| module.join(" "))
}

/// The module capacity a reported total corresponds to, in GB as the row
/// spells it.
///
/// Reported total is always below nominal — firmware and the GPU carveout
/// are taken before the kernel counts what is left — so this rounds up to
/// the module capacity that contains it. Only the capacities Jetson modules
/// actually ship in are accepted: a total that lands outside them is not a
/// board this knows, and saying so is better than naming a capacity that
/// does not exist.
fn jetson_module_capacity_gb(reported_bytes: u64) -> Option<u64> {
    const SHIPPING_CAPACITIES_GB: [u64; 5] = [4, 8, 16, 32, 64];
    const BYTES_PER_GB: u64 = 1024 * 1024 * 1024;
    SHIPPING_CAPACITIES_GB.into_iter().find(|gb| {
        let nominal = gb * BYTES_PER_GB;
        // The same window the carveout justifies, applied to one capacity
        // at a time rather than used to choose between rows.
        reported_bytes <= nominal && u128::from(reported_bytes) * 100 >= u128::from(nominal) * 80
    })
}

/// `MemTotal` from `/proc/meminfo`, in bytes.
///
/// The kernel prints this in kibibytes with a `kB` unit that has meant KiB
/// since the file existed. A line whose unit is anything else is a file this
/// parser does not understand, and is read as absent rather than converted
/// on a guess.
#[must_use]
pub fn mem_total_from_meminfo(body: &str) -> Option<u64> {
    let line = body
        .lines()
        .find(|line| line.starts_with("MemTotal:"))?
        .strip_prefix("MemTotal:")?;
    let mut fields = line.split_whitespace();
    let value: u64 = fields.next()?.parse().ok()?;
    match fields.next() {
        Some("kB") => value.checked_mul(1024),
        _ => None,
    }
}

/// NVIDIA's PCI vendor id. Stable since the company existed; a device
/// reporting it is an NVIDIA part regardless of what any driver says.
const NVIDIA_PCI_VENDOR: &str = "0x10de";

/// PCI addresses of NVIDIA display or 3D controllers in an enumeration.
///
/// Reads only the vendor and class columns. The class is checked because a
/// vendor match alone would also count an audio function — a discrete card
/// commonly presents an HDMI audio device on the same board, and counting
/// it would report two accelerators where the machine has one.
///
/// A line this cannot parse is skipped rather than failing the whole
/// enumeration: this fact is evidence, and one malformed line should not
/// discard the devices either side of it. Nothing matches on the result,
/// so a skipped line cannot admit a machine it should not.
#[must_use]
pub fn nvidia_pci_functions(body: &str) -> Vec<String> {
    body.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let address = fields.next()?;
            let vendor = fields.next()?;
            let _device = fields.next()?;
            let class = fields.next()?;
            if !vendor.eq_ignore_ascii_case(NVIDIA_PCI_VENDOR) {
                return None;
            }
            // Parsed as a number rather than sliced as text. The token is
            // ASCII hex on any real bus, but slicing bytes off a string
            // that might not be would panic on a multi-byte character —
            // and this function documents that it SKIPS what it cannot
            // parse. `from_str_radix` rejects a malformed token instead.
            let value = u32::from_str_radix(class.trim_start_matches("0x"), 16).ok()?;
            // PCI base class 0x03 is "display controller"; the subclass
            // separates VGA (0x00) from 3D (0x02). Both are the device an
            // accelerator presents, and neither is the audio function
            // (base class 0x04) on the same board.
            ((value >> 16) & 0xff == 0x03).then(|| address.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{jetpack_for_l4t, jetpack_row_version, L4tRelease};

    #[test]
    fn the_jetpack_mapping_claims_only_the_feature_release() {
        // An L4T revision does not determine a JetPack patch: NVIDIA's
        // archive pairs 36.5.0 with 6.2.2 and 36.5.2 with 6.2.3. Reading a
        // patch off the line is therefore wrong on every revision but the
        // one it was read from. Asserted as a property over every line the
        // mapping answers for, because the place this gets got wrong is
        // the next arm somebody adds, not the two that exist.
        for revision_major in 0..=16 {
            for revision_minor in 0..=4 {
                let release = L4tRelease {
                    major: 36,
                    revision_major,
                    revision_minor,
                };
                let Some(jetpack) = jetpack_for_l4t(release) else {
                    continue;
                };
                assert_eq!(
                    jetpack.matches('.').count(),
                    1,
                    "r36.{revision_major}.{revision_minor} maps to `{jetpack}`, which names a \
                     patch release the L4T line cannot determine"
                );
                assert_eq!(
                    jetpack_row_version(jetpack).as_deref(),
                    Some(jetpack),
                    "`{jetpack}` must already be what a row records"
                );
            }
        }
    }

    #[test]
    fn both_l4t_revisions_of_jetpack_6_2_answer_alike() {
        // The whole point of the row generalisation: r36.4 and r36.5 are
        // the same support claim, so they must not resolve to different
        // JetPack strings and land on different rows.
        let r36_4 = jetpack_for_l4t(L4tRelease {
            major: 36,
            revision_major: 4,
            revision_minor: 3,
        });
        let r36_5 = jetpack_for_l4t(L4tRelease {
            major: 36,
            revision_major: 5,
            revision_minor: 0,
        });
        assert_eq!(r36_4, Some("6.2"));
        assert_eq!(r36_4, r36_5);
    }
}
