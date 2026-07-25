// SPDX-License-Identifier: Apache-2.0
//
// The typed platform-reason vocabulary: why a detected platform is not a
// supported combination.
//
// This crate owns the enum; `doctor`, deploy admission, and status all
// emit these values rather than prose, so the same condition reads the
// same way everywhere. Trigger conditions and user-facing rendering are
// frozen separately by the doctor work; the values themselves are frozen
// here.

use serde::{Deserialize, Serialize};

/// Why a detected platform is not a supported combination.
///
/// `try_from` pins decoding to the plain string form: serde's derived
/// `Deserialize` for a fieldless enum would also accept the
/// externally-tagged map form (`{"unsupported_os_version": null}`), which
/// a schema pinning `type: "string"` rejects.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "String")]
pub enum PlatformReason {
    /// The accelerator SKU is not the exact SKU any row names. Never a
    /// nearest match: a near-miss SKU is unsupported, not degraded.
    UnsupportedAcceleratorSku,
    /// The OS version is below the row's floor or is not the exact version
    /// a row names.
    UnsupportedOsVersion,
    /// The CPU architecture is not one this release builds for.
    UnsupportedCpuArch,
    /// The CPU architecture is supported but the vendor is not, on a row
    /// where vendor is load-bearing.
    UnsupportedCpuVendor,
    /// The accelerator is partitioned. Partitioned devices are rejected
    /// before model load rather than served at reduced capacity.
    MigModeEnabled,
    /// A required backend package is absent from the installed package
    /// set.
    MissingBackendPackage,
    /// A required driver or compute runtime is absent or version-mismatched
    /// against the row.
    MissingDriverRuntime,
    /// The accelerator runtime is installed but unavailable at run time —
    /// distinct from a package being missing.
    AcceleratorRuntimeUnavailable,
    /// A telemetry collector the row expects failed, so signals the row
    /// depends on are unavailable.
    TelemetryDegraded,
    /// The detected identity exactly matches a Planned row: the platform
    /// is known and defined, but carries no validation evidence yet.
    RowPlannedNotValidated,
}

impl PlatformReason {
    /// Every reason, in declaration order. Downstream conformance tests
    /// iterate this to prove each reason has a trigger and a rendering.
    pub const ALL: [Self; 10] = [
        Self::UnsupportedAcceleratorSku,
        Self::UnsupportedOsVersion,
        Self::UnsupportedCpuArch,
        Self::UnsupportedCpuVendor,
        Self::MigModeEnabled,
        Self::MissingBackendPackage,
        Self::MissingDriverRuntime,
        Self::AcceleratorRuntimeUnavailable,
        Self::TelemetryDegraded,
        Self::RowPlannedNotValidated,
    ];

    /// Stable serialized name (snake_case). The exhaustive match makes a
    /// newly added reason a compile error here rather than a silently
    /// unspelled one.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedAcceleratorSku => "unsupported_accelerator_sku",
            Self::UnsupportedOsVersion => "unsupported_os_version",
            Self::UnsupportedCpuArch => "unsupported_cpu_arch",
            Self::UnsupportedCpuVendor => "unsupported_cpu_vendor",
            Self::MigModeEnabled => "mig_mode_enabled",
            Self::MissingBackendPackage => "missing_backend_package",
            Self::MissingDriverRuntime => "missing_driver_runtime",
            Self::AcceleratorRuntimeUnavailable => "accelerator_runtime_unavailable",
            Self::TelemetryDegraded => "telemetry_degraded",
            Self::RowPlannedNotValidated => "row_planned_not_validated",
        }
    }
}

impl std::fmt::Display for PlatformReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<String> for PlatformReason {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::ALL
            .into_iter()
            .find(|reason| reason.as_str() == value)
            .ok_or_else(|| format!("unknown platform reason `{value}`"))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::PlatformReason;

    #[test]
    fn the_vocabulary_is_ten_distinct_values() {
        let mut spellings: Vec<&str> = PlatformReason::ALL.iter().map(|r| r.as_str()).collect();
        assert_eq!(spellings.len(), 10);
        spellings.sort_unstable();
        spellings.dedup();
        assert_eq!(spellings.len(), 10, "reason spellings must be distinct");
    }

    #[test]
    fn spellings_round_trip() {
        for reason in PlatformReason::ALL {
            let json = serde_json::to_string(&reason).expect("serialize");
            assert_eq!(json, format!("\"{}\"", reason.as_str()));
            let back: PlatformReason = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, reason);
            assert_eq!(reason.to_string(), reason.as_str());
        }
    }

    #[test]
    fn non_string_forms_reject() {
        serde_json::from_str::<PlatformReason>(r#"{"telemetry_degraded":null}"#)
            .expect_err("map form must reject");
        serde_json::from_str::<PlatformReason>("0").expect_err("variant index must reject");
        serde_json::from_str::<PlatformReason>(r#""not_a_reason""#)
            .expect_err("unknown spelling must reject");
    }
}
