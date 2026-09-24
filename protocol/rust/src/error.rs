// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F01: Rust mirror of `protocol/schemas/error.json` and the C++
// `tensorplate::Error` value object.

use serde::{Deserialize, Serialize};

use crate::{DecodeError, ValidatePayload, SCHEMA_VERSION};

/// Stable error codes shared with the C++ runtime and JSON Schema.
///
/// Wire format is the snake_case name; the numeric C++ enum value is **not**
/// part of the protocol. Order matches the C++ `Error::Code` enumeration in
/// `include/tensorplate/core/error.hpp`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Schema, config, or manifest validation failure.
    ConfigInvalid,
    /// Model artifact could not be loaded.
    LoadFailed,
    /// Session is not in a state that permits this operation.
    NotReady,
    /// Tensor shape does not match the model contract.
    ShapeMismatch,
    /// Operation, capability, or schema version is not supported.
    Unsupported,
    /// Out-of-memory during allocation or execution.
    OomError,
    /// Operation exceeded its deadline.
    Timeout,
    /// Inference execution failed for backend-specific reasons.
    InferenceFailed,
    /// Unexpected internal error; usually a bug.
    Internal,
    /// Operation was cancelled at the caller's request.
    Cancelled,
    /// A backend process or worker the operation needs is unavailable,
    /// was reset, or is shutting down.
    Unavailable,
    /// A bounded quota, credit, or capacity limit was exhausted.
    ResourceExhausted,
}

impl ErrorCode {
    /// Every code in declaration order, which is the C++ numeric order and
    /// the order of the `code` enum in `protocol/schemas/error.json`.
    pub const ALL: [Self; 12] = [
        Self::ConfigInvalid,
        Self::LoadFailed,
        Self::NotReady,
        Self::ShapeMismatch,
        Self::Unsupported,
        Self::OomError,
        Self::Timeout,
        Self::InferenceFailed,
        Self::Internal,
        Self::Cancelled,
        Self::Unavailable,
        Self::ResourceExhausted,
    ];

    /// Stable serialized name (snake_case). Matches the C++
    /// `tensorplate::to_string(Error::Code)`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ConfigInvalid => "config_invalid",
            Self::LoadFailed => "load_failed",
            Self::NotReady => "not_ready",
            Self::ShapeMismatch => "shape_mismatch",
            Self::Unsupported => "unsupported",
            Self::OomError => "oom_error",
            Self::Timeout => "timeout",
            Self::InferenceFailed => "inference_failed",
            Self::Internal => "internal",
            Self::Cancelled => "cancelled",
            Self::Unavailable => "unavailable",
            Self::ResourceExhausted => "resource_exhausted",
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Mirror of `tensorplate::Error` (C++) and `protocol/schemas/error.json`.
///
/// `schema_version` is fixed to [`SCHEMA_VERSION`] for v0.1.0; decoders
/// reject other values via [`crate::DecodeError::UnsupportedSchemaVersion`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProtocolError {
    pub schema_version: String,
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub context: Option<String>,
}

impl ProtocolError {
    /// Construct a v0.1 error payload with no extra context.
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            code,
            message: message.into(),
            context: None,
        }
    }

    /// Attach a context string (file path, request id, backend detail, ...).
    #[must_use]
    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }
}

impl ValidatePayload for ProtocolError {
    fn validate_payload(self) -> Result<Self, DecodeError> {
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{ErrorCode, ProtocolError, SCHEMA_VERSION};

    #[test]
    fn all_lists_every_code_once_in_declaration_order() {
        use serde::de::value::{Error as ValueError, U32Deserializer};
        use serde::Deserialize;

        // serde's derived Deserialize also accepts a variant index, so it
        // knows every variant: ALL must match it at each index and end where
        // the enum ends, or a code added to the enum alone is never checked.
        let by_index = |index: usize| {
            let index = u32::try_from(index).expect("index fits in u32");
            ErrorCode::deserialize(U32Deserializer::<ValueError>::new(index))
        };
        for (index, code) in ErrorCode::ALL.into_iter().enumerate() {
            assert_eq!(
                code as usize, index,
                "{code} is out of place in ErrorCode::ALL"
            );
            assert_eq!(by_index(index).expect("variant index"), code);
        }
        assert!(
            by_index(ErrorCode::ALL.len()).is_err(),
            "ErrorCode has a variant that ErrorCode::ALL does not list"
        );
    }

    #[test]
    fn appended_codes_keep_their_wire_names() {
        assert_eq!(ErrorCode::Cancelled.as_str(), "cancelled");
        assert_eq!(ErrorCode::Unavailable.as_str(), "unavailable");
        assert_eq!(ErrorCode::ResourceExhausted.as_str(), "resource_exhausted");
        assert!(serde_json::from_str::<ErrorCode>("\"canceled\"").is_err());
    }

    #[test]
    fn code_round_trip_via_json() {
        for code in ErrorCode::ALL {
            let json = serde_json::to_string(&code).expect("serialize");
            let back: ErrorCode = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(code, back, "round-trip mismatch for {code}");
            // The serialized form is a quoted snake_case string.
            assert_eq!(json, format!("\"{}\"", code.as_str()));
        }
    }

    #[test]
    fn payload_round_trip_preserves_fields() {
        let original = ProtocolError::new(ErrorCode::ShapeMismatch, "rank mismatch")
            .with_context("input=image_front rank=4 expected=3");
        let json = serde_json::to_string(&original).expect("serialize");
        let back: ProtocolError = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, back);
        assert_eq!(back.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn missing_context_field_decodes_as_none() {
        let json = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","code":"timeout","message":"deadline"}}"#
        );
        let p: ProtocolError = serde_json::from_str(&json).expect("decode");
        assert_eq!(p.context, None);
        assert_eq!(p.code, ErrorCode::Timeout);
    }
}
