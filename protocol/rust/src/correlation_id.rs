// SPDX-License-Identifier: Apache-2.0
//
// V01-E12-F02: Correlation identifier value object and policy.
//
// `CorrelationId` carries request, deploy-transaction, and inference
// correlation across the agent, serving worker, scheduler, session,
// adapter, sidecar IPC, observability service, and CLI. The wire format
// is `[A-Za-z0-9_-]{1,64}` so consumers can safely embed the value in
// log lines, metric exemplars, and JSON envelopes without escaping or
// unbounded label cardinality.
//
// The policy distinguishes three identifier kinds:
//
//   - **Request ID** — produced by serving ingress per inference call.
//   - **Transaction ID** — produced by the agent per deploy transaction.
//   - **Correlation ID** — operator-supplied or upstream-supplied join
//     key used to thread logs, metrics, and errors across processes.
//
// All three use the same lexical policy so consumers can validate them
// with a single helper.

use std::fmt;
use std::str::FromStr;

use serde::de::{Deserialize, Deserializer, Error as DeError};
use serde::ser::{Serialize, Serializer};

use crate::DecodeError;

/// Maximum bytes for an external correlation id.
pub const MAX_CORRELATION_ID_BYTES: usize = 64;
/// Minimum bytes for an external correlation id.
pub const MIN_CORRELATION_ID_BYTES: usize = 1;
/// Length of a generated correlation id (hex digits over 64 bits of
/// entropy plus a `tp_` prefix yields 19 bytes).
pub const GENERATED_CORRELATION_ID_LEN: usize = 19;

/// Bounded correlation identifier. Implementations of `From<u64>` are
/// not provided to avoid accidentally treating an inference sequence
/// number as a correlation id.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CorrelationId(String);

impl CorrelationId {
    /// Generate a process-local correlation id from monotonic counter
    /// bytes. The caller supplies the seed so the producer can swap in a
    /// monotonic counter or a 64-bit `Instant` delta for tests. The
    /// generated id is `tp_<16 hex digits>` (19 bytes), matching the
    /// `[A-Za-z0-9_-]{1,64}` policy by construction.
    #[must_use]
    pub fn from_seed(seed: u64) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut buf = String::with_capacity(GENERATED_CORRELATION_ID_LEN);
        buf.push_str("tp_");
        for shift in (0..16).rev() {
            let nibble = ((seed >> (shift * 4)) & 0xF) as usize;
            buf.push(HEX[nibble] as char);
        }
        Self(buf)
    }

    /// Parse an externally supplied correlation id.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError::InvalidPayload`] if the id is empty,
    /// exceeds [`MAX_CORRELATION_ID_BYTES`], or contains characters
    /// outside the `[A-Za-z0-9_-]` set.
    pub fn parse(value: impl Into<String>) -> Result<Self, DecodeError> {
        let value = value.into();
        validate_correlation_id(&value)?;
        Ok(Self(value))
    }

    /// Borrowed view of the identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the inner string.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Display for CorrelationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for CorrelationId {
    type Err = DecodeError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s.to_string())
    }
}

impl Serialize for CorrelationId {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for CorrelationId {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Self::parse(s).map_err(|e| match e {
            DecodeError::InvalidPayload(msg) => D::Error::custom(msg),
            other => D::Error::custom(other.to_string()),
        })
    }
}

/// Validate an external correlation id without allocating a new
/// [`CorrelationId`].
///
/// # Errors
///
/// See [`CorrelationId::parse`].
pub fn validate_correlation_id(value: &str) -> Result<(), DecodeError> {
    let len = value.len();
    if !(MIN_CORRELATION_ID_BYTES..=MAX_CORRELATION_ID_BYTES).contains(&len) {
        return Err(DecodeError::InvalidPayload(format!(
            "correlation id must be {MIN_CORRELATION_ID_BYTES}..={MAX_CORRELATION_ID_BYTES} bytes"
        )));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(DecodeError::InvalidPayload(
            "correlation id must match [A-Za-z0-9_-]+".into(),
        ));
    }
    Ok(())
}

/// Sanitise an externally supplied correlation id by either accepting
/// it or generating a fresh one with [`CorrelationId::from_seed`].
///
/// Producers use this when they have to accept arbitrary upstream
/// values (HTTP headers, sidecar IPC payloads) and want to keep their
/// metric and log label cardinality bounded. Invalid values are
/// replaced with a generated id derived from the supplied `seed`.
#[must_use]
pub fn sanitise_or_generate(supplied: Option<&str>, seed: u64) -> CorrelationId {
    if let Some(value) = supplied {
        if validate_correlation_id(value).is_ok() {
            return CorrelationId(value.to_string());
        }
    }
    CorrelationId::from_seed(seed)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{
        sanitise_or_generate, validate_correlation_id, CorrelationId, GENERATED_CORRELATION_ID_LEN,
        MAX_CORRELATION_ID_BYTES,
    };
    use crate::DecodeError;

    #[test]
    fn generated_id_is_bounded_and_in_policy() {
        let id = CorrelationId::from_seed(0x1234_5678_9abc_def0);
        assert!(id.as_str().starts_with("tp_"));
        assert_eq!(id.as_str().len(), GENERATED_CORRELATION_ID_LEN);
        validate_correlation_id(id.as_str()).expect("policy");
    }

    #[test]
    fn parse_accepts_valid_external_id() {
        let id = CorrelationId::parse("req-1234_AbZ").expect("valid");
        assert_eq!(id.as_str(), "req-1234_AbZ");
    }

    #[test]
    fn parse_rejects_empty() {
        let err = CorrelationId::parse(String::new()).expect_err("empty");
        assert!(matches!(err, DecodeError::InvalidPayload(_)));
    }

    #[test]
    fn parse_rejects_overlong() {
        let s = "a".repeat(MAX_CORRELATION_ID_BYTES + 1);
        let err = CorrelationId::parse(s).expect_err("overlong");
        assert!(matches!(err, DecodeError::InvalidPayload(_)));
    }

    #[test]
    fn parse_rejects_disallowed_chars() {
        for bad in [
            "bad id",
            "with/slash",
            "with.dot",
            "with:colon",
            "with*star",
        ] {
            let err = CorrelationId::parse(bad.to_string()).expect_err("rejected");
            assert!(matches!(err, DecodeError::InvalidPayload(_)));
        }
    }

    #[test]
    fn sanitise_or_generate_keeps_valid_input() {
        let id = sanitise_or_generate(Some("upstream-1"), 0);
        assert_eq!(id.as_str(), "upstream-1");
    }

    #[test]
    fn sanitise_or_generate_replaces_invalid_input() {
        let id = sanitise_or_generate(Some("invalid id"), 0x1);
        assert_ne!(id.as_str(), "invalid id");
        validate_correlation_id(id.as_str()).expect("generated must be in policy");
    }

    #[test]
    fn json_round_trip_preserves_value() {
        let id = CorrelationId::parse("deploy-42").expect("valid");
        let json = serde_json::to_string(&id).expect("ser");
        assert_eq!(json, "\"deploy-42\"");
        let back: CorrelationId = serde_json::from_str(&json).expect("de");
        assert_eq!(back, id);
    }

    #[test]
    fn json_decode_rejects_out_of_policy_value() {
        let err = serde_json::from_str::<CorrelationId>("\"bad id\"").expect_err("rejected");
        assert!(err.to_string().contains("correlation id"));
    }
}
