// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F05: Rust mirror of `protocol/schemas/buffer_ref.json` and the C++
// `tensorplate::BufferRef` value object.
//
// Note: This Rust struct is the *protocol* representation of a buffer handle,
// used in logs, traces, status reports, and test fixtures. It is NOT a
// memory-transfer wire format; v0.1.0 does not move buffer payloads through
// JSON. Real memory ownership is owned by the C++ buffer plane (V01-E03);
// the Rust agent only ever sees the metadata.

use serde::{Deserialize, Serialize};

use crate::{DecodeError, ValidatePayload, SCHEMA_VERSION};

/// Sentinel id reserved for the released / "no buffer" handle.
pub const NULL_BUFFER_ID: u64 = 0;

/// Ownership state recorded by a [`BufferRef`]. See the C++ header for the
/// authoritative responsibility matrix.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BufferOwnership {
    /// Holder must release exactly once.
    Owned,
    /// Holder may read; release is forbidden.
    Borrowed,
    /// Tombstone; not valid for I/O.
    #[default]
    Released,
}

impl BufferOwnership {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owned => "owned",
            Self::Borrowed => "borrowed",
            Self::Released => "released",
        }
    }
}

/// Mirror of `tensorplate::BufferRef` (C++) for protocol payloads.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BufferRef {
    pub schema_version: String,
    pub id: u64,
    pub size_bytes: u64,
    pub ownership: BufferOwnership,
}

/// Errors raised by [`BufferRef::new`]. Mirrors the C++ `BufferRef::create`
/// validation rules.
#[derive(Debug, thiserror::Error)]
pub enum BufferRefError {
    #[error("BufferRef.id == 0 is reserved for the released sentinel")]
    ReservedNullId,
    #[error("BufferRef.size_bytes must be > 0 for owned/borrowed handles")]
    ZeroSize,
}

impl BufferRef {
    /// Build a released sentinel handle. Equivalent to the
    /// default-constructed C++ `BufferRef`.
    #[must_use]
    pub fn null() -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            id: NULL_BUFFER_ID,
            size_bytes: 0,
            ownership: BufferOwnership::Released,
        }
    }

    /// Build a validated handle.
    ///
    /// # Errors
    ///
    /// - [`BufferRefError::ReservedNullId`] if `ownership != Released` and
    ///   `id == NULL_BUFFER_ID`.
    /// - [`BufferRefError::ZeroSize`] if `ownership != Released` and
    ///   `size_bytes == 0`.
    pub fn new(
        id: u64,
        size_bytes: u64,
        ownership: BufferOwnership,
    ) -> Result<Self, BufferRefError> {
        if ownership != BufferOwnership::Released {
            if id == NULL_BUFFER_ID {
                return Err(BufferRefError::ReservedNullId);
            }
            if size_bytes == 0 {
                return Err(BufferRefError::ZeroSize);
            }
        }
        Ok(Self {
            schema_version: SCHEMA_VERSION.to_string(),
            id,
            size_bytes,
            ownership,
        })
    }

    /// Tombstone a handle. Idempotent.
    pub fn mark_released(&mut self) {
        self.ownership = BufferOwnership::Released;
    }

    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.ownership != BufferOwnership::Released
    }
}

impl ValidatePayload for BufferRef {
    fn validate_payload(self) -> Result<Self, DecodeError> {
        Self::new(self.id, self.size_bytes, self.ownership)
            .map_err(|err| DecodeError::InvalidPayload(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{BufferOwnership, BufferRef, BufferRefError, NULL_BUFFER_ID, SCHEMA_VERSION};
    use crate::decode_with_version_check;

    #[test]
    fn round_trip_preserves_fields() {
        let h = BufferRef::new(7, 1024, BufferOwnership::Owned).expect("valid");
        let json = serde_json::to_string(&h).expect("serialize");
        let back: BufferRef = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(h, back);
        assert_eq!(back.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn null_sentinel_is_released() {
        let h = BufferRef::null();
        assert_eq!(h.id, NULL_BUFFER_ID);
        assert_eq!(h.size_bytes, 0);
        assert_eq!(h.ownership, BufferOwnership::Released);
        assert!(!h.is_valid());
    }

    #[test]
    fn rejects_null_id_for_active_handles() {
        assert!(matches!(
            BufferRef::new(NULL_BUFFER_ID, 16, BufferOwnership::Owned),
            Err(BufferRefError::ReservedNullId)
        ));
        assert!(matches!(
            BufferRef::new(NULL_BUFFER_ID, 16, BufferOwnership::Borrowed),
            Err(BufferRefError::ReservedNullId)
        ));
        // Released with id 0 is OK; that's the canonical sentinel.
        assert!(BufferRef::new(NULL_BUFFER_ID, 0, BufferOwnership::Released).is_ok());
    }

    #[test]
    fn rejects_zero_size_for_active_handles() {
        assert!(matches!(
            BufferRef::new(1, 0, BufferOwnership::Owned),
            Err(BufferRefError::ZeroSize)
        ));
        assert!(matches!(
            BufferRef::new(1, 0, BufferOwnership::Borrowed),
            Err(BufferRefError::ZeroSize)
        ));
    }

    #[test]
    fn mark_released_is_idempotent() {
        let mut h = BufferRef::new(42, 8, BufferOwnership::Owned).expect("valid");
        assert!(h.is_valid());
        h.mark_released();
        assert!(!h.is_valid());
        // Calling again is a no-op (ownership stays Released).
        h.mark_released();
        assert_eq!(h.ownership, BufferOwnership::Released);
    }

    #[test]
    fn version_check_decoder_accepts_current_schema() {
        let json = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","id":1,"size_bytes":16,"ownership":"borrowed"}}"#
        );
        let h: BufferRef = decode_with_version_check(&json).expect("decode");
        assert_eq!(h.id, 1);
        assert_eq!(h.ownership, BufferOwnership::Borrowed);
    }

    #[test]
    fn version_check_decoder_rejects_current_schema_invalid_handle() {
        let json = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","id":0,"size_bytes":16,"ownership":"owned"}}"#
        );
        let err = decode_with_version_check::<BufferRef>(&json).expect_err("rejected");
        assert!(matches!(err, crate::DecodeError::InvalidPayload(_)));
    }
}
