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

/// What the host reports about its OS and CPU, before any accelerator is
/// considered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostIdentity {
    pub architecture: CpuArchitecture,
    pub vendor: CpuVendor,
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
}

/// What the accelerator reports about itself. Absent on hosts with no
/// accelerator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceleratorIdentity {
    /// Exact SKU as the platform reports it. Compared verbatim against a
    /// row's SKU: a near-miss is unsupported, never a nearest match.
    pub sku: String,
    /// Whether the device is partitioned. Partitioned devices are rejected
    /// before any SKU comparison, so a partitioned instance of a supported
    /// SKU never resolves to its row.
    pub partitioned: bool,
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
    /// Detect host identity, or fail with a typed error. Detection that
    /// cannot determine the platform fails rather than guessing.
    ///
    /// # Errors
    ///
    /// Returns an error when the host cannot be identified — an
    /// unreadable source, or a value no supported platform reports.
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
