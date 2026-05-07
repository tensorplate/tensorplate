// SPDX-License-Identifier: Apache-2.0

//! `tensorplate-protocol` - shared Rust types for TensorPlate cross-component
//! schemas.
//!
//! V01-E01-F03 ships the crate skeleton; V01-E01-F06 adds the cross-process
//! version constants that mirror `include/tensorplate/version.hpp`. Real
//! type definitions land in V01-E02 (core value types and protocol
//! schemas) and V01-E13 (model bundle manifest).
//!
//! ## Versioning
//!
//! The protocol and bundle-format constants below are an independent
//! semver track from the crate's `CARGO_PKG_VERSION` (the runtime
//! version). They are hand-mirrored from CMake until V01-E02 introduces
//! the schema files that own these numbers; `version_consistency_test`
//! in this crate plus the C++ `Version` tests guard against silent drift.
//! See `docs/architecture/versioning.md`.

#![forbid(unsafe_code)]

/// Cross-process protocol major version. Bumping this is a breaking change
/// to any schema under `protocol/schemas/`.
pub const PROTOCOL_VERSION_MAJOR: u32 = 0;

/// Cross-process protocol minor version. Additive schema changes.
pub const PROTOCOL_VERSION_MINOR: u32 = 1;

/// Cross-process protocol version string in `MAJOR.MINOR` form.
pub const PROTOCOL_VERSION: &str = "0.1";

/// Model bundle on-disk format major version. Bumping this changes how
/// `tensorplate-agent` lays out and verifies a deployed bundle.
pub const BUNDLE_FORMAT_VERSION_MAJOR: u32 = 0;

/// Model bundle on-disk format minor version.
pub const BUNDLE_FORMAT_VERSION_MINOR: u32 = 1;

/// Model bundle format version string in `MAJOR.MINOR` form.
pub const BUNDLE_FORMAT_VERSION: &str = "0.1";

/// Crate-level marker used by the v0.1 scaffolding tests. Replaced by the
/// real protocol surface in V01-E02.
pub const SKELETON_MARKER: &str = "tensorplate-protocol-skeleton";

/// Returns the protocol crate version string compiled from Cargo metadata.
/// This corresponds to the runtime release version, **not** the protocol
/// version. Use [`PROTOCOL_VERSION`] for the cross-process protocol.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::{
        version, BUNDLE_FORMAT_VERSION, BUNDLE_FORMAT_VERSION_MAJOR, BUNDLE_FORMAT_VERSION_MINOR,
        PROTOCOL_VERSION, PROTOCOL_VERSION_MAJOR, PROTOCOL_VERSION_MINOR, SKELETON_MARKER,
    };

    #[test]
    fn marker_is_stable() {
        assert_eq!(SKELETON_MARKER, "tensorplate-protocol-skeleton");
    }

    #[test]
    fn version_is_non_empty() {
        assert!(!version().is_empty());
    }

    #[test]
    fn protocol_version_strings_match_components() {
        assert_eq!(
            PROTOCOL_VERSION,
            format!("{PROTOCOL_VERSION_MAJOR}.{PROTOCOL_VERSION_MINOR}")
        );
    }

    #[test]
    fn bundle_format_version_strings_match_components() {
        assert_eq!(
            BUNDLE_FORMAT_VERSION,
            format!("{BUNDLE_FORMAT_VERSION_MAJOR}.{BUNDLE_FORMAT_VERSION_MINOR}")
        );
    }
}
