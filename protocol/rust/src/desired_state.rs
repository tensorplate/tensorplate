// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F07-T02: Rust mirror of `protocol/schemas/desired_state.json`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::model_spec::ModelSpec;
use crate::SCHEMA_VERSION;

/// Rollout policy. v0.1.0 supports `Immediate` only; future versions
/// (canary / staged) extend without renaming.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutStrategy {
    #[default]
    Immediate,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Rollout {
    #[serde(default)]
    pub strategy: RolloutStrategy,
}

/// Mirror of `protocol/schemas/desired_state.json`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DesiredState {
    pub schema_version: String,
    pub deployment_id: String,
    pub bundle_digest: String,
    #[serde(default = "default_bundle_format_version")]
    pub bundle_format_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_spec: Option<ModelSpec>,
    #[serde(default, skip_serializing_if = "is_default_rollout")]
    pub rollout: Rollout,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

fn default_bundle_format_version() -> String {
    crate::BUNDLE_FORMAT_VERSION.to_string()
}

fn is_default_rollout(r: &Rollout) -> bool {
    r == &Rollout::default()
}

/// Validation errors raised by [`DesiredState::new`].
#[derive(Debug, thiserror::Error)]
pub enum DesiredStateError {
    #[error("DesiredState.deployment_id must be non-empty")]
    EmptyDeploymentId,
    #[error("DesiredState.bundle_digest must be non-empty and follow the `algo:hex` form")]
    InvalidBundleDigest,
}

fn looks_like_digest(d: &str) -> bool {
    // pattern: ^[a-z0-9-]+:[A-Fa-f0-9]+$
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

impl DesiredState {
    /// Build and validate a [`DesiredState`].
    ///
    /// # Errors
    ///
    /// See [`DesiredStateError`].
    pub fn new(
        deployment_id: impl Into<String>,
        bundle_digest: impl Into<String>,
        model_spec: Option<ModelSpec>,
        rollout: Rollout,
        labels: BTreeMap<String, String>,
    ) -> Result<Self, DesiredStateError> {
        let deployment_id = deployment_id.into();
        if deployment_id.is_empty() {
            return Err(DesiredStateError::EmptyDeploymentId);
        }
        let bundle_digest = bundle_digest.into();
        if !looks_like_digest(&bundle_digest) {
            return Err(DesiredStateError::InvalidBundleDigest);
        }
        Ok(Self {
            schema_version: SCHEMA_VERSION.to_string(),
            deployment_id,
            bundle_digest,
            bundle_format_version: crate::BUNDLE_FORMAT_VERSION.to_string(),
            model_spec,
            rollout,
            labels,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{
        BTreeMap, DesiredState, DesiredStateError, Rollout, RolloutStrategy, SCHEMA_VERSION,
    };
    use crate::decode_with_version_check;
    use crate::model_spec::{ModelClass, ModelSpec, PrecisionHint};

    fn sample() -> DesiredState {
        let spec = ModelSpec::new(
            "yolov8n",
            ModelClass::Vision,
            "models/yolov8n.engine",
            "tensorrt",
            PrecisionHint::Fp16,
            None,
        )
        .expect("valid spec");
        let mut labels = BTreeMap::new();
        labels.insert("env".into(), "lab".into());
        DesiredState::new(
            "deploy-2024-05-09-1",
            "sha256:cafebabe",
            Some(spec),
            Rollout::default(),
            labels,
        )
        .expect("valid")
    }

    #[test]
    fn round_trip_preserves_fields() {
        let s = sample();
        let json = serde_json::to_string(&s).expect("serialize");
        let back: DesiredState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(s, back);
        assert_eq!(back.schema_version, SCHEMA_VERSION);
        assert_eq!(back.rollout.strategy, RolloutStrategy::Immediate);
    }

    #[test]
    fn rejects_empty_deployment_id() {
        assert!(matches!(
            DesiredState::new("", "sha256:abcd", None, Rollout::default(), BTreeMap::new()),
            Err(DesiredStateError::EmptyDeploymentId)
        ));
    }

    #[test]
    fn rejects_malformed_bundle_digest() {
        for bad in ["", "not-a-digest", "sha256:", ":abcd", "SHA256:abcd"] {
            let r = DesiredState::new("deploy", bad, None, Rollout::default(), BTreeMap::new());
            assert!(
                matches!(r, Err(DesiredStateError::InvalidBundleDigest)),
                "expected InvalidBundleDigest for `{bad}`"
            );
        }
    }

    #[test]
    fn version_check_decoder_rejects_old_schema() {
        let json = r#"{"schema_version":"0.0","deployment_id":"d","bundle_digest":"sha256:ab"}"#;
        let err = decode_with_version_check::<DesiredState>(json).expect_err("rejected");
        assert!(matches!(
            err,
            crate::DecodeError::UnsupportedSchemaVersion { .. }
        ));
    }
}
