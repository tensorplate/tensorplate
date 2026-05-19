// SPDX-License-Identifier: Apache-2.0
//
// V01-E10 typed observability error taxonomy.
//
// The observability service runs independently from the agent and the
// serving worker. Errors surface to the operator through structured logs
// at startup and through bounded diagnostic counters at runtime; this
// taxonomy is the single source of truth for both.

use tensorplate_protocol::ErrorCode;

/// Result alias used throughout the observability crate.
pub type ObservabilityResult<T> = Result<T, ObservabilityError>;

/// Typed observability errors. Each variant maps to a stable
/// [`ErrorCode`] so the V01-E11 CLI sees the same codes as the rest of
/// the runtime.
#[derive(Debug, thiserror::Error)]
pub enum ObservabilityError {
    #[error("observability config is invalid: {0}")]
    Config(String),

    #[error("event listener bind failed: {0}")]
    ListenerBind(String),

    #[error("event payload was rejected: {0}")]
    InvalidEvent(String),

    #[error("snapshot sink failed: {0}")]
    SnapshotSink(String),

    #[error("safe-state sink failed: {0}")]
    SafeStateSink(String),

    #[error("ROS 2 publisher failed: {0}")]
    Ros2Publisher(String),

    #[error("internal observability error: {0}")]
    Internal(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

impl ObservabilityError {
    /// Project the error onto a stable [`ErrorCode`] for cross-process
    /// diagnostics. Operators consume this through the bounded
    /// diagnostics store and the status snapshot.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            ObservabilityError::Config(_) | ObservabilityError::ListenerBind(_) => {
                ErrorCode::ConfigInvalid
            }
            ObservabilityError::InvalidEvent(_) => ErrorCode::Unsupported,
            ObservabilityError::SnapshotSink(_)
            | ObservabilityError::SafeStateSink(_)
            | ObservabilityError::Ros2Publisher(_)
            | ObservabilityError::Internal(_)
            | ObservabilityError::Io(_)
            | ObservabilityError::Serialization(_) => ErrorCode::Internal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ObservabilityError;
    use tensorplate_protocol::ErrorCode;

    #[test]
    fn config_error_maps_to_config_invalid() {
        let err = ObservabilityError::Config("bad".into());
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
    }

    #[test]
    fn invalid_event_maps_to_unsupported() {
        let err = ObservabilityError::InvalidEvent("unknown version".into());
        assert_eq!(err.code(), ErrorCode::Unsupported);
    }

    #[test]
    fn sink_errors_map_to_internal() {
        let err = ObservabilityError::SnapshotSink("disk full".into());
        assert_eq!(err.code(), ErrorCode::Internal);
    }
}
