// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F07-T05: Rust mirror of `protocol/schemas/deploy_transaction.json`.

use serde::{Deserialize, Serialize};

use crate::error::ErrorCode;
use crate::{DecodeError, ValidatePayload, SCHEMA_VERSION};

/// State machine for the deploy transaction. Forward-only along the
/// success path; `Failed` and `RolledBack` are terminal.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployState {
    Received,
    Verified,
    Staged,
    CapacityChecked,
    Prepared,
    Warmed,
    Promoted,
    Active,
    Failed,
    RolledBack,
}

impl DeployState {
    /// True if this state is terminal (no further transitions).
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Active | Self::Failed | Self::RolledBack)
    }
}

/// Failure metadata attached to terminal-failure transactions.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeployFailure {
    pub error_code: ErrorCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// True if retrying the same transaction is reasonable. False
    /// implies operator intervention is required.
    pub recoverable: bool,
}

/// Mirror of `protocol/schemas/deploy_transaction.json`. Durable state
/// owned by `tensorplate-agent`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeployTransaction {
    pub schema_version: String,
    pub transaction_id: String,
    pub deployment_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub bundle_digest: String,
    pub state: DeployState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_monotonic_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition_monotonic_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<DeployFailure>,
}

/// Validation errors raised by [`DeployTransaction::new`].
#[derive(Debug, thiserror::Error)]
pub enum DeployTransactionError {
    #[error("DeployTransaction.transaction_id must be non-empty")]
    EmptyTransactionId,
    #[error("DeployTransaction.deployment_id must be non-empty")]
    EmptyDeploymentId,
    #[error("DeployTransaction.failure must be present when state is Failed or RolledBack")]
    MissingFailureMetadata,
    #[error("DeployTransaction.failure must be absent for non-terminal-failure states")]
    SpuriousFailureMetadata,
    #[error("DeployTransaction.bundle_digest, if present, must follow the `algo:hex` form")]
    InvalidBundleDigest,
}

fn looks_like_digest(d: &str) -> bool {
    if let Some((algo, hex)) = d.split_once(':') {
        let algo_ok = !algo.is_empty()
            && algo
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        let hex_ok = !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit());
        algo_ok && hex_ok
    } else {
        false
    }
}

impl DeployTransaction {
    /// Build and validate a [`DeployTransaction`].
    ///
    /// # Errors
    ///
    /// See [`DeployTransactionError`].
    pub fn new(
        transaction_id: impl Into<String>,
        deployment_id: impl Into<String>,
        bundle_digest: impl Into<String>,
        state: DeployState,
        started_monotonic_ns: Option<u64>,
        last_transition_monotonic_ns: Option<u64>,
        failure: Option<DeployFailure>,
    ) -> Result<Self, DeployTransactionError> {
        let transaction_id = transaction_id.into();
        if transaction_id.is_empty() {
            return Err(DeployTransactionError::EmptyTransactionId);
        }
        let deployment_id = deployment_id.into();
        if deployment_id.is_empty() {
            return Err(DeployTransactionError::EmptyDeploymentId);
        }
        let bundle_digest = bundle_digest.into();
        if !bundle_digest.is_empty() && !looks_like_digest(&bundle_digest) {
            return Err(DeployTransactionError::InvalidBundleDigest);
        }
        let is_failure_state = matches!(state, DeployState::Failed | DeployState::RolledBack);
        if is_failure_state && failure.is_none() {
            return Err(DeployTransactionError::MissingFailureMetadata);
        }
        if !is_failure_state && failure.is_some() {
            return Err(DeployTransactionError::SpuriousFailureMetadata);
        }
        Ok(Self {
            schema_version: SCHEMA_VERSION.to_string(),
            transaction_id,
            deployment_id,
            bundle_digest,
            state,
            started_monotonic_ns,
            last_transition_monotonic_ns,
            failure,
        })
    }
}

impl ValidatePayload for DeployTransaction {
    fn validate_payload(self) -> Result<Self, DecodeError> {
        Self::new(
            self.transaction_id,
            self.deployment_id,
            self.bundle_digest,
            self.state,
            self.started_monotonic_ns,
            self.last_transition_monotonic_ns,
            self.failure,
        )
        .map_err(|err| DecodeError::InvalidPayload(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{
        DeployFailure, DeployState, DeployTransaction, DeployTransactionError, SCHEMA_VERSION,
    };
    use crate::decode_with_version_check;
    use crate::error::ErrorCode;

    #[test]
    fn happy_path_round_trips() {
        let tx = DeployTransaction::new(
            "tx-1",
            "deploy-1",
            "sha256:cafe",
            DeployState::Active,
            Some(1_000),
            Some(2_000),
            None,
        )
        .expect("valid");
        let json = serde_json::to_string(&tx).expect("serialize");
        let back: DeployTransaction = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(tx, back);
        assert_eq!(back.schema_version, SCHEMA_VERSION);
        assert!(back.state.is_terminal());
    }

    #[test]
    fn failed_transaction_round_trips_with_typed_failure() {
        let tx = DeployTransaction::new(
            "tx-2",
            "deploy-1",
            "sha256:cafe",
            DeployState::Failed,
            Some(1),
            Some(2),
            Some(DeployFailure {
                error_code: ErrorCode::OomError,
                message: Some("backend rejected: insufficient memory".into()),
                recoverable: false,
            }),
        )
        .expect("valid");
        let json = serde_json::to_string(&tx).expect("serialize");
        let back: DeployTransaction = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(tx, back);
        assert_eq!(
            back.failure.as_ref().expect("present").error_code,
            ErrorCode::OomError
        );
    }

    #[test]
    fn rejects_failure_metadata_inconsistency() {
        // Failed state without failure metadata.
        let r = DeployTransaction::new(
            "tx",
            "d",
            "sha256:ab",
            DeployState::Failed,
            None,
            None,
            None,
        );
        assert!(matches!(
            r,
            Err(DeployTransactionError::MissingFailureMetadata)
        ));

        // Active state with failure metadata.
        let r = DeployTransaction::new(
            "tx",
            "d",
            "sha256:ab",
            DeployState::Active,
            None,
            None,
            Some(DeployFailure {
                error_code: ErrorCode::Internal,
                message: None,
                recoverable: true,
            }),
        );
        assert!(matches!(
            r,
            Err(DeployTransactionError::SpuriousFailureMetadata)
        ));
    }

    #[test]
    fn rejects_empty_transaction_id() {
        let r = DeployTransaction::new(
            "",
            "d",
            "sha256:ab",
            DeployState::Received,
            None,
            None,
            None,
        );
        assert!(matches!(r, Err(DeployTransactionError::EmptyTransactionId)));
    }

    #[test]
    fn version_check_decoder_rejects_old_schema() {
        let json = r#"{"schema_version":"0.0","transaction_id":"t","deployment_id":"d","state":"received"}"#;
        let err = decode_with_version_check::<DeployTransaction>(json).expect_err("rejected");
        assert!(matches!(
            err,
            crate::DecodeError::UnsupportedSchemaVersion { .. }
        ));
    }

    #[test]
    fn version_check_decoder_rejects_current_schema_failed_without_failure() {
        let json = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","transaction_id":"tx","deployment_id":"d","bundle_digest":"sha256:ab","state":"failed"}}"#
        );
        let err = decode_with_version_check::<DeployTransaction>(&json).expect_err("rejected");
        assert!(matches!(err, crate::DecodeError::InvalidPayload(_)));
    }
}
