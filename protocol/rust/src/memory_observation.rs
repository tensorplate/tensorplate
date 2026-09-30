// SPDX-License-Identifier: Apache-2.0

//! Memory samples shared by platform collectors and their management-plane consumers.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::json_numbers::MAX_SAFE_BYTES;
use crate::serde_shape::{deserialize_map_only, deserialize_vec_map_only};
use crate::{BudgetDomainName, DecodeError, ValidatePayload, SCHEMA_VERSION};

/// Why a reading has no numeric value; none of these means zero usage.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    SourceUnavailable,
    MissingField,
    Unsupported,
}

/// A byte reading with availability attached to that specific quantity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "availability", rename_all = "snake_case", deny_unknown_fields)]
pub enum ByteObservation {
    Available {
        bytes: u64,
    },
    Unavailable {
        #[serde(deserialize_with = "string_enum")]
        reason: UnavailableReason,
    },
}

impl ByteObservation {
    pub const fn bytes(self) -> Option<u64> {
        match self {
            Self::Available { bytes } => Some(bytes),
            Self::Unavailable { .. } => None,
        }
    }

    pub const fn unavailable(reason: UnavailableReason) -> Self {
        Self::Unavailable { reason }
    }
}

/// Attribution comes from the caller's PID ownership map, never an executable name.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessRole {
    Agent,
    ServingWorker,
    PythonSidecar,
    External,
}

/// RSS or driver process usage is an aggregate, not an additive budget line.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessMemory {
    pub pid: u32,
    #[serde(deserialize_with = "string_enum")]
    pub role: ProcessRole,
    pub bytes: ByteObservation,
}

/// An observed empty process list differs from an unavailable process query.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "availability", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProcessAggregate {
    Available {
        #[serde(deserialize_with = "deserialize_vec_map_only")]
        processes: Vec<ProcessMemory>,
    },
    Unavailable {
        #[serde(deserialize_with = "string_enum")]
        reason: UnavailableReason,
    },
}

/// Physical domain totals. Driver reservations are not allocator reservations.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomainMemory {
    pub capacity: ByteObservation,
    pub available: ByteObservation,
    pub used: ByteObservation,
    pub driver_reserved: ByteObservation,
}

impl DomainMemory {
    /// Consumption includes driver/OS use; it is not the sum of process readings.
    pub fn consumed_bytes(self) -> Option<u64> {
        self.capacity.bytes()?.checked_sub(self.available.bytes()?)
    }
}

/// Identifies the actual source used for this sample, including a CSV fallback.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemorySource {
    Proc,
    NvidiaSmiXml,
    NvidiaSmiCsv,
}

/// Availability of capacity, free/available memory and process attribution.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationAvailability {
    Available,
    Partial,
    Unavailable,
}

/// Mirrors `memory_observation.json`; deserialization checks semantic invariants.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MemoryObservation {
    pub schema_version: String,
    pub domain: BudgetDomainName,
    pub source: MemorySource,
    pub availability: ObservationAvailability,
    pub allocated: ByteObservation,
    pub reserved: ByteObservation,
    pub process_aggregate: ProcessAggregate,
    pub device_aggregate: DomainMemory,
    pub sampled_monotonic_ns: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireObservation {
    schema_version: String,
    domain: BudgetDomainName,
    #[serde(deserialize_with = "string_enum")]
    source: MemorySource,
    #[serde(deserialize_with = "string_enum")]
    availability: ObservationAvailability,
    allocated: ByteObservation,
    reserved: ByteObservation,
    process_aggregate: ProcessAggregate,
    #[serde(deserialize_with = "deserialize_map_only")]
    device_aggregate: DomainMemory,
    sampled_monotonic_ns: u64,
}

