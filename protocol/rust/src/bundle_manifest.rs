// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F03: Rust mirror of `protocol/schemas/bundle_manifest.json`.
//
// Owned by the agent's deploy-time bundle verifier. V01-E13 may add fields
// without bumping the bundle format version as long as the fields listed
// here keep their shape; the verifier deliberately tolerates unknown extra
// fields so the manifest envelope can grow without breaking older agents.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::model_spec::{ModelClass, PrecisionHint};
use crate::{DecodeError, ValidatePayload, SCHEMA_VERSION};

/// Role each artifact plays inside a bundle.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactRole {
    Model,
    Tokenizer,
    Calibration,
    VitisCalibration,
    Precompiled,
    Auxiliary,
}

impl ArtifactRole {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Tokenizer => "tokenizer",
            Self::Calibration => "calibration",
            Self::VitisCalibration => "vitis_calibration",
            Self::Precompiled => "precompiled",
            Self::Auxiliary => "auxiliary",
        }
    }
}

/// Device family the bundle declares as compatible.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)]
pub enum DeviceFamily {
    #[default]
    Any,
    #[serde(rename = "jetson-orin")]
    JetsonOrin,
    #[serde(rename = "jetson-thor")]
    JetsonThor,
    Kria,
    #[serde(rename = "x86_64")]
    X86_64,
}

impl DeviceFamily {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::JetsonOrin => "jetson-orin",
            Self::JetsonThor => "jetson-thor",
            Self::Kria => "kria",
            Self::X86_64 => "x86_64",
        }
    }
}

/// One artifact entry inside the manifest.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BundleArtifact {
    pub role: ArtifactRole,
    pub path: String,
    pub digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_size: Option<u64>,
}

/// Hardware compatibility envelope.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TargetHardware {
    #[serde(default)]
    pub device_family: DeviceFamily,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_memory_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_estimate_bytes: Option<u64>,
}

/// Inclusive runtime version range.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuntimeCompatibility {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_runtime_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_runtime_version: Option<String>,
}

/// Capability requirements declared by the bundle.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct CapabilityRequirements {
    #[serde(default, rename = "async", skip_serializing_if = "is_false")]
    pub async_: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub streaming: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub generation: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub kv_cache: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub fixed_shape: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

/// V01-E08-F03 manifest envelope. Mirrors
/// `protocol/schemas/bundle_manifest.json`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BundleManifest {
    pub schema_version: String,
    pub name: String,
    pub version: String,
    pub format_version: String,
    pub model_class: ModelClass,
    pub backend_hint: String,
    #[serde(default, skip_serializing_if = "is_default_precision")]
    pub precision_hint: PrecisionHint,
    pub artifacts: Vec<BundleArtifact>,
    #[serde(default, skip_serializing_if = "is_default_target_hardware")]
    pub target_hardware: TargetHardware,
    #[serde(default, skip_serializing_if = "is_default_runtime_compatibility")]
    pub runtime_compatibility: RuntimeCompatibility,
    #[serde(default, skip_serializing_if = "is_default_capability_requirements")]
    pub capability_requirements: CapabilityRequirements,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_digest: Option<String>,
    /// Forward-compatible extra fields preserved for V01-E13.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty", flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_default_precision(p: &PrecisionHint) -> bool {
    *p == PrecisionHint::Auto
}

fn is_default_target_hardware(h: &TargetHardware) -> bool {
    h == &TargetHardware::default()
}

