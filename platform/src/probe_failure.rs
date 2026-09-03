// SPDX-License-Identifier: Apache-2.0
//
// Shared classification for accelerator-probe failures.
//
// The agent and `tensorplate doctor` both observe the same host and
// accelerator, but neither may depend on the other.  Keeping this decision in
// the platform crate prevents a readable-but-unexpected `nvidia-smi` answer
// from being called a broken driver in one path and a detection failure in the
// other.

use crate::{HostReport, PlatformProbeError, PlatformReason};

/// Operator-facing class for a failed accelerator probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AcceleratorProbeFailureClass {
    /// A physical NVIDIA device is present, but its driver-facing probe could
    /// not answer.
    MissingDriverRuntime,
    /// The failure does not prove that the driver is missing or broken.
    DetectionFailed,
}

impl AcceleratorProbeFailureClass {
    /// The frozen platform reason carried by this class, when one applies.
    #[must_use]
    pub const fn reason(self) -> Option<PlatformReason> {
        match self {
            Self::MissingDriverRuntime => Some(PlatformReason::MissingDriverRuntime),
            Self::DetectionFailed => None,
        }
    }
}

/// Classify an accelerator probe failure using the independently observed
/// host evidence.
///
/// An [`PlatformProbeError::Unreadable`] result can mean a missing or broken
/// driver, but only when the PCI bus independently reports an NVIDIA display
/// controller.  [`PlatformProbeError::Unrecognized`] means the tool answered
/// and this release could not interpret the answer (for example, a multi-GPU
/// topology or malformed row); blaming the driver in that case is incorrect.
#[must_use]
pub fn classify_accelerator_probe_failure(
    host: &HostReport,
    error: &PlatformProbeError,
) -> AcceleratorProbeFailureClass {
    if matches!(error, PlatformProbeError::Unreadable { .. })
        && !host.exact.nvidia_pci_functions.is_empty()
    {
        AcceleratorProbeFailureClass::MissingDriverRuntime
    } else {
        AcceleratorProbeFailureClass::DetectionFailed
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{identify, HostSources};

    fn host_with_pci(pci_devices: Option<&str>) -> HostReport {
        identify(&HostSources {
            uname_machine: Some("x86_64".to_string()),
            os_release: Some("NAME=Ubuntu\nVERSION_ID=\"24.04\"\n".to_string()),
            cpuinfo: Some("vendor_id : GenuineIntel\n".to_string()),
            pci_devices: pci_devices.map(str::to_string),
            ..HostSources::default()
        })
        .expect("the test host interprets")
    }

    #[test]
    fn an_unreadable_probe_is_a_driver_failure_only_with_pci_evidence() {
        let error = PlatformProbeError::Unreadable {
            source_name: "nvidia-smi".to_string(),
            detail: "driver communication failed".to_string(),
        };
        let with_card = host_with_pci(Some("0000:00:04.0 0x10de 0x27b8 0x030200\n"));
        let without_card = host_with_pci(Some(""));

        let classified = classify_accelerator_probe_failure(&with_card, &error);
        assert_eq!(
            classified,
            AcceleratorProbeFailureClass::MissingDriverRuntime
        );
        assert_eq!(
            classified.reason(),
            Some(PlatformReason::MissingDriverRuntime)
        );
        assert_eq!(
            classify_accelerator_probe_failure(&without_card, &error),
            AcceleratorProbeFailureClass::DetectionFailed
        );
    }

    #[test]
    fn an_answer_that_cannot_be_interpreted_never_blames_the_driver() {
        let host = host_with_pci(Some("0000:00:04.0 0x10de 0x27b8 0x030200\n"));
        let error = PlatformProbeError::Unrecognized {
            source_name: "nvidia-smi".to_string(),
            detail: "expected exactly one device, found 2".to_string(),
        };

        let classified = classify_accelerator_probe_failure(&host, &error);
        assert_eq!(classified, AcceleratorProbeFailureClass::DetectionFailed);
        assert_eq!(classified.reason(), None);
    }
}
