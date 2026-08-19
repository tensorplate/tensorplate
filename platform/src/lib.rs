// SPDX-License-Identifier: Apache-2.0

//! `tensorplate-platform` — the platform support row registry types.
//!
//! This crate owns platform identity for TensorPlate: the support
//! rows the registry stores, the roadmap targets that are deliberately not
//! rows, and the typed platform-reason vocabulary that `doctor`, deploy
//! admission, and status will emit once they are wired to it.
//!
//! The organizing rule is **explicit scope**. A row names one OS version,
//! driver stack, CPU architecture, and validation environment. Accelerator
//! matching is exact by default; an explicit family row may define a broader,
//! lower-priority Preview compatibility envelope without transferring its
//! representative hardware evidence to every member SKU. CPU vendors are the
//! other set-valued identity: a row with an accelerator names exactly one,
//! while an accelerator-less utility row states the vendors it actually
//! covers. Roadmap targets are never matched against a detected platform and
//! never counted as supported.
//!
//! Schemas under `config/schemas/` are the language-neutral source of
//! truth; the types here mirror them and version on the same config-schema
//! track, independent of the cross-process protocol version. Validation is
//! unavoidable — the `from_json` constructors are the only way to obtain a
//! row or a target, and neither type implements `Deserialize` — and
//! failures are typed, so an unsupported or malformed platform fails
//! closed rather than being silently downgraded.

#![forbid(unsafe_code)]

pub mod accelerator;
pub mod capability;
pub mod detect;
pub mod error;
pub mod identity;
pub mod matrix;
pub mod probe;
pub mod reason;
pub mod registry;
pub mod roadmap;
pub mod row;

pub use accelerator::{
    identify_accelerator, AcceleratorReport, AcceleratorSources, ExactAcceleratorFacts,
    NvidiaSmiProbe,
};
pub use capability::{AcceleratorObservation, PlatformCapability};
pub use detect::{
    identify, identify_jetson_accelerator, identify_platform, nvidia_pci_functions, ExactHostFacts,
    HostReport, HostSources, L4tRelease, PlatformReport,
};
pub use error::{PlatformProbeError, PlatformRegistryError};
pub use identity::{
    AcceleratorIdentity, AcceleratorProbe, DetectedArchitecture, DetectedPlatform, DetectedVendor,
    HostIdentity, HostProbe,
};
pub use matrix::render_support_matrix;
pub use probe::SystemHostProbe;
pub use reason::PlatformReason;
pub use registry::{PlatformRegistry, ProfileSelection, RowMatch};
pub use roadmap::{RoadmapTarget, ROADMAP_TARGET_SCHEMA_VERSION};
pub use row::{
    Accelerator, AcceleratorMatchPolicy, BackendPackageSet, CpuArchitecture, CpuIdentity,
    CpuVendor, Evidence, Gate, GateSemantics, GateValue, KernelDriverStack, ModelClassRowRef,
    OsIdentity, PackageChannel, Partitioning, PlatformSupportRow, Provenance, StackComponent,
    SupportLevel, ValidationEnvironment, ValidationEnvironmentKind,
    PLATFORM_SUPPORT_ROW_SCHEMA_VERSION,
};
