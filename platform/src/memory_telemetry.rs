// SPDX-License-Identifier: Apache-2.0
//
// Per-row memory telemetry: what a machine actually has, read against
// what its row budgets.
//
// The distinction this exists to carry is the memory model. A discrete
// GPU has two pools and the accelerator's is the one a model loads into;
// a unified-memory platform has ONE pool that the host and the
// accelerator both draw from, so the same number means something
// different. Reporting a Jetson's 8 GiB as if it were a discrete
// framebuffer would tell an operator they have 8 GiB for a model when
// the OS is living in it too.

use crate::capability::AcceleratorObservation;
use crate::detect::PlatformReport;
use crate::row::{GateValue, PlatformSupportRow};
use tensorplate_protocol::PlatformMemoryProfileName;

/// Memory as one row sees it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlatformMemoryTelemetry {
    row_id: String,
    memory_profile: PlatformMemoryProfileName,
    host_total_bytes: Option<u64>,
    accelerator_total_bytes: Option<u64>,
    row_nominal_capacity_bytes: u64,
    memory_gate: GateValue,
}

impl PlatformMemoryTelemetry {
    /// Collect what the row and the observation together say about memory.
    ///
    /// Returns `None` when the row declares no accelerator: a CPU-only row
    /// has a host pool and no second one, and a telemetry record whose
    /// accelerator half is always absent would be noise on every such row.
    #[must_use]
    pub fn collect(row: &PlatformSupportRow, report: &PlatformReport) -> Option<Self> {
        let declared = row.accelerator()?;
        let observed: Option<&AcceleratorObservation> = report.accelerator.as_ref();
        let host_total_bytes = report.host.exact.host_total_memory_bytes;
        let accelerator_total_bytes = match declared.memory_profile {
            // One pool. The accelerator's total IS the host's, and saying
            // so is the whole point of carrying the profile: a caller
            // must not add these two together.
            PlatformMemoryProfileName::UnifiedMemory => {
                observed.and_then(|o| o.memory_bytes).or(host_total_bytes)
            }
            // Two pools, and only the probe can report the second.
            PlatformMemoryProfileName::DiscreteGpu => observed.and_then(|o| o.memory_bytes),
        };
        Some(Self {
            row_id: row.row_id().to_string(),
            memory_profile: declared.memory_profile,
            host_total_bytes,
            accelerator_total_bytes,
            row_nominal_capacity_bytes: declared.memory_bytes,
            memory_gate: row.gate_semantics().memory.gate,
        })
    }

    #[must_use]
    pub fn row_id(&self) -> &str {
        &self.row_id
    }

    #[must_use]
    pub fn memory_profile(&self) -> PlatformMemoryProfileName {
        self.memory_profile
    }

    /// Whether the host and accelerator figures name the same physical
    /// pool. True on unified-memory rows, and the reason a caller must
    /// not sum them.
    #[must_use]
    pub fn shares_one_pool(&self) -> bool {
        matches!(
            self.memory_profile,
            PlatformMemoryProfileName::UnifiedMemory
        )
    }

    #[must_use]
    pub fn host_total_bytes(&self) -> Option<u64> {
        self.host_total_bytes
    }

    #[must_use]
    pub fn accelerator_total_bytes(&self) -> Option<u64> {
        self.accelerator_total_bytes
    }

    #[must_use]
    pub fn row_nominal_capacity_bytes(&self) -> u64 {
        self.row_nominal_capacity_bytes
    }

    #[must_use]
    pub fn memory_gate(&self) -> GateValue {
        self.memory_gate
    }

    /// The usable capacity this row may budget on this observation.
    ///
    /// A row records the device's nominal capacity while runtime probes
    /// report usable memory after firmware and driver reservations. Those
    /// are deliberately different quantities: an L4 row says 24 GiB and
    /// `nvidia-smi` reports less. Treating nominal capacity as a minimum
    /// would therefore reject the exact device the row was validated on.
    ///
    /// The observed value is instead a ceiling, bounded by the row's
    /// nominal claim. This is the same rule used by [`crate::PlatformCapability`]
    /// for model admission. `None` means no usable-capacity observation was
    /// available; callers report that as collector availability rather than
    /// inventing a memory shortfall.
    #[must_use]
    pub fn effective_budget_bytes(&self) -> Option<u64> {
        self.accelerator_total_bytes
            .map(|observed| observed.min(self.row_nominal_capacity_bytes))
    }
}
