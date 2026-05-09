// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F02: Rust mirror of `protocol/schemas/model_spec.json` and the C++
// `tensorplate::ModelSpec` value object.

use serde::{Deserialize, Serialize};

use crate::SCHEMA_VERSION;

/// Model class taxonomy. v0.1.0 validates only `Vision` and `Vla`; the
/// remaining values are reserved so future bundle parsing does not require
/// a schema bump.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelClass {
    Vision,
    Speech,
    Language,
    Vla,
    Embedding,
    Custom,
}

impl ModelClass {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vision => "vision",
            Self::Speech => "speech",
            Self::Language => "language",
            Self::Vla => "vla",
            Self::Embedding => "embedding",
            Self::Custom => "custom",
        }
    }
}

/// Numeric precision profile hint. Backends that cannot honor the hint
/// return [`crate::ErrorCode::Unsupported`] rather than silently
/// downgrading.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrecisionHint {
    #[default]
    Auto,
    Fp32,
    Fp16,
    Bfloat16,
    Int8,
    Int4,
}

impl PrecisionHint {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Fp32 => "fp32",
            Self::Fp16 => "fp16",
            Self::Bfloat16 => "bfloat16",
            Self::Int8 => "int8",
            Self::Int4 => "int4",
        }
    }
}

/// Mirror of `tensorplate::ModelSpec` (C++) and
/// `protocol/schemas/model_spec.json`.
///
/// The struct is the wire representation. Use [`ModelSpec::new`] to
/// validate a freshly-built instance against the same rules the C++
/// runtime applies.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelSpec {
    pub schema_version: String,
    pub model_id: String,
    pub model_class: ModelClass,
    pub artifact_path: String,
    pub backend_hint: String,
    #[serde(default, skip_serializing_if = "is_default_precision")]
    pub precision_hint: PrecisionHint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

// serde's `skip_serializing_if` requires `fn(&T) -> bool`; the by-value
// suggestion does not apply here.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_default_precision(p: &PrecisionHint) -> bool {
    *p == PrecisionHint::Auto
}

/// Validation errors raised by [`ModelSpec::new`].
#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("ModelSpec.model_id must be non-empty")]
    EmptyModelId,
    #[error("ModelSpec.artifact_path must be non-empty")]
    EmptyArtifactPath,
    #[error("ModelSpec.backend_hint must be non-empty")]
    EmptyBackendHint,
    #[error("ModelSpec.profile_id, if present, must be non-empty")]
    EmptyProfileId,
}

impl ModelSpec {
    /// Build and validate a new ModelSpec at the v0.1 schema version.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] if any required string field is empty
    /// or `profile_id` is present-but-empty. The same rules are applied
    /// by the C++ `ModelSpec::create` factory.
    pub fn new(
        model_id: impl Into<String>,
        model_class: ModelClass,
        artifact_path: impl Into<String>,
        backend_hint: impl Into<String>,
        precision_hint: PrecisionHint,
        profile_id: Option<String>,
    ) -> Result<Self, ValidationError> {
        let model_id = model_id.into();
        if model_id.is_empty() {
            return Err(ValidationError::EmptyModelId);
        }
        let artifact_path = artifact_path.into();
        if artifact_path.is_empty() {
            return Err(ValidationError::EmptyArtifactPath);
        }
        let backend_hint = backend_hint.into();
        if backend_hint.is_empty() {
            return Err(ValidationError::EmptyBackendHint);
        }
        if matches!(profile_id.as_deref(), Some("")) {
            return Err(ValidationError::EmptyProfileId);
        }
        Ok(Self {
            schema_version: SCHEMA_VERSION.to_string(),
            model_id,
            model_class,
            artifact_path,
            backend_hint,
            precision_hint,
            profile_id,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{ModelClass, ModelSpec, PrecisionHint, ValidationError, SCHEMA_VERSION};
    use crate::decode_with_version_check;

    fn sample() -> ModelSpec {
        ModelSpec::new(
            "yolov8n",
            ModelClass::Vision,
            "models/yolov8n.engine",
            "tensorrt",
            PrecisionHint::Fp16,
            Some("orin-nano-fp16".into()),
        )
        .expect("valid spec")
    }

    #[test]
    fn round_trip_preserves_all_fields() {
        let spec = sample();
        let json = serde_json::to_string(&spec).expect("serialize");
        let back: ModelSpec = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(spec, back);
    }

    #[test]
    fn class_and_precision_serialize_as_snake_case() {
        let spec = sample();
        let json = serde_json::to_string(&spec).expect("serialize");
        assert!(json.contains("\"model_class\":\"vision\""));
        assert!(json.contains("\"precision_hint\":\"fp16\""));
        assert!(json.contains("\"backend_hint\":\"tensorrt\""));
    }

    #[test]
    fn auto_precision_is_omitted_for_compactness() {
        let spec = ModelSpec::new(
            "smolvla",
            ModelClass::Vla,
            "models/smolvla.pt",
            "python_pytorch",
            PrecisionHint::Auto,
            None,
        )
        .expect("valid");
        let json = serde_json::to_string(&spec).expect("serialize");
        assert!(!json.contains("precision_hint"));
        // Defaults to Auto on decode.
        let back: ModelSpec = serde_json::from_str(&json).expect("decode");
        assert_eq!(back.precision_hint, PrecisionHint::Auto);
    }

    #[test]
    fn validation_rejects_empty_required_fields() {
        assert!(matches!(
            ModelSpec::new(
                "",
                ModelClass::Vision,
                "p",
                "tensorrt",
                PrecisionHint::Auto,
                None
            ),
            Err(ValidationError::EmptyModelId)
        ));
        assert!(matches!(
            ModelSpec::new(
                "id",
                ModelClass::Vision,
                "",
                "tensorrt",
                PrecisionHint::Auto,
                None
            ),
            Err(ValidationError::EmptyArtifactPath)
        ));
        assert!(matches!(
            ModelSpec::new("id", ModelClass::Vision, "p", "", PrecisionHint::Auto, None),
            Err(ValidationError::EmptyBackendHint)
        ));
        assert!(matches!(
            ModelSpec::new(
                "id",
                ModelClass::Vision,
                "p",
                "tensorrt",
                PrecisionHint::Auto,
                Some(String::new())
            ),
            Err(ValidationError::EmptyProfileId)
        ));
    }

    #[test]
    fn version_check_decoder_accepts_current_schema() {
        let json = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","model_id":"x","model_class":"vision","artifact_path":"p","backend_hint":"tensorrt"}}"#
        );
        let spec: ModelSpec = decode_with_version_check(&json).expect("decode");
        assert_eq!(spec.model_id, "x");
        assert_eq!(spec.precision_hint, PrecisionHint::Auto);
    }

    #[test]
    fn version_check_decoder_rejects_old_schema() {
        let json = r#"{"schema_version":"0.0","model_id":"x","model_class":"vision","artifact_path":"p","backend_hint":"tensorrt"}"#;
        let err = decode_with_version_check::<ModelSpec>(json).expect_err("rejected");
        assert!(matches!(
            err,
            crate::DecodeError::UnsupportedSchemaVersion { .. }
        ));
    }
}
