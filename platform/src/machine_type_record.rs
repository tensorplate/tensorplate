// SPDX-License-Identifier: Apache-2.0
//
// A Compute Engine machine type established without the network.
//
// The metadata service is the only authority on a GCE instance's machine
// type, and it is reached over the network. A host whose network access is
// denied still has to resolve its shape-scoped row, and it must not do so by
// reporting no machine type: a shape-scoped row whose thermal, power and
// throttle signals are context only admits a shape miss on technical
// prerequisites, so "undetected" would quietly become "admitted, unvalidated".
//
// So `tensorplate-agent` records what the metadata service answered, next to
// the local facts it answered on: the kernel boot ID, logical CPU count,
// `MemTotal`, and the NVIDIA display device ids on the PCI bus. When the
// service later cannot be reached, detection uses the recorded machine type only while every one of
// those facts is still exactly what it was. A record cannot survive a new
// kernel boot: start the agent online after every reboot before going offline.
// Anything else -- no record, a record this release cannot read, a fact that
// changed or cannot be read -- fails detection with an error that names
// why. There is no path from here to "no machine type".
//
// Everything in this module is pure. Reading and writing the file lives in
// [`crate::probe`].

use serde::{Deserialize, Serialize};
use tensorplate_protocol::install_paths::{INSTANCE_BINDING_PATH, MACHINE_TYPE_RECORD_PATH};
use tensorplate_protocol::serde_shape::is_canonical_identifier;

use crate::detect::{
    is_compute_engine, logical_cpu_count, machine_type_from_metadata, mem_total_from_meminfo,
    nvidia_display_devices, HostSources,
};
use crate::error::{PlatformProbeError, GCE_METADATA_SOURCE_NAME};
use crate::instance_binding::{check_live_instance, machine_type_changed, InstanceBinding};

/// What every unestablished-identity error opens with when the sources do
/// not say why the metadata service gave no answer.
const CONTEXT: &str =
    "host reports as a Compute Engine instance and the metadata service could not be reached";

/// What every unestablished-identity error opens with: an instance without
/// a live answer, and the cause class of the last attempt -- transient
/// unavailability, blocked access, or not reached -- with what the operator
/// can do about it. What the two statuses mean is Google's, from its
/// metadata server troubleshooting page; a status that does not pass may
/// come from something else answering in the server's place.
fn context(sources: &HostSources) -> String {
    match sources.gce_metadata_unanswered.as_deref() {
        Some("http-503") => "host reports as a Compute Engine instance and its metadata service \
             answered HTTP 503 (transient unavailability: Google documents 503 while the \
             metadata server boots or migrates or the host is under maintenance, and says it \
             resolves within a few seconds; if it persists, something other than the metadata \
             server may be answering for 169.254.169.254:80, such as a proxy or a custom route)"
            .to_string(),
        Some("http-429") => "host reports as a Compute Engine instance and its metadata service \
             answered HTTP 429 (transient unavailability: Google documents 429 as an endpoint's \
             rate limiting, and says to retry after a few seconds; if it persists, something \
             other than the metadata server may be answering for 169.254.169.254:80, such as a \
             proxy or a custom route)"
            .to_string(),
        Some("refused") => "host reports as a Compute Engine instance and connections to its \
             metadata service at 169.254.169.254:80 were refused (blocked access: a firewall \
             rule, a proxy or custom routing on this host rejects them; allow that address and \
             port for tensorplate-agent)"
            .to_string(),
        Some("timeout") => "host reports as a Compute Engine instance and nothing answered from \
             its metadata service within the budget (not reached: the network may not be up \
             yet, or a firewall rule, a proxy or custom routing drops the traffic)"
            .to_string(),
        _ => CONTEXT.to_string(),
    }
}

/// The only record layout this release reads or writes.
pub const MACHINE_TYPE_RECORD_SCHEMA_VERSION: u32 = 2;

/// Where a detected machine type came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MachineTypeSource {
    /// A live answer from the GCE metadata service.
    GceMetadata,
    /// A metadata answer `tensorplate-agent` recorded earlier in this kernel boot,
    /// used because the service could not be reached and every local fact
    /// the record is bound to is unchanged.
    RecordedFromMetadata,
}

