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

/// The `source_name` of every detection error about what the metadata
/// service answered, for `tensorplate doctor` to choose its hint by.
pub const GCE_METADATA_SOURCE_NAME: &str = "GCE metadata service";

/// What a metadata answer that is neither a result nor a documented
/// transient status says about the host, for every error that reports one:
/// another status, a response that cannot be parsed, or a 200 whose body is
/// not a machine type or an instance id.
/// The 403 causes are Google's, from its metadata server troubleshooting
/// page; that page also says a proxy can intercept the VM's queries.
pub(crate) const BROKEN_METADATA_ANSWER: &str =
    "the answer came from the metadata server itself, which Google \
    documents answering 403 for an endpoint disabled by project or instance settings or a \
    request that fails its security checks, or from something answering in its place for \
    169.254.169.254:80, such as a proxy or a custom route; check those settings and let this \
    host reach the metadata server directly, then start tensorplate-agent again";

/// Why platform detection failed.
///
/// Kept separate from [`PlatformRegistryError`] because the two are
/// different kinds of failure with different operator responses: a bad
/// registry document is invalid static config, while an unreadable
/// hardware source is a runtime detection failure. Collapsing them would
/// report a failed accelerator query as a config error.
#[derive(Debug, thiserror::Error)]
pub enum PlatformProbeError {
    /// A detection source could not be read.
    #[error("cannot read platform detection source `{source_name}`: {detail}")]
    Unreadable { source_name: String, detail: String },

    /// A source was readable but what it reported could not be
    /// interpreted at all — malformed, truncated, or structurally
    /// unexpected.
    ///
    /// This is **not** for a value that is merely off-matrix: a readable
    /// architecture or vendor no row names is returned as
    /// `DetectedArchitecture::Other` / `DetectedVendor::Other`, so the
    /// registry can report it as unsupported rather than undetectable.
    #[error("uninterpretable platform detail from `{source_name}`: {detail}")]
    Unrecognized { source_name: String, detail: String },

    /// Every source was readable, but together they do not establish an
    /// identity a row can be matched against, and reporting a partial one
    /// would be admitted as something it is not.
    ///
    /// Today this is a Compute Engine instance whose metadata service gave no
    /// answer and whose recorded machine type is missing, unusable,
    /// bound to local facts that have changed, or contradicted by the
    /// instance binding written in the same boot. Reporting that instance
    /// with no machine type would admit it as an unvalidated shape.
    #[error("platform identity could not be established from `{source_name}`: {detail}")]
    IdentityUnestablished { source_name: String, detail: String },

    /// The metadata service answers for a different Compute Engine instance
    /// from the one this host's identity was recorded on: the disk was moved
    /// to, or cloned into, another instance.
    ///
    /// Another attempt cannot settle it, and a later answer does not heal
    /// it. Reprovisioning is explicit: delete the instance binding and the
    /// machine-type record, then start `tensorplate-agent` while the
    /// metadata service is reachable.
    #[error("platform identity in `{source_name}` belongs to another instance: {detail}")]
    InstanceChanged { source_name: String, detail: String },

    /// The instance this host's identity was recorded on now has another
    /// machine type than the instance binding records: it was stopped and
    /// given a different machine type. Online, the live answer names it;
    /// offline, the machine-type record written in this boot does, against
    /// a binding from an earlier boot, and a moved disk cannot be told
    /// apart from a resize there.
    ///
    /// Another attempt cannot settle it, and a later answer does not heal
    /// it. Reprovisioning is explicit, as for [`Self::InstanceChanged`]:
    /// delete the instance binding and the machine-type record, then start
    /// `tensorplate-agent` while the metadata service is reachable.
    #[error("platform identity in `{source_name}` was recorded on another machine type: {detail}")]
    MachineTypeChanged { source_name: String, detail: String },
}

impl From<PlatformProbeError> for ProtocolError {
    fn from(value: PlatformProbeError) -> Self {
        let code = match value {
            // Readable, but not something this release can interpret.
            PlatformProbeError::Unrecognized { .. } => ErrorCode::Unsupported,
            // The runtime could not determine what it is running on, either
            // because a source was unreadable or because the readable ones do
            // not establish an identity without guessing.
            PlatformProbeError::Unreadable { .. }
            | PlatformProbeError::IdentityUnestablished { .. }
            | PlatformProbeError::InstanceChanged { .. }
            | PlatformProbeError::MachineTypeChanged { .. } => ErrorCode::Internal,
        };
        ProtocolError::new(code, "platform detection failed").with_context(value.to_string())
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
