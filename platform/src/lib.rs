// SPDX-License-Identifier: Apache-2.0

//! `tensorplate-platform` — the platform support row registry types.
//!
//! This crate owns platform identity for TensorPlate: the exact support
//! rows the registry stores, the roadmap targets that are deliberately not
//! rows, and the typed platform-reason vocabulary that `doctor`, deploy
//! admission, and status all emit.
//!
//! The organizing rule is **exactness**. A row names one OS version, one
//! driver stack, one CPU architecture and vendor, one accelerator SKU, and
//! one validation environment, because evidence recorded on one row never
//! transfers to another. Anything not exact enough to be a row is a
//! [`RoadmapTarget`] instead, and roadmap targets are never matched
//! against a detected platform and never counted as supported.
//!
//! Schemas under `config/schemas/` are the language-neutral source of
//! truth; the types here mirror them and version on the same config-schema
//! track, independent of the cross-process protocol version. Validation is
//! unavoidable on every decoding path and failures are typed, so an
//! unsupported or malformed platform fails closed rather than being
//! silently downgraded.

#![forbid(unsafe_code)]

pub mod error;
pub mod reason;
pub mod roadmap;
pub mod row;

pub use error::PlatformRegistryError;
pub use reason::PlatformReason;
pub use roadmap::{RoadmapTarget, ROADMAP_TARGET_SCHEMA_VERSION};
pub use row::{
    Accelerator, BackendPackageSet, CpuArchitecture, CpuIdentity, CpuVendor, Evidence, Gate,
    GateSemantics, GateValue, KernelDriverStack, ModelClassRowRef, OsIdentity, PackageChannel,
    Partitioning, PlatformSupportRow, Provenance, StackComponent, SupportLevel,
    ValidationEnvironment, ValidationEnvironmentKind, PLATFORM_SUPPORT_ROW_SCHEMA_VERSION,
};
