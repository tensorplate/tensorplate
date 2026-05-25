// SPDX-License-Identifier: Apache-2.0
//
// V01-E11-F01-T02: typed CLI errors mapped to documented exit codes.
//
// The CLI never panics on user input. Every failure path lands in this
// taxonomy so help text, JSON output, and shell scripts see the same
// shape. Exit codes are stable enough for the release validation harness
// to assert on; new variants must extend the table, not reuse a code
// already meaning something different.

use std::io;

use tensorplate_protocol::ErrorCode;

/// Result alias used throughout the CLI crate.
pub type CliResult<T> = Result<T, CliError>;

/// Process exit code categories. The numeric values are part of the v0.1.0
/// CLI contract and are documented in `docs/cli/exit-codes.md`.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExitCode {
    /// Command succeeded.
    Success = 0,
    /// Generic failure that does not map to any specific bucket below.
    Failure = 1,
    /// CLI arguments or config file rejected before any agent call.
    Usage = 2,
    /// Agent reachable, agent rejected the request with a typed error.
    AgentError = 3,
    /// Agent unreachable, connection refused, or transport timed out.
    Transport = 4,
    /// A concurrent agent transaction is in flight.
    Busy = 5,
    /// Operation is structurally unavailable (e.g. rollback with no
    /// previous active deployment, unsupported profile mode).
    Unavailable = 6,
    /// `tensorplate doctor` found at least one failing check.
    DoctorFindings = 10,
    /// `tensorplate infer` failed for a typed serving reason.
    InferenceFailed = 11,
}

impl ExitCode {
    /// Convert to the process exit code the binary returns.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Typed CLI error taxonomy. Carries enough context for the human and
/// JSON renderers without leaking raw `io::Error` stacks at error sites.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("usage error: {0}")]
    Usage(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("profile mode `{mode}` is reserved for future releases and not implemented in v0.1.0")]
    UnsupportedProfile { mode: String },

    #[error("agent transport failed: {message}")]
    Transport {
        message: String,
        hint: Option<String>,
    },

    #[error("agent returned an error response: {message}")]
    Agent {
        code: ErrorCode,
        message: String,
        context: Option<String>,
        hint: Option<String>,
    },

    #[error("agent is busy with an in-flight transaction")]
    Busy { hint: Option<String> },

    #[error("operation is unavailable: {message}")]
    Unavailable {
        message: String,
        hint: Option<String>,
    },

    #[error("operation timed out after {timeout_ms} ms")]
    Timeout {
        timeout_ms: u64,
        hint: Option<String>,
    },

    #[error("doctor reported {failing} failing check(s) out of {total}")]
    DoctorFindings { failing: u32, total: u32 },

    #[error("inference failed: {message}")]
    Inference {
        code: ErrorCode,
        message: String,
        hint: Option<String>,
    },

    #[error("io error: {0}")]
    Io(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("internal cli error: {0}")]
    Internal(String),
}

impl CliError {
    /// Map this error onto a process exit code.
    #[must_use]
    pub fn exit_code(&self) -> ExitCode {
        match self {
            CliError::Usage(_) | CliError::Config(_) => ExitCode::Usage,
            CliError::UnsupportedProfile { .. } | CliError::Unavailable { .. } => {
                ExitCode::Unavailable
            }
            CliError::Transport { .. } | CliError::Timeout { .. } => ExitCode::Transport,
            CliError::Agent { .. } => ExitCode::AgentError,
            CliError::Busy { .. } => ExitCode::Busy,
            CliError::DoctorFindings { .. } => ExitCode::DoctorFindings,
            CliError::Inference { .. } => ExitCode::InferenceFailed,
            CliError::Io(_) | CliError::Serialization(_) | CliError::Internal(_) => {
                ExitCode::Failure
            }
        }
    }

    /// Stable, machine-readable error code surfaced via JSON output.
    #[must_use]
    pub fn protocol_code(&self) -> ErrorCode {
        match self {
            CliError::Usage(_) | CliError::Config(_) | CliError::DoctorFindings { .. } => {
                ErrorCode::ConfigInvalid
            }
            CliError::UnsupportedProfile { .. } | CliError::Unavailable { .. } => {
                ErrorCode::Unsupported
            }
            CliError::Transport { .. } | CliError::Timeout { .. } => ErrorCode::Timeout,
            CliError::Agent { code, .. } | CliError::Inference { code, .. } => *code,
            CliError::Busy { .. } => ErrorCode::NotReady,
            CliError::Io(_) | CliError::Serialization(_) | CliError::Internal(_) => {
                ErrorCode::Internal
            }
        }
    }

    /// Short actionable hint surfaced after the error message. Returned
    /// to operators in human output and as an `error.hint` field in JSON.
    #[must_use]
    pub fn hint(&self) -> Option<&str> {
        match self {
            CliError::Transport { hint, .. }
            | CliError::Agent { hint, .. }
            | CliError::Busy { hint }
            | CliError::Unavailable { hint, .. }
            | CliError::Timeout { hint, .. }
            | CliError::Inference { hint, .. } => hint.as_deref(),
            CliError::UnsupportedProfile { .. } => {
                Some("v0.1.0 supports `local` and `url` profile modes; use a different profile")
            }
            CliError::DoctorFindings { .. } => {
                Some("re-run `tensorplate doctor` after addressing each `fail` finding")
            }
            _ => None,
        }
    }

    /// Extra context surfaced in JSON output.
    #[must_use]
    pub fn context(&self) -> Option<&str> {
        match self {
            CliError::Agent { context, .. } => context.as_deref(),
            _ => None,
        }
    }
}

impl From<io::Error> for CliError {
    fn from(value: io::Error) -> Self {
        CliError::Io(value.to_string())
    }
}

impl From<serde_json::Error> for CliError {
    fn from(value: serde_json::Error) -> Self {
        CliError::Serialization(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_stable() {
        // Locking the numeric values down so the release validation
        // harness can assert specific shell exit codes.
        assert_eq!(ExitCode::Success.as_u8(), 0);
        assert_eq!(ExitCode::Failure.as_u8(), 1);
        assert_eq!(ExitCode::Usage.as_u8(), 2);
        assert_eq!(ExitCode::AgentError.as_u8(), 3);
        assert_eq!(ExitCode::Transport.as_u8(), 4);
        assert_eq!(ExitCode::Busy.as_u8(), 5);
        assert_eq!(ExitCode::Unavailable.as_u8(), 6);
        assert_eq!(ExitCode::DoctorFindings.as_u8(), 10);
        assert_eq!(ExitCode::InferenceFailed.as_u8(), 11);
    }

    #[test]
    fn error_maps_to_documented_exit_code() {
        let err = CliError::Agent {
            code: ErrorCode::Unsupported,
            message: "bad backend".into(),
            context: None,
            hint: None,
        };
        assert_eq!(err.exit_code(), ExitCode::AgentError);
        assert_eq!(err.protocol_code(), ErrorCode::Unsupported);
    }

    #[test]
    fn unsupported_profile_carries_actionable_hint() {
        let err = CliError::UnsupportedProfile {
            mode: "ssh_tunnel".into(),
        };
        assert!(err.hint().is_some());
        assert_eq!(err.exit_code(), ExitCode::Unavailable);
    }

    #[test]
    fn timeout_classifies_as_transport_error() {
        let err = CliError::Timeout {
            timeout_ms: 500,
            hint: None,
        };
        assert_eq!(err.exit_code(), ExitCode::Transport);
    }
}
