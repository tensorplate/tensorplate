// SPDX-License-Identifier: Apache-2.0
//
// Vendor-neutral accelerator memory observations and resolved capability
// records.
//
// Detection records what the machine reports. The registry then bounds that
// observation by the resolved support row before it becomes an admission limit.
// Keeping those two values distinct prevents a machine with more memory than
// the validated row from silently broadening the row's support claim.

use tensorplate_protocol::PlatformMemoryProfileName;

use crate::identity::AcceleratorIdentity;

/// Accelerator identity plus the memory facts reported by the machine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceleratorObservation {
    pub identity: AcceleratorIdentity,
    /// Memory reported by the accelerator probe. Absence means the probe
    /// retained a usable identity but could not read a trustworthy capacity.
    pub memory_bytes: Option<u64>,
    pub memory_profile: PlatformMemoryProfileName,
}

/// Capability record resolved from an observation and a support row.
///
/// The fields are read-only because `max_resident_model_memory` is valid only
/// after the registry has bounded detected memory by the row budget.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlatformCapability {
    row_id: String,
    memory_profile: PlatformMemoryProfileName,
    detected_memory_bytes: Option<u64>,
    row_memory_budget_bytes: u64,
    max_resident_model_memory: u64,
}

impl PlatformCapability {
    pub(crate) fn bounded(
        row_id: &str,
        memory_profile: PlatformMemoryProfileName,
        detected_memory_bytes: Option<u64>,
        row_memory_budget_bytes: u64,
    ) -> Self {
        Self {
            row_id: row_id.to_string(),
            memory_profile,
            detected_memory_bytes,
            row_memory_budget_bytes,
            // A missing reading adds no tighter machine-observed bound. The
            // validated row budget still caps admission, so this neither
            // collapses capacity to zero nor leaves deployment unbounded.
            max_resident_model_memory: detected_memory_bytes
                .map_or(row_memory_budget_bytes, |detected| {
                    detected.min(row_memory_budget_bytes)
                }),
        }
    }

    #[must_use]
    pub fn row_id(&self) -> &str {
        &self.row_id
    }

    #[must_use]
    pub fn memory_profile(&self) -> PlatformMemoryProfileName {
        self.memory_profile
    }

    #[must_use]
    pub fn detected_memory_bytes(&self) -> Option<u64> {
        self.detected_memory_bytes
    }

    #[must_use]
    pub fn row_memory_budget_bytes(&self) -> u64 {
        self.row_memory_budget_bytes
    }

    #[must_use]
    pub fn max_resident_model_memory(&self) -> u64 {
        self.max_resident_model_memory
    }
}
