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
// | `image_identity` (Jetson) | L4T `r36.4.3` | `L4T r36.4.x (…)` |
//
// Matching is exact string equality, so detection must produce the row's
// spelling or the row can never match — including on the very hardware it
// describes. Normalizing here, once, is what keeps that true. The
// unnormalized strings are not thrown away: they are carried in
// [`ExactHostFacts`] for evidence recording, which needs the precision
// matching deliberately discards.

use crate::error::PlatformProbeError;
use crate::identity::{DetectedArchitecture, DetectedVendor, HostIdentity};
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
    /// Body of the GCE metadata machine-type response, e.g.
    /// `projects/1234/machineTypes/g2-standard-8`.
    pub gce_machine_type: Option<String>,
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
}

/// A detected host: the row-comparable identity plus the exact facts that
/// identity was normalized from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostReport {
    pub identity: HostIdentity,
    pub exact: ExactHostFacts,
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
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value);
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    })
}

/// The x86 CPU vendor from `/proc/cpuinfo`.
///
/// Reads the `vendor_id` of the first core; every core on a host reports
/// the same vendor, and a row names the host's vendor, not a core's.
#[must_use]
pub fn cpuinfo_vendor(content: &str) -> Option<DetectedVendor> {
    let raw = content.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim() == "vendor_id").then(|| value.trim().to_string())
    })?;
    Some(match raw.as_str() {
        "GenuineIntel" => DetectedVendor::Known(CpuVendor::Intel),
        "AuthenticAMD" => DetectedVendor::Known(CpuVendor::Amd),
        _ => DetectedVendor::Other(raw),
    })
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

    /// The release at row granularity, e.g. `r36.4.x`.
    ///
    /// The patch is collapsed to a literal `x` because a row's evidence is
    /// recorded against an L4T minor line, not a single patch: a device
    /// taking a patch update must not silently stop matching the row that
    /// describes it.
    #[must_use]
    pub fn row_granularity(&self) -> String {
        format!("r{}.{}.x", self.major, self.revision_major)
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

/// The JetPack version a row records, from the `nvidia-jetpack` package
/// version.
///
/// dpkg reports `6.2-b77`; the Debian revision after `-` is a build
/// number, not part of the JetPack version a row names.
#[must_use]
pub fn jetpack_version(package_version: &str) -> Option<String> {
    let trimmed = package_version.trim();
    let version = trimmed.split('-').next()?.trim();
    (!version.is_empty()).then(|| version.to_string())
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
    let version = sources
        .nvidia_jetpack_version
        .as_deref()
        .and_then(jetpack_version)
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

    let vendor = sources
        .cpuinfo
        .as_deref()
        .and_then(cpuinfo_vendor)
        .ok_or_else(|| PlatformProbeError::Unrecognized {
            source_name: "/proc/cpuinfo".to_string(),
            detail: "no CPU vendor reported in /proc/cpuinfo".to_string(),
        })?;

    Ok((name, version, None, vendor))
}
