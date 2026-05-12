// SPDX-License-Identifier: Apache-2.0

//! `tensorplate-protocol` — shared Rust types for TensorPlate cross-component
//! schemas.
//!
//! V01-E01-F03 shipped the crate skeleton; V01-E01-F06 added the cross-process
//! version constants that mirror `include/tensorplate/version.hpp`. V01-E02
//! lands the first real protocol surface: the typed error taxonomy
//! ([`error`]) and the schema-version decode contract
//! ([`decode_with_version_check`]). Subsequent V01-E02 features add
//! [`model_spec`], [`tensor_view`], [`buffer_ref`], [`infer_request`],
//! [`infer_result`], and the V01-E02-F07 control-plane payloads
//! ([`desired_state`], [`worker_status`], [`health_event`],
//! [`deploy_transaction`], [`python_pytorch_ipc`]).
//!
//! ## Versioning
//!
//! [`PROTOCOL_VERSION`] / [`SCHEMA_VERSION`] are an independent semver track
//! from the crate's `CARGO_PKG_VERSION` (the runtime version). They are
//! hand-mirrored from CMake until the build system owns the constants
//! end-to-end; `version_consistency_test` in this crate plus the C++
//! `Version` tests guard against silent drift. See
//! `docs/architecture/versioning.md`.

#![forbid(unsafe_code)]

use serde::de::DeserializeOwned;

pub mod buffer_pressure_event;
pub mod buffer_ref;
pub mod deploy_transaction;
pub mod desired_state;
pub mod error;
pub mod health_event;
pub mod infer_request;
pub mod infer_result;
pub mod model_spec;
pub mod python_pytorch_ipc;
pub mod tensor_view;
pub mod worker_status;

pub use buffer_pressure_event::{BufferPressureEvent, BufferPressureEventError, MemoryPressure};
pub use buffer_ref::{BufferOwnership, BufferRef, BufferRefError, NULL_BUFFER_ID};
pub use deploy_transaction::{
    DeployFailure, DeployState, DeployTransaction, DeployTransactionError,
};
pub use desired_state::{DesiredState, DesiredStateError, Rollout, RolloutStrategy};
pub use error::{ErrorCode, ProtocolError};
pub use health_event::{ControlLoopMetrics, HealthEvent, HealthEventKind};
pub use infer_request::{InferRequest, InferRequestError, NamedInput, RequestMetadata};
pub use infer_result::{
    InferResult, InferResultError, InferResultStatus, InferenceTiming, NamedOutput,
};
pub use model_spec::{ModelClass, ModelSpec, PrecisionHint};
pub use python_pytorch_ipc::{
    IpcMessage, IpcMessageError, IpcMessageKind, IpcMetric, IpcStatus, IpcTensor,
};
pub use tensor_view::{DType, Layout, TensorView, TensorViewError};
pub use worker_status::{ComponentState, WorkerStatus, WorkerStatusError};

/// Cross-process protocol major version. Bumping this is a breaking change
/// to any schema under `protocol/schemas/`.
pub const PROTOCOL_VERSION_MAJOR: u32 = 0;

/// Cross-process protocol minor version. Additive schema changes.
pub const PROTOCOL_VERSION_MINOR: u32 = 1;

/// Cross-process protocol version string in `MAJOR.MINOR` form.
pub const PROTOCOL_VERSION: &str = "0.1";

/// Value embedded as the `schema_version` field of every v0.1 protocol
/// payload. Identical to [`PROTOCOL_VERSION`]; the alias documents intent.
pub const SCHEMA_VERSION: &str = PROTOCOL_VERSION;

/// Model bundle on-disk format major version. Bumping this changes how
/// `tensorplate-agent` lays out and verifies a deployed bundle.
pub const BUNDLE_FORMAT_VERSION_MAJOR: u32 = 0;

/// Model bundle on-disk format minor version.
pub const BUNDLE_FORMAT_VERSION_MINOR: u32 = 1;

/// Model bundle format version string in `MAJOR.MINOR` form.
pub const BUNDLE_FORMAT_VERSION: &str = "0.1";

/// Crate-level marker used by the v0.1.0 scaffolding tests. Retained for
/// backwards compatibility with the V01-E01 smoke test; new code should
/// not depend on it.
pub const SKELETON_MARKER: &str = "tensorplate-protocol-skeleton";

/// Returns the protocol crate version string compiled from Cargo metadata.
/// This corresponds to the runtime release version, **not** the protocol
/// version. Use [`PROTOCOL_VERSION`] for the cross-process protocol.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Errors returned by [`decode_with_version_check`].
///
/// Production decoders translate [`DecodeError::UnsupportedSchemaVersion`]
/// into a [`ProtocolError`] with [`ErrorCode::Unsupported`] before
/// surfacing to clients, satisfying the V01-E02 acceptance criterion that
/// "Unknown schema versions are rejected with typed errors."
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// JSON parse failure.
    #[error("malformed payload: {0}")]
    Malformed(#[from] serde_json::Error),

    /// Top-level `schema_version` field was missing or not a string.
    #[error("missing or non-string `schema_version` field")]
    MissingSchemaVersion,

    /// Top-level `schema_version` did not match [`SCHEMA_VERSION`].
    #[error("unsupported schema_version `{got}` (expected `{expected}`)")]
    UnsupportedSchemaVersion {
        /// The version string the decoder observed.
        got: String,
        /// The version string the build was compiled against.
        expected: &'static str,
    },

    /// Payload decoded at the current schema version but violated semantic
    /// validation that JSON field types cannot express.
    #[error("invalid payload: {0}")]
    InvalidPayload(String),
}

