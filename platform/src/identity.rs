// SPDX-License-Identifier: Apache-2.0
//
// What a probe reports about the machine it is running on, and the traits
// that produce it.
//
// Detected identity is deliberately a separate type from a support row: a
// row is a claim the project has committed to, an identity is an
// observation. Matching one against the other is the registry's job, and
// keeping them distinct is what stops "we saw an NVIDIA GPU" from being
// read as "this GPU is supported".

use crate::error::PlatformProbeError;
use crate::row::{CpuArchitecture, CpuVendor};

/// A CPU architecture as *observed*, which may be one no row names.
///
/// Detection uses an open value where the row schema uses a closed enum:
/// a machine really can report `riscv64`, and collapsing that into a probe
/// failure would make [`crate::PlatformReason::UnsupportedCpuArch`]
/// unreachable — the platform would be reported as undetectable rather
/// than as unsupported, which is a different and less actionable claim.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DetectedArchitecture {
    /// An architecture the row schema can express.
    Known(CpuArchitecture),
    /// An architecture no row names, carried verbatim for diagnosis.
    Other(String),
}

impl DetectedArchitecture {
    /// The row-schema value, if this architecture is one rows can name.
    #[must_use]
    pub fn known(&self) -> Option<CpuArchitecture> {
        match self {
            Self::Known(architecture) => Some(*architecture),
            Self::Other(_) => None,
        }
    }

    /// What the machine reported, for diagnosis.
    #[must_use]
    pub fn as_reported(&self) -> &str {
        match self {
            Self::Known(architecture) => architecture.as_str(),
            Self::Other(raw) => raw,
        }
    }
}

/// A CPU vendor as *observed*, which may be one no row names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DetectedVendor {
    /// A vendor the row schema can express.
    Known(CpuVendor),
    /// A vendor no row names, carried verbatim for diagnosis.
    Other(String),
}

impl DetectedVendor {
    /// The row-schema value, if this vendor is one rows can name.
    #[must_use]
    pub fn known(&self) -> Option<CpuVendor> {
        match self {
            Self::Known(vendor) => Some(*vendor),
            Self::Other(_) => None,
        }
    }

    /// What the machine reported, for diagnosis.
    #[must_use]
    pub fn as_reported(&self) -> &str {
        match self {
            Self::Known(vendor) => vendor.as_str(),
            Self::Other(raw) => raw,
        }
    }
}

/// What the host reports about its OS and CPU, before any accelerator is
/// considered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostIdentity {
    pub architecture: DetectedArchitecture,
    pub vendor: DetectedVendor,
    /// OS name as the row schema spells it (e.g. the distribution or
    /// platform name, not a kernel string).
    pub os_name: String,
    /// Exact OS version. A row naming a different version never matches,
    /// because evidence does not transfer between versions.
    pub os_version: String,
    /// Image or distribution identity where the OS version alone is not
    /// exact, e.g. a JetPack release on an Ubuntu base. Absent when the
    /// platform has no such concept.
    pub image_identity: Option<String>,
    /// Machine-comparable shape the host is running on (e.g. a cloud
    /// machine type), where the platform exposes one. A row scoped to a
    /// machine shape matches only a host reporting the same value, so an
    /// accelerator in an unvalidated chassis does not inherit a claim
    /// recorded on one specific shape.
    pub machine_type: Option<String>,
}

/// What the accelerator reports about itself. Absent on hosts with no
/// accelerator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceleratorIdentity {
    /// Exact SKU as the platform reports it. Compared verbatim against exact
    /// rows and without normalization against any explicit family policy: a
    /// near miss is unsupported, never a nearest match.
    pub sku: String,
    /// Whether the device is partitioned. Partitioned devices are rejected
    /// before any SKU comparison, so a partitioned instance of a supported
    /// SKU never resolves to its row.
    pub partitioned: bool,
    /// How many accelerators the host reports.
    ///
    /// Carried in the identity rather than left to the probe so a
    /// multi-device host produces a verdict instead of an error: the
    /// count is a fact about the machine, and a machine no row claims is
    /// unsupported, not undetectable. Rejected before any SKU comparison
    /// for the same reason `partitioned` is -- two of a supported card is
    /// a topology nothing was validated on, not a degraded version of the
    /// row that claims one.
    pub device_count: u32,
}

/// A complete observation of the machine: host plus accelerator, if any.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DetectedPlatform {
    pub host: HostIdentity,
    pub accelerator: Option<AcceleratorIdentity>,
}

impl DetectedPlatform {
    /// An accelerator-less observation.
    #[must_use]
    pub fn host_only(host: HostIdentity) -> Self {
        Self {
            host,
            accelerator: None,
        }
    }

    /// An observation with an accelerator.
    #[must_use]
    pub fn with_accelerator(host: HostIdentity, accelerator: AcceleratorIdentity) -> Self {
        Self {
            host,
            accelerator: Some(accelerator),
        }
    }
}

/// Reads host OS and CPU identity from the running machine.
///
/// The registry consumes this trait rather than any concrete probe, so the
/// OS-specific implementations stay out of the matching logic and tests
/// can supply recorded identities instead of real hardware.
pub trait HostProbe {
    /// Detect host identity, or fail with a typed error.
    ///
    /// A value that is readable but that no row names is **not** an
    /// error: return it as [`DetectedArchitecture::Other`] or
    /// [`DetectedVendor::Other`]. Failing instead would report an
    /// unsupported machine as an undetectable one and make
    /// [`crate::PlatformReason::UnsupportedCpuArch`] and
    /// [`crate::PlatformReason::UnsupportedCpuVendor`] unreachable.
    ///
    /// # Errors
    ///
    /// Returns an error only when a detection source cannot be read, or
    /// when what it reports cannot be interpreted at all — never merely
    /// because the value is off-matrix.
    fn detect_host(&self) -> Result<HostIdentity, PlatformProbeError>;
}

/// Reads accelerator identity from the running machine.
pub trait AcceleratorProbe {
    /// Detect the accelerator, returning `Ok(None)` when the machine has
    /// none. An accelerator that is present but unreadable is an error,
    /// not an absence: those two must never look alike.
    ///
    /// # Errors
    ///
    /// Returns an error when an accelerator is present but its identity
    /// cannot be read.
    fn detect_accelerator(&self) -> Result<Option<AcceleratorIdentity>, PlatformProbeError>;
}