impl MachineTypeSource {
    /// The token the agent logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GceMetadata => "gce_metadata",
            Self::RecordedFromMetadata => "recorded_gce_metadata",
        }
    }
}

/// What writing the record did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordWrite {
    /// The record was created or replaced.
    Written,
    /// The record already held exactly these bytes; nothing was written.
    Unchanged,
    /// There was no live metadata answer to record.
    NotApplicable,
    /// There was a live answer, but this fact it would be bound to is
    /// unavailable; nothing was written.
    FactsUnavailable(&'static str),
}

impl std::fmt::Display for RecordWrite {
    /// The token the agent logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Written => f.write_str("written"),
            Self::Unchanged => f.write_str("unchanged"),
            Self::NotApplicable => f.write_str("not_applicable"),
            Self::FactsUnavailable(fact) => write!(f, "not_recorded (unavailable: {fact})"),
        }
    }
}

/// The local facts a recorded machine type is bound to.
///
/// The boot ID restricts the record to one running kernel: a stopped GCE
/// instance can change machine type, and a disk can move to another instance,
/// but both start a new boot. Capacity equality alone cannot establish this.
/// The other facts also refuse changes within that boot; none derives a shape.
/// Once the metadata service answers, the instance binding refuses both
/// cases, and only reprovisioning records them again.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalShapeFacts {
    /// The kernel boot UUID from `/proc/sys/kernel/random/boot_id`.
    pub boot_id: String,
    /// `processor` entries in `/proc/cpuinfo`.
    pub logical_cpus: u32,
    /// `MemTotal`, in bytes. Compared exactly: a tolerance would be a
    /// guessed constant, and a changed total refuses loudly and heals on
    /// the next start that reaches the metadata service, unless the
    /// instance was given another machine type, which the instance binding
    /// refuses until the host is reprovisioned.
    pub mem_total_bytes: u64,
    /// `<vendor>:<device>` of every NVIDIA display function, sorted.
    pub nvidia_display_devices: Vec<String>,
}

impl LocalShapeFacts {
    /// Read the facts from host sources.
    ///
    /// # Errors
    ///
    /// Names the first fact that is not available. An unavailable fact is
    /// never treated as matching.
    pub fn from_sources(sources: &HostSources) -> Result<Self, &'static str> {
        let boot_id = sources
            .boot_id
            .as_deref()
            .map(str::trim)
            .filter(|id| is_boot_id(id))
            .ok_or("the kernel boot ID (no canonical UUID in /proc/sys/kernel/random/boot_id)")?
            .to_string();
        let logical_cpus = sources
            .cpuinfo
            .as_deref()
            .map(logical_cpu_count)
            .and_then(|count| u32::try_from(count).ok())
            .filter(|count| *count > 0)
            .ok_or("the logical CPU count (no `processor` entries in /proc/cpuinfo)")?;
        let mem_total_bytes = sources
            .proc_meminfo
            .as_deref()
            .and_then(mem_total_from_meminfo)
            .ok_or("MemTotal (no `MemTotal: <n> kB` line in /proc/meminfo)")?;
        let mut nvidia_display_devices = sources
            .pci_devices
            .as_deref()
            .map(|bus| {
                nvidia_display_devices(bus)
                    .into_iter()
                    .map(|(_address, id)| id)
                    .collect::<Vec<_>>()
            })
            .ok_or("the NVIDIA display devices (the PCI bus was not enumerated)")?;
        nvidia_display_devices.sort();
        Ok(Self {
            boot_id,
            logical_cpus,
            mem_total_bytes,
            nvidia_display_devices,
        })
    }
}

/// The record file's content.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineTypeRecord {
    pub schema_version: u32,
    /// The kernel boot in which the live metadata answer was obtained.
    pub boot_id: String,
    /// The bare machine type, e.g. `g2-standard-8`. Never the project-scoped
    /// resource name the metadata service answers with.
    pub machine_type: String,
    pub logical_cpus: u32,
    pub mem_total_bytes: u64,
    pub nvidia_display_devices: Vec<String>,
}