impl<'de> Deserialize<'de> for MemoryObservation {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire: WireObservation = deserialize_map_only(deserializer)?;
        let value = Self {
            schema_version: wire.schema_version,
            domain: wire.domain,
            source: wire.source,
            availability: wire.availability,
            allocated: wire.allocated,
            reserved: wire.reserved,
            process_aggregate: wire.process_aggregate,
            device_aggregate: wire.device_aggregate,
            sampled_monotonic_ns: wire.sampled_monotonic_ns,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

impl MemoryObservation {
    /// Construct a source sample; allocator statistics require a separate source.
    pub fn new(
        domain: BudgetDomainName,
        source: MemorySource,
        device_aggregate: DomainMemory,
        process_aggregate: ProcessAggregate,
        sampled_monotonic_ns: u64,
    ) -> Result<Self, &'static str> {
        let mut value = Self {
            schema_version: SCHEMA_VERSION.into(),
            domain,
            source,
            availability: ObservationAvailability::Unavailable,
            allocated: ByteObservation::unavailable(UnavailableReason::Unsupported),
            reserved: ByteObservation::unavailable(UnavailableReason::Unsupported),
            process_aggregate,
            device_aggregate,
            sampled_monotonic_ns,
        };
        value.availability = value.observed_availability();
        value.validate()?;
        Ok(value)
    }

    fn observed_availability(&self) -> ObservationAvailability {
        let totals = self.device_aggregate;
        let processes_ready = matches!(&self.process_aggregate,
            ProcessAggregate::Available { processes } if processes.iter().all(|p| p.bytes.bytes().is_some()));
        if totals.capacity.bytes().is_some()
            && totals.available.bytes().is_some()
            && processes_ready
        {
            ObservationAvailability::Available
        } else if [
            totals.capacity,
            totals.available,
            totals.used,
            totals.driver_reserved,
        ]
        .iter()
        .any(|v| v.bytes().is_some())
            || matches!(&self.process_aggregate, ProcessAggregate::Available { .. })
        {
            ObservationAvailability::Partial
        } else {
            ObservationAvailability::Unavailable
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != SCHEMA_VERSION {
            return Err("unsupported memory observation schema_version");
        }
        if !matches!(
            (self.source, self.domain),
            (MemorySource::Proc, BudgetDomainName::GuestRam)
                | (
                    MemorySource::NvidiaSmiXml | MemorySource::NvidiaSmiCsv,
                    BudgetDomainName::DeviceVram
                )
        ) {
            return Err("memory observation source does not measure this domain");
        }
        if self.sampled_monotonic_ns > MAX_SAFE_BYTES {
            return Err("sample timestamp exceeds the exact JSON integer range");
        }
        let totals = self.device_aggregate;
        for reading in [
            self.allocated,
            self.reserved,
            totals.capacity,
            totals.available,
            totals.used,
            totals.driver_reserved,
        ] {
            check_bytes(reading)?;
        }
        if totals.capacity.bytes() == Some(0) {
            return Err("measured capacity must be positive");
        }
        if let Some(capacity) = totals.capacity.bytes() {
            for reading in [totals.available, totals.used, totals.driver_reserved] {
                if reading.bytes().is_some_and(|bytes| bytes > capacity) {
                    return Err("domain reading exceeds measured capacity");
                }
            }
        }
        if self.allocated.bytes().is_some() || self.reserved.bytes().is_some() {
            return Err("this source does not report allocator allocated or reserved bytes");
        }
        if let ProcessAggregate::Available { processes } = &self.process_aggregate {
            let mut pids = HashSet::new();
            for process in processes {
                if process.pid == 0 || !pids.insert(process.pid) {
                    return Err("process IDs must be positive and unique within a sample");
                }
                check_bytes(process.bytes)?;
            }
        }
        if self.availability != self.observed_availability() {
            return Err("sample availability disagrees with its readings");
        }
        Ok(())
    }
}

fn check_bytes(value: ByteObservation) -> Result<(), &'static str> {
    if value.bytes().is_some_and(|bytes| bytes > MAX_SAFE_BYTES) {
        return Err("memory reading exceeds the exact JSON integer range");
    }
    Ok(())
}

impl ValidatePayload for MemoryObservation {
    fn validate_payload(self) -> Result<Self, DecodeError> {
        self.validate()
            .map_err(|message| DecodeError::InvalidPayload(message.into()))?;
        Ok(self)
    }
}

fn string_enum<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<T, D::Error> {
    let value = String::deserialize(deserializer)?;
    T::deserialize(serde::de::value::StringDeserializer::<D::Error>::new(value))
}
