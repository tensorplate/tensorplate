// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F01: Rust mirror of `protocol/schemas/error.json` and the C++
// `tensorplate::Error` value object.

use serde::{Deserialize, Serialize};

use crate::SCHEMA_VERSION;

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
}

impl ErrorCode {
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{ErrorCode, ProtocolError, SCHEMA_VERSION};

    #[test]
    fn code_round_trip_via_json() {
        for code in [
            ErrorCode::ConfigInvalid,
            ErrorCode::LoadFailed,
            ErrorCode::NotReady,
            ErrorCode::ShapeMismatch,
            ErrorCode::Unsupported,
            ErrorCode::OomError,
            ErrorCode::Timeout,
            ErrorCode::InferenceFailed,
            ErrorCode::Internal,
        ] {
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