impl MachineTypeRecord {
    /// Parse and validate a record file's content.
    ///
    /// # Errors
    ///
    /// Says why the content is not a record this release can use: not JSON,
    /// an unknown or missing field, another schema version, a machine type
    /// that is not a canonical identifier -- which no row names, so a record
    /// carrying one would resolve as an unvalidated shape -- or a device
    /// entry that is not a lowercase `0x<vendor>:0x<device>` PCI id. Fact
    /// values are not range-checked here: a record is only ever used when
    /// they equal the live host's exactly.
    ///
    /// The reason never quotes the file. Detection errors reach the agent's
    /// journal, and a record is written by an account that is not root.
    pub fn parse(body: &str) -> Result<Self, String> {
        let record: Self = serde_json::from_str(body).map_err(|err| {
            let kind = match err.classify() {
                serde_json::error::Category::Io => "I/O",
                serde_json::error::Category::Syntax => "syntax",
                serde_json::error::Category::Data => "data",
                serde_json::error::Category::Eof => "truncated",
            };
            format!(
                "not a valid record ({kind} error at line {}, column {})",
                err.line(),
                err.column()
            )
        })?;
        if record.schema_version != MACHINE_TYPE_RECORD_SCHEMA_VERSION {
            return Err(format!(
                "schema_version {} is not {MACHINE_TYPE_RECORD_SCHEMA_VERSION}",
                record.schema_version
            ));
        }
        if !is_boot_id(&record.boot_id) {
            return Err("the boot ID is not a canonical UUID".to_string());
        }
        if !is_canonical_identifier(&record.machine_type) {
            return Err("the machine type is not a canonical identifier".to_string());
        }
        if !record.nvidia_display_devices.iter().all(|id| is_pci_id(id)) {
            return Err(
                "an NVIDIA display device entry is not a `0x<vendor>:0x<device>` PCI id"
                    .to_string(),
            );
        }
        Ok(record)
    }

    /// The record for a live metadata answer, or `None` when these sources
    /// carry no live answer naming a machine type.
    ///
    /// Only a live answer is ever recorded. A record built from sources that
    /// were themselves resolved from a record would launder a stale machine
    /// type into a fresh one.
    ///
    /// # Errors
    ///
    /// Names the fact that is unavailable when there is a live answer but
    /// the record could not be bound to what it was answered on.
    pub fn for_live_sources(sources: &HostSources) -> Result<Option<Self>, &'static str> {
        let Some(machine_type) = sources
            .gce_machine_type
            .as_deref()
            .and_then(machine_type_from_metadata)
        else {
            return Ok(None);
        };
        let facts = LocalShapeFacts::from_sources(sources)?;
        Ok(Some(Self {
            schema_version: MACHINE_TYPE_RECORD_SCHEMA_VERSION,
            boot_id: facts.boot_id,
            machine_type,
            logical_cpus: facts.logical_cpus,
            mem_total_bytes: facts.mem_total_bytes,
            nvidia_display_devices: facts.nvidia_display_devices,
        }))
    }

    /// The exact bytes the record file holds.
    ///
    /// # Errors
    ///
    /// Only if serialization itself fails.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self).map(|json| json + "\n")
    }

    /// The first bound fact that differs from `live`, or `None` when all match.
    /// Hardware differences include both values; boot UUIDs are never echoed.
    #[must_use]
    pub fn first_difference(&self, live: &LocalShapeFacts) -> Option<String> {
        if self.boot_id != live.boot_id {
            return Some("the kernel boot ID changed; the record is valid only within the boot in which it was written".to_string());
        }
        if self.logical_cpus != live.logical_cpus {
            return Some(format!(
                "logical CPU count was {} when recorded and is {} now",
                self.logical_cpus, live.logical_cpus
            ));
        }
        if self.mem_total_bytes != live.mem_total_bytes {
            return Some(format!(
                "MemTotal was {} bytes when recorded and is {} bytes now",
                self.mem_total_bytes, live.mem_total_bytes
            ));
        }
        if self.nvidia_display_devices != live.nvidia_display_devices {
            return Some(format!(
                "NVIDIA display devices were [{}] when recorded and are [{}] now",
                self.nvidia_display_devices.join(", "),
                live.nvidia_display_devices.join(", ")
            ));
        }
        None
    }
}