fn is_default_runtime_compatibility(c: &RuntimeCompatibility) -> bool {
    c == &RuntimeCompatibility::default()
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_default_capability_requirements(c: &CapabilityRequirements) -> bool {
    *c == CapabilityRequirements::default()
}

/// Validation errors raised by [`BundleManifest::new`].
#[derive(Debug, thiserror::Error)]
pub enum BundleManifestError {
    #[error("BundleManifest.name must be non-empty")]
    EmptyName,
    #[error("BundleManifest.version must be non-empty")]
    EmptyVersion,
    #[error("BundleManifest.format_version must follow MAJOR.MINOR form")]
    InvalidFormatVersion,
    #[error("BundleManifest.backend_hint must be non-empty")]
    EmptyBackendHint,
    #[error("BundleManifest.artifacts must contain at least one entry")]
    NoArtifacts,
    #[error("BundleManifest.artifacts must contain exactly one entry with role=model")]
    MissingModelArtifact,
    #[error(
        "BundleManifest.artifacts[{index}].path must not be absolute or contain `..` segments"
    )]
    UnsafeArtifactPath { index: usize },
    #[error("BundleManifest.artifacts[{index}].digest must follow `algo:hex` form")]
    InvalidArtifactDigest { index: usize },
    #[error("BundleManifest.artifacts contains a duplicate path `{path}`")]
    DuplicateArtifactPath { path: String },
    #[error("BundleManifest.manifest_digest, if present, must follow the `algo:hex` form")]
    InvalidManifestDigest,
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

