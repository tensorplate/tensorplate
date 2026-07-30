// SPDX-License-Identifier: Apache-2.0
//
// V01-E08 typed agent error taxonomy.
//
// Every fallible operation in the agent returns `AgentResult<T>`. Errors
// are mapped to the cross-process [`tensorplate_protocol::ErrorCode`]
// before they cross the local control API boundary so the CLI sees the
// same stable codes as the C++ runtime.

use std::path::PathBuf;

use tensorplate_protocol::agent_state::ErrorRecord;
use tensorplate_protocol::ErrorCode;

/// Result alias used throughout the agent crate.
pub type AgentResult<T> = Result<T, AgentError>;

/// Typed error variants raised by agent components. Each variant carries
/// enough context to be useful in a log line; the [`AgentError::to_record`]
/// adapter projects them onto the wire-format [`ErrorRecord`] consumed by
/// the durable state store and the local control API.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("agent config is invalid: {0}")]
    Config(String),

    #[error("bundle manifest is invalid: {0}")]
    BundleManifest(String),

    #[error("bundle artifact `{path}` failed integrity check: {reason}")]
    BundleIntegrity { path: String, reason: String },

    #[error("bundle path `{0}` does not exist or is not a directory")]
    BundleMissing(PathBuf),

    #[error("bundle declares unsupported runtime version range: {0}")]
    UnsupportedRuntimeVersion(String),

    #[error("bundle declares unsupported target hardware: {0}")]
    UnsupportedHardware(String),

    #[error("bundle declares unavailable backend `{0}`")]
    UnsupportedBackend(String),

    /// packaging: the backend exists in `available_backends` but the
    /// packaging prober reported a typed reason it cannot run on this
    /// device. Carries the backend name plus a short reason for log
    /// output; the CLI doctor surfaces the full structured detail.
    #[error("backend `{backend}` is unrunnable on this device: {reason}")]
    BackendUnrunnable { backend: String, reason: String },

    /// The machine itself cannot honour a deploy: it matches no support
    /// row, matches one that carries no claim, is partitioned, or is
    /// missing a driver/runtime component or backend package the matched
    /// row requires. `reason` is the frozen platform reason where one
    /// applies — a machine-shape miss deliberately has none, because the
    /// vocabulary has no value for it and the nearest ones all name a
    /// dimension that is fine.
    #[error("platform cannot admit this deploy: {detail}")]
    PlatformNotAdmissible {
        reason: Option<&'static str>,
        detail: String,
    },

    #[error("bundle requires capability `{0}` not published by backend `{1}`")]
    UnsupportedCapability(String, String),

    #[error("bundle memory estimate exceeds configured device capacity")]
    InsufficientCapacity,

    #[error("invalid transaction transition: {0}")]
    InvalidTransition(String),

    #[error("a concurrent transaction is already in flight (id={0})")]
    Busy(String),

    #[error("requested operation is unavailable: {0}")]
    Unavailable(String),

    #[error("durable state file is corrupt or malformed: {0}")]
    CorruptState(String),

    #[error("worker control failed: {0}")]
    WorkerControl(String),

    #[error("worker prepare/warm/promote timed out after {0} ms")]
    WorkerTimeout(u64),

    #[error("worker reported candidate is not ready")]
    WorkerNotReady,

    #[error("internal agent error: {0}")]
    Internal(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

impl AgentError {
    /// Project the error onto the durable / wire-format [`ErrorRecord`].
    /// `recoverable` reflects the v0.1.0 retry policy: transient I/O and
    /// timeout errors are recoverable; config / unsupported / corrupt
    /// errors require operator intervention.
    #[must_use]
    pub fn to_record(&self) -> ErrorRecord {
        let (code, recoverable) = match self {
            AgentError::Config(_)
            | AgentError::BundleManifest(_)
            | AgentError::BundleMissing(_)
            | AgentError::CorruptState(_)
            | AgentError::InvalidTransition(_) => (ErrorCode::ConfigInvalid, false),
            AgentError::BundleIntegrity { .. } => (ErrorCode::LoadFailed, false),
            AgentError::UnsupportedRuntimeVersion(_)
            | AgentError::UnsupportedHardware(_)
            | AgentError::UnsupportedBackend(_)
            | AgentError::BackendUnrunnable { .. }
            | AgentError::PlatformNotAdmissible { .. }
            | AgentError::UnsupportedCapability(_, _)
            | AgentError::Unavailable(_) => (ErrorCode::Unsupported, false),
            AgentError::InsufficientCapacity => (ErrorCode::OomError, true),
            AgentError::Busy(_) | AgentError::WorkerNotReady => (ErrorCode::NotReady, true),
            AgentError::WorkerControl(_) => (ErrorCode::InferenceFailed, true),
            AgentError::WorkerTimeout(_) => (ErrorCode::Timeout, true),
            AgentError::Internal(_) | AgentError::Io(_) | AgentError::Serialization(_) => {
                (ErrorCode::Internal, true)
            }
        };
        ErrorRecord::new(code, self.to_string()).recoverable(recoverable)
    }

    /// Stable [`ErrorCode`] for this error.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        self.to_record().code
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_used,
        clippy::default_trait_access
    )]
    use super::AgentError;
    use tensorplate_protocol::ErrorCode;

    #[test]
    fn config_error_maps_to_config_invalid() {
        let err = AgentError::Config("bad".into());
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(!err.to_record().recoverable);
    }

    #[test]
    fn busy_error_is_not_ready_and_recoverable() {
        let err = AgentError::Busy("tx-1".into());
        let r = err.to_record();
        assert_eq!(r.code, ErrorCode::NotReady);
        assert!(r.recoverable);
    }

    #[test]
    fn worker_timeout_is_typed_timeout() {
        let err = AgentError::WorkerTimeout(5_000);
        assert_eq!(err.code(), ErrorCode::Timeout);
    }

    #[test]
    fn unsupported_backend_is_unsupported_non_recoverable() {
        let err = AgentError::UnsupportedBackend("vitis_ai".into());
        let r = err.to_record();
        assert_eq!(r.code, ErrorCode::Unsupported);
        assert!(!r.recoverable);
    }
}
