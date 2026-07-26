// SPDX-License-Identifier: Apache-2.0
//
// Typed failures for platform registry parsing. These are static-config
// validation failures, so they map to `ErrorCode::ConfigInvalid` rather
// than the protocol-level `Unsupported`.

use tensorplate_protocol::{ErrorCode, ProtocolError};

/// Why a platform support row or roadmap target failed to load.
#[derive(Debug, thiserror::Error)]
pub enum PlatformRegistryError {
    /// The document is not valid JSON or violates the schema shape
    /// (unknown fields, missing required fields, wrong types or forms).
    #[error("malformed platform registry document: {0}")]
    Malformed(#[from] serde_json::Error),

    /// Top-level `schema_version` is missing or not a string.
    #[error("platform registry document is missing `schema_version`")]
    MissingSchemaVersion,

    /// `schema_version` does not match the expected version for the
    /// document kind.
    #[error("unsupported platform registry schema_version `{got}` (expected `{expected}`)")]
    UnsupportedSchemaVersion { got: String, expected: &'static str },

    /// A byte-valued token is outside the safe integer domain. Checked on
    /// the exact decimal lexeme before any float parsing.
    #[error("invalid byte value `{token}`: {reason}")]
    InvalidNumberLexeme { token: String, reason: &'static str },

    /// The row is well-formed but violates a row invariant.
    #[error("invalid platform support row: {reason}")]
    InvalidRow { reason: &'static str },

    /// The roadmap target is well-formed but violates a target invariant.
    #[error("invalid roadmap target: {reason}")]
    InvalidRoadmapTarget { reason: &'static str },

    /// A registry document could not be read.
    #[error("cannot read platform registry path `{path}`: {detail}")]
    Unreadable { path: String, detail: String },

    /// A registry document is invalid, named by its source path so the
    /// operator knows which file to fix.
    #[error("invalid platform registry document `{path}`: {source}")]
    InDocument {
        path: String,
        #[source]
        source: Box<PlatformRegistryError>,
    },

    /// Two registry entries collide, so a lookup could not be answered
    /// deterministically. Rejected at load rather than resolved by
    /// picking a winner at query time.
    #[error("ambiguous platform registry: {detail}")]
    AmbiguousRegistry { detail: String },
}

impl PlatformRegistryError {
    /// Attach the source path of the document that failed.
    #[must_use]
    pub fn in_document(path: &std::path::Path, source: Self) -> Self {
        Self::InDocument {
            path: path.display().to_string(),
            source: Box::new(source),
        }
    }
}

impl From<PlatformRegistryError> for ProtocolError {
    fn from(value: PlatformRegistryError) -> Self {
        ProtocolError::new(
            ErrorCode::ConfigInvalid,
            "invalid platform registry document",
        )
        .with_context(value.to_string())
    }
}