/// A lowercase UUID as Linux exposes it, excluding the all-zero sentinel.
pub(crate) fn is_boot_id(id: &str) -> bool {
    id.len() == 36
        && id != "00000000-0000-0000-0000-000000000000"
        && id.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
}

/// Whether `id` is `0x<vendor>:0x<device>` with four lowercase hex digits
/// each, as [`nvidia_display_devices`] spells a sysfs PCI id.
fn is_pci_id(id: &str) -> bool {
    let hex4 = |part: &str| {
        part.len() == 4
            && part
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    id.strip_prefix("0x")
        .and_then(|rest| rest.split_once(":0x"))
        .is_some_and(|(vendor, device)| hex4(vendor) && hex4(device))
}

/// The machine type these sources establish, and where it came from.
///
/// In order:
///
/// 1. A live metadata answer wins over any record, which is ignored. An
///    answer that is not a machine-type resource name is uninterpretable,
///    not absent. An instance binding naming another instance than the live
///    instance-id answer, or this instance on another machine type than the
///    live answer, fails detection (see [`crate::instance_binding`]).
/// 2. A host whose firmware does not say it is a Compute Engine instance has
///    no machine type, and any record is ignored.
/// 3. A Compute Engine instance without a live answer uses the recorded
///    machine type only if the record parses, every fact it is bound to
///    matches exactly, an instance binding written in this boot agrees
///    with it, and a binding from an earlier boot names its machine type.
///
/// # Errors
///
/// [`PlatformProbeError::Unrecognized`] for a live answer that is not a
/// machine-type resource name or instance id,
/// [`PlatformProbeError::InstanceChanged`] for a binding from another
/// instance, [`PlatformProbeError::MachineTypeChanged`] for a binding that
/// names another machine type for this instance, and
/// [`PlatformProbeError::IdentityUnestablished`] for every
/// other Compute Engine case without one. Never `Ok(None)` on Compute
/// Engine: an instance reporting no machine type is admitted as an
/// unvalidated shape rather than refused.
pub fn establish_machine_type(
    sources: &HostSources,
) -> Result<Option<(String, MachineTypeSource)>, PlatformProbeError> {
    if let Some(body) = sources.gce_machine_type.as_deref() {
        // The body is not echoed: it carries the project number, and it is
        // whatever the peer sent.
        let machine_type =
            machine_type_from_metadata(body).ok_or_else(|| PlatformProbeError::Unrecognized {
                source_name: GCE_METADATA_SOURCE_NAME.to_string(),
                detail: "the machine-type answer is not \
                         `projects/<project>/machineTypes/<machine-type>` with a canonical \
                         machine type"
                    .to_string(),
            })?;
        check_live_instance(sources, &machine_type)?;
        return Ok(Some((machine_type, MachineTypeSource::GceMetadata)));
    }
    if !sources
        .dmi_product_name
        .as_deref()
        .is_some_and(is_compute_engine)
    {
        return Ok(None);
    }

    let context = context(sources);
    let Some(body) = sources.machine_type_record.as_deref() else {
        return Err(PlatformProbeError::IdentityUnestablished {
            source_name: MACHINE_TYPE_RECORD_PATH.to_string(),
            detail: format!(
                "{context}, and no machine type has been recorded on this host; \
                 start tensorplate-agent once while the metadata service is reachable"
            ),
        });
    };
    let unestablished = |detail: String| PlatformProbeError::IdentityUnestablished {
        source_name: MACHINE_TYPE_RECORD_PATH.to_string(),
        detail,
    };
    let record = MachineTypeRecord::parse(body).map_err(|reason| {
        unestablished(format!(
            "{context}, and the recorded machine type is unusable: {reason}; \
             start tensorplate-agent once while the metadata service is reachable to record it again"
        ))
    })?;
    let live = LocalShapeFacts::from_sources(sources).map_err(|fact| {
        unestablished(format!(
            "{context}, and the recorded machine type `{}` cannot be checked against this host \
             because {fact} is unavailable",
            record.machine_type
        ))
    })?;
    if let Some(difference) = record.first_difference(&live) {
        return Err(unestablished(format!(
            "{context}, and the recorded machine type `{}` no longer describes this host: \
             {difference}; start tensorplate-agent once while the metadata service is reachable \
             to record it again (a stopped instance can be given a different machine type; if \
             it was, that start refuses and says how to reprovision)",
            record.machine_type
        )));
    }
    if let Some(binding) = sources.instance_binding.as_deref() {
        let unbound = |detail: String| PlatformProbeError::IdentityUnestablished {
            source_name: INSTANCE_BINDING_PATH.to_string(),
            detail: format!(
                "{context}, and {detail}; start tensorplate-agent once while the metadata \
                 service is reachable to record both again"
            ),
        };
        let binding = InstanceBinding::parse(binding)
            .map_err(|reason| unbound(format!("the instance binding is unusable: {reason}")))?;
        if binding.boot_id == live.boot_id {
            if let Some(disagreement) = binding.disagreement(&record, body) {
                return Err(unbound(disagreement));
            }
        } else if binding.machine_type != record.machine_type {
            // The record is from this boot and the binding from an earlier
            // one: only a record written without this release -- by the
            // 0.2.1 agent after a rollback -- can name another machine type.
            // Offline there is no instance id to tell a resize from a moved
            // disk; the online start that follows refuses either way, with
            // MachineTypeChanged or InstanceChanged, and both reprovision
            // alike. That start needs the service, so the message opens with
            // why it gave none now, as every other refusal here does.
            return Err(machine_type_changed(&format!(
                "{context}, and the machine type recorded in this boot is `{}`, and this host's \
                 identity was recorded on `{}`; the instance was given a different machine type, \
                 or the disk was moved to an instance of that type",
                record.machine_type, binding.machine_type
            )));
        }
    }
    Ok(Some((
        record.machine_type,
        MachineTypeSource::RecordedFromMetadata,
    )))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_logged_tokens_are_stable() {
        // The follow-up offline harness greps the agent journal for these.
        assert_eq!(MachineTypeSource::GceMetadata.as_str(), "gce_metadata");
        assert_eq!(
            MachineTypeSource::RecordedFromMetadata.as_str(),
            "recorded_gce_metadata"
        );
        assert_eq!(RecordWrite::Written.to_string(), "written");
        assert_eq!(RecordWrite::Unchanged.to_string(), "unchanged");
        assert_eq!(RecordWrite::NotApplicable.to_string(), "not_applicable");
        assert_eq!(
            RecordWrite::FactsUnavailable("MemTotal").to_string(),
            "not_recorded (unavailable: MemTotal)"
        );
    }

    #[test]
    fn a_record_serializes_deterministically() {
        let record = MachineTypeRecord {
            schema_version: 2,
            boot_id: "12345678-1234-4234-8234-123456789abc".to_string(),
            machine_type: "g2-standard-8".to_string(),
            logical_cpus: 8,
            mem_total_bytes: 33_649_020_928,
            nvidia_display_devices: vec!["0x10de:0x27b8".to_string()],
        };
        let json = record.to_json().expect("serializes");
        assert_eq!(
            json,
            "{\n  \"schema_version\": 2,\n  \"boot_id\": \"12345678-1234-4234-8234-123456789abc\",\n  \"machine_type\": \"g2-standard-8\",\n  \
             \"logical_cpus\": 8,\n  \"mem_total_bytes\": 33649020928,\n  \
             \"nvidia_display_devices\": [\n    \"0x10de:0x27b8\"\n  ]\n}\n"
        );
        assert_eq!(MachineTypeRecord::parse(&json).expect("parses"), record);
    }
}
