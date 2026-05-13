// SPDX-License-Identifier: Apache-2.0
//
// V01-E03-F06: Rust mirror of `protocol/schemas/buffer_pressure_event.json`
// and the C++ `tensorplate::BufferPressureEvent` value.
//
// This struct is the *protocol* representation of a pressure transition.
// Production wiring lives above this layer (V01-E12 observability); the
// crate provides decode + validation so the agent and observability
// service can consume buffer-plane pressure events without depending on
// the C++ runtime headers.

use serde::{Deserialize, Serialize};

use crate::{DecodeError, ValidatePayload, SCHEMA_VERSION};

/// Coarse memory-pressure level. Mirrors the C++ `MemoryPressure` enum.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPressure {
    /// In-use bytes below the warning threshold.
    #[default]
    Normal,
    /// In-use bytes between the warning and critical thresholds.
    Warning,
    /// In-use bytes at or above the critical threshold.
    Critical,
}

impl MemoryPressure {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

/// Mirror of `tensorplate::BufferPressureEvent` for protocol payloads.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BufferPressureEvent {
    pub schema_version: String,
    pub pool_name: String,
    pub previous: MemoryPressure,
    pub current: MemoryPressure,
    pub capacity_bytes: u64,
    pub in_use_bytes: u64,
    pub active_count: u64,
    pub high_water_bytes: u64,
    pub allocation_failures: u64,
}

/// Errors raised by [`BufferPressureEvent::new`]. Mirrors the C++ value
/// constraints.
#[derive(Debug, thiserror::Error)]
pub enum BufferPressureEventError {
    #[error("BufferPressureEvent.pool_name must be non-empty")]
    EmptyPoolName,
    #[error(
        "BufferPressureEvent.in_use_bytes ({in_use}) cannot exceed capacity_bytes ({capacity})"
    )]
    InUseExceedsCapacity { in_use: u64, capacity: u64 },
    #[error(
        "BufferPressureEvent.high_water_bytes ({high}) cannot be less than in_use_bytes ({in_use})"
    )]
    HighWaterBelowInUse { high: u64, in_use: u64 },
}

impl BufferPressureEvent {
    /// Build a validated event.
    ///
    /// # Errors
    ///
    /// - [`BufferPressureEventError::EmptyPoolName`] if `pool_name` is empty.
    /// - [`BufferPressureEventError::InUseExceedsCapacity`] if `in_use_bytes`
    ///   is greater than `capacity_bytes` (manager invariant violated).
    /// - [`BufferPressureEventError::HighWaterBelowInUse`] if
    ///   `high_water_bytes` is less than `in_use_bytes`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool_name: impl Into<String>,
        previous: MemoryPressure,
        current: MemoryPressure,
        capacity_bytes: u64,
        in_use_bytes: u64,
        active_count: u64,
        high_water_bytes: u64,
        allocation_failures: u64,
    ) -> Result<Self, BufferPressureEventError> {
        let pool_name = pool_name.into();
        if pool_name.is_empty() {
            return Err(BufferPressureEventError::EmptyPoolName);
        }
        if in_use_bytes > capacity_bytes {
            return Err(BufferPressureEventError::InUseExceedsCapacity {
                in_use: in_use_bytes,
                capacity: capacity_bytes,
            });
        }
        if high_water_bytes < in_use_bytes {
            return Err(BufferPressureEventError::HighWaterBelowInUse {
                high: high_water_bytes,
                in_use: in_use_bytes,
            });
        }
        Ok(Self {
            schema_version: SCHEMA_VERSION.to_string(),
            pool_name,
            previous,
            current,
            capacity_bytes,
            in_use_bytes,
            active_count,
            high_water_bytes,
            allocation_failures,
        })
    }
}

impl ValidatePayload for BufferPressureEvent {
    fn validate_payload(self) -> Result<Self, DecodeError> {
        Self::new(
            self.pool_name,
            self.previous,
            self.current,
            self.capacity_bytes,
            self.in_use_bytes,
            self.active_count,
            self.high_water_bytes,
            self.allocation_failures,
        )
        .map_err(|err| DecodeError::InvalidPayload(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{BufferPressureEvent, BufferPressureEventError, MemoryPressure, SCHEMA_VERSION};
    use crate::decode_with_version_check;

    #[test]
    fn round_trip_preserves_fields() {
        let e = BufferPressureEvent::new(
            "default",
            MemoryPressure::Normal,
            MemoryPressure::Warning,
            1024,
            800,
            3,
            900,
            0,
        )
        .expect("valid");
        let json = serde_json::to_string(&e).expect("serialize");
        let back: BufferPressureEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(e, back);
        assert_eq!(back.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn rejects_empty_pool_name() {
        let r = BufferPressureEvent::new(
            "",
            MemoryPressure::Normal,
            MemoryPressure::Normal,
            16,
            0,
            0,
            0,
            0,
        );
        assert!(matches!(r, Err(BufferPressureEventError::EmptyPoolName)));
    }

    #[test]
    fn rejects_in_use_above_capacity() {
        let r = BufferPressureEvent::new(
            "p",
            MemoryPressure::Normal,
            MemoryPressure::Critical,
            16,
            32,
            1,
            32,
            0,
        );
        assert!(matches!(
            r,
            Err(BufferPressureEventError::InUseExceedsCapacity { .. })
        ));
    }

    #[test]
    fn rejects_high_water_below_in_use() {
        let r = BufferPressureEvent::new(
            "p",
            MemoryPressure::Normal,
            MemoryPressure::Warning,
            64,
            32,
            1,
            16,
            0,
        );
        assert!(matches!(
            r,
            Err(BufferPressureEventError::HighWaterBelowInUse { .. })
        ));
    }

    #[test]
    fn version_check_decoder_accepts_current_schema() {
        let json = format!(
            r#"{{
              "schema_version":"{SCHEMA_VERSION}",
              "pool_name":"default",
              "previous":"normal",
              "current":"warning",
              "capacity_bytes":1024,
              "in_use_bytes":800,
              "active_count":3,
              "high_water_bytes":900,
              "allocation_failures":0
            }}"#
        );
        let e: BufferPressureEvent = decode_with_version_check(&json).expect("decode");
        assert_eq!(e.pool_name, "default");
        assert_eq!(e.current, MemoryPressure::Warning);
    }

    #[test]
    fn version_check_decoder_rejects_invalid_payload() {
        let json = format!(
            r#"{{
              "schema_version":"{SCHEMA_VERSION}",
              "pool_name":"",
              "previous":"normal",
              "current":"normal",
              "capacity_bytes":1024,
              "in_use_bytes":0,
              "active_count":0,
              "high_water_bytes":0,
              "allocation_failures":0
            }}"#
        );
        let err = decode_with_version_check::<BufferPressureEvent>(&json).expect_err("rejected");
        assert!(matches!(err, crate::DecodeError::InvalidPayload(_)));
    }
}