fn looks_like_version_pair(v: &str) -> bool {
    let mut parts = v.split('.');
    let major = parts.next();
    let minor = parts.next();
    let extra = parts.next();
    matches!(major, Some(s) if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
        && matches!(minor, Some(s) if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
        && extra.is_none()
}

fn artifact_path_is_safe(p: &str) -> bool {
    if p.is_empty() {
        return false;
    }
    if p.starts_with('/') {
        return false;
    }
    !p.split('/').any(|seg| seg == ".." || seg.is_empty())
}

impl BundleManifest {
    /// Validate the manifest envelope. Returns the manifest itself on success
    /// so callers can chain the call after `serde_json::from_str`.
    ///
    /// # Errors
    ///
    /// See [`BundleManifestError`].
    pub fn validate(self) -> Result<Self, BundleManifestError> {
        if self.name.is_empty() {
            return Err(BundleManifestError::EmptyName);
        }
        if self.version.is_empty() {
            return Err(BundleManifestError::EmptyVersion);
        }
        if !looks_like_version_pair(&self.format_version) {
            return Err(BundleManifestError::InvalidFormatVersion);
        }
        if self.backend_hint.is_empty() {
            return Err(BundleManifestError::EmptyBackendHint);
        }
        if self.artifacts.is_empty() {
            return Err(BundleManifestError::NoArtifacts);
        }
        let mut model_count = 0usize;
        let mut seen_paths: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for (index, art) in self.artifacts.iter().enumerate() {
            if art.role == ArtifactRole::Model {
                model_count += 1;
            }
            if !artifact_path_is_safe(&art.path) {
                return Err(BundleManifestError::UnsafeArtifactPath { index });
            }
            if !seen_paths.insert(art.path.as_str()) {
                return Err(BundleManifestError::DuplicateArtifactPath {
                    path: art.path.clone(),
                });
            }
            if !looks_like_digest(&art.digest) {
                return Err(BundleManifestError::InvalidArtifactDigest { index });
            }
        }
        if model_count != 1 {
            return Err(BundleManifestError::MissingModelArtifact);
        }
        if let Some(ref d) = self.manifest_digest {
            if !looks_like_digest(d) {
                return Err(BundleManifestError::InvalidManifestDigest);
            }
        }
        Ok(self)
    }

    /// Return the single `model` artifact entry, if present. Validated
    /// manifests always contain exactly one; the `Option` is preserved
    /// so callers that build a manifest by hand (e.g. tests) do not
    /// panic when querying an in-progress value.
    #[must_use]
    pub fn model_artifact(&self) -> Option<&BundleArtifact> {
        self.artifacts
            .iter()
            .find(|a| a.role == ArtifactRole::Model)
    }
}

impl ValidatePayload for BundleManifest {
    fn validate_payload(self) -> Result<Self, DecodeError> {
        self.validate()
            .map_err(|err| DecodeError::InvalidPayload(err.to_string()))
    }
}

impl Default for BundleManifest {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            name: String::new(),
            version: String::new(),
            format_version: crate::BUNDLE_FORMAT_VERSION.to_string(),
            model_class: ModelClass::Vision,
            backend_hint: String::new(),
            precision_hint: PrecisionHint::Auto,
            artifacts: Vec::new(),
            target_hardware: TargetHardware::default(),
            runtime_compatibility: RuntimeCompatibility::default(),
            capability_requirements: CapabilityRequirements::default(),
            manifest_digest: None,
            extra: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::collections::BTreeMap;

    use super::{
        ArtifactRole, BundleArtifact, BundleManifest, BundleManifestError, CapabilityRequirements,
        DeviceFamily, ModelClass, PrecisionHint, RuntimeCompatibility, TargetHardware,
        SCHEMA_VERSION,
    };
    use crate::decode_with_version_check;

    fn vision_manifest() -> BundleManifest {
        BundleManifest {
            schema_version: SCHEMA_VERSION.to_string(),
            name: "yolov8n".into(),
            version: "1.0.0".into(),
            format_version: "0.1".into(),
            model_class: ModelClass::Vision,
            backend_hint: "tensorrt".into(),
            precision_hint: PrecisionHint::Fp16,
            artifacts: vec![BundleArtifact {
                role: ArtifactRole::Model,
                path: "model.engine".into(),
                digest: "sha256:cafebabe".into(),
                byte_size: Some(1024),
            }],
            target_hardware: TargetHardware {
                device_family: DeviceFamily::JetsonOrin,
                min_memory_bytes: None,
                memory_estimate_bytes: Some(512 * 1024 * 1024),
            },
            runtime_compatibility: RuntimeCompatibility {
                min_runtime_version: Some("0.1.0".into()),
                max_runtime_version: Some("0.2.0".into()),
            },
            capability_requirements: CapabilityRequirements::default(),
            manifest_digest: Some("sha256:deadbeef".into()),
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn vision_manifest_round_trips() {
        let m = vision_manifest().validate().expect("valid");
        let json = serde_json::to_string(&m).expect("serialize");
        let back: BundleManifest = serde_json::from_str(&json).expect("deserialize");
        let back = back.validate().expect("re-validate");
        assert_eq!(m, back);
        assert_eq!(back.schema_version, SCHEMA_VERSION);
        assert_eq!(
            back.model_artifact().expect("model artifact present").path,
            "model.engine"
        );
    }

    #[test]
    fn rejects_unsafe_artifact_path() {
        let mut m = vision_manifest();
        m.artifacts[0].path = "../escape.bin".into();
        assert!(matches!(
            m.validate(),
            Err(BundleManifestError::UnsafeArtifactPath { .. })
        ));
        let mut m = vision_manifest();
        m.artifacts[0].path = "/abs/path".into();
        assert!(matches!(
            m.validate(),
            Err(BundleManifestError::UnsafeArtifactPath { .. })
        ));
    }

    #[test]
    fn rejects_missing_model_role() {
        let mut m = vision_manifest();
        m.artifacts[0].role = ArtifactRole::Auxiliary;
        assert!(matches!(
            m.validate(),
            Err(BundleManifestError::MissingModelArtifact)
        ));
    }

    #[test]
    fn rejects_invalid_digest() {
        let mut m = vision_manifest();
        m.artifacts[0].digest = "not-a-digest".into();
        assert!(matches!(
            m.validate(),
            Err(BundleManifestError::InvalidArtifactDigest { index: 0 })
        ));
    }

    #[test]
    fn rejects_unknown_schema_version() {
        let json = r#"{"schema_version":"99.99","name":"x","version":"1","format_version":"0.1","model_class":"vision","backend_hint":"tensorrt","artifacts":[{"role":"model","path":"m","digest":"sha256:ab"}]}"#;
        let err = decode_with_version_check::<BundleManifest>(json).expect_err("rejected");
        assert!(matches!(
            err,
            crate::DecodeError::UnsupportedSchemaVersion { .. }
        ));
    }

    #[test]
    fn unknown_extra_fields_are_preserved() {
        let json = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"x","version":"1","format_version":"0.1","model_class":"vision","backend_hint":"tensorrt","artifacts":[{{"role":"model","path":"m","digest":"sha256:ab"}}],"future_field":42}}"#
        );
        let m: BundleManifest = decode_with_version_check(&json).expect("decode");
        assert!(m.extra.contains_key("future_field"));
    }
}