impl From<DecodeError> for ProtocolError {
    fn from(value: DecodeError) -> Self {
        match value {
            DecodeError::Malformed(e) => {
                ProtocolError::new(ErrorCode::ConfigInvalid, format!("malformed payload: {e}"))
            }
            DecodeError::MissingSchemaVersion => ProtocolError::new(
                ErrorCode::Unsupported,
                "missing or non-string `schema_version` field",
            ),
            DecodeError::UnsupportedSchemaVersion { got, expected } => ProtocolError::new(
                ErrorCode::Unsupported,
                format!("unsupported schema_version `{got}` (expected `{expected}`)"),
            ),
            DecodeError::InvalidPayload(message) => {
                ProtocolError::new(ErrorCode::ConfigInvalid, message)
            }
        }
    }
}

/// Semantic validation hook for protocol payloads decoded from JSON.
///
/// `serde` enforces type shape. Implementations here enforce the same
/// semantic rules as the public constructors, and may canonicalize fields
/// whose schema defaults need runtime computation.
pub trait ValidatePayload: Sized {
    /// Validate and return the decoded payload.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError::InvalidPayload`] when semantic validation fails.
    fn validate_payload(self) -> Result<Self, DecodeError>;
}

/// Decode a JSON payload of type `T` after verifying that its top-level
/// `schema_version` field matches [`SCHEMA_VERSION`].
///
/// This is the single entry point for "unknown schema versions are rejected
/// with typed errors" required by V01-E02. Decoders that bypass it lose
/// the guarantee.
///
/// # Errors
///
/// - [`DecodeError::Malformed`] if the bytes are not valid JSON or `T`'s
///   serde structural decode fails.
/// - [`DecodeError::MissingSchemaVersion`] if the top-level
///   `schema_version` field is missing or not a string.
/// - [`DecodeError::UnsupportedSchemaVersion`] if the version string
///   does not equal [`SCHEMA_VERSION`].
/// - [`DecodeError::InvalidPayload`] if semantic validation fails.
pub fn decode_with_version_check<T>(json: &str) -> Result<T, DecodeError>
where
    T: DeserializeOwned + ValidatePayload,
{
    let value: serde_json::Value = serde_json::from_str(json)?;
    let observed = value
        .get("schema_version")
        .and_then(serde_json::Value::as_str)
        .ok_or(DecodeError::MissingSchemaVersion)?;
    if observed != SCHEMA_VERSION {
        return Err(DecodeError::UnsupportedSchemaVersion {
            got: observed.to_string(),
            expected: SCHEMA_VERSION,
        });
    }
    let parsed: T = serde_json::from_value(value)?;
    parsed.validate_payload()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]

    use super::{
        decode_with_version_check, version, DecodeError, ErrorCode, ProtocolError,
        BUNDLE_FORMAT_VERSION, BUNDLE_FORMAT_VERSION_MAJOR, BUNDLE_FORMAT_VERSION_MINOR,
        PROTOCOL_VERSION, PROTOCOL_VERSION_MAJOR, PROTOCOL_VERSION_MINOR, SCHEMA_VERSION,
        SKELETON_MARKER,
    };

    #[test]
    fn marker_is_stable() {
        assert_eq!(SKELETON_MARKER, "tensorplate-protocol-skeleton");
    }

    #[test]
    fn version_is_non_empty() {
        assert!(!version().is_empty());
    }

    #[test]
    fn protocol_version_strings_match_components() {
        assert_eq!(
            PROTOCOL_VERSION,
            format!("{PROTOCOL_VERSION_MAJOR}.{PROTOCOL_VERSION_MINOR}")
        );
    }

    #[test]
    fn bundle_format_version_strings_match_components() {
        assert_eq!(
            BUNDLE_FORMAT_VERSION,
            format!("{BUNDLE_FORMAT_VERSION_MAJOR}.{BUNDLE_FORMAT_VERSION_MINOR}")
        );
    }

    #[test]
    fn schema_version_matches_protocol_version() {
        assert_eq!(SCHEMA_VERSION, PROTOCOL_VERSION);
    }

    #[test]
    fn decode_accepts_current_schema_version() {
        let json = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","code":"timeout","message":"deadline exceeded"}}"#
        );
        let p: ProtocolError = decode_with_version_check(&json).expect("decode");
        assert_eq!(p.code, ErrorCode::Timeout);
        assert_eq!(p.message, "deadline exceeded");
    }

    #[test]
    fn decode_rejects_unknown_schema_version() {
        let json = r#"{"schema_version":"99.99","code":"internal","message":"x"}"#;
        let err = decode_with_version_check::<ProtocolError>(json).expect_err("must reject");
        match err {
            DecodeError::UnsupportedSchemaVersion { got, expected } => {
                assert_eq!(got, "99.99");
                assert_eq!(expected, SCHEMA_VERSION);
            }
            other => panic!("expected UnsupportedSchemaVersion, got {other:?}"),
        }
    }

    #[test]
    fn decode_rejects_missing_schema_version() {
        let json = r#"{"code":"internal","message":"x"}"#;
        let err = decode_with_version_check::<ProtocolError>(json).expect_err("must reject");
        assert!(matches!(err, DecodeError::MissingSchemaVersion));
    }

    #[test]
    fn decode_error_maps_to_unsupported_protocol_error() {
        let err = DecodeError::UnsupportedSchemaVersion {
            got: "99.99".into(),
            expected: SCHEMA_VERSION,
        };
        let p: ProtocolError = err.into();
        assert_eq!(p.code, ErrorCode::Unsupported);
    }
}
