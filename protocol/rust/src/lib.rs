// SPDX-License-Identifier: Apache-2.0

//! `tensorplate-protocol` - shared Rust types for TensorPlate cross-component
//! schemas.
//!
//! V01-E01-F03 only ships the crate skeleton so the workspace builds and
//! reserves a stable place for the schema-derived types. Real type
//! definitions land in V01-E02 (core value types and protocol schemas) and
//! V01-E13 (model bundle manifest).

#![forbid(unsafe_code)]

/// Crate-level marker used by the v0.1 scaffolding tests. Replaced by the
/// real protocol surface in V01-E02.
pub const SKELETON_MARKER: &str = "tensorplate-protocol-skeleton";

/// Returns the protocol crate version string compiled from Cargo metadata.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::{version, SKELETON_MARKER};

    #[test]
    fn marker_is_stable() {
        assert_eq!(SKELETON_MARKER, "tensorplate-protocol-skeleton");
    }

    #[test]
    fn version_is_non_empty() {
        assert!(!version().is_empty());
    }
}
