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
use crate::tensor_view::{DType, Layout};
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
    Weights,
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
            Self::Weights => "weights",
        }
    }
}

/// Concrete artifact representation. Aligns with the optional `kind`
/// field on each manifest artifact. The parser falls back to filename
/// heuristics when the field is omitted; bundle authors should set it
/// explicitly for non-NVIDIA backends.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    TensorrtEngine,
    LibtorchState,
    PythonPytorchEntry,
    Onnx,
    VitisXmodel,
    Weights,
    Tokenizer,
    Calibration,
    Auxiliary,
}

impl ArtifactKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TensorrtEngine => "tensorrt_engine",
            Self::LibtorchState => "libtorch_state",
            Self::PythonPytorchEntry => "python_pytorch_entry",
            Self::Onnx => "onnx",
            Self::VitisXmodel => "vitis_xmodel",
            Self::Weights => "weights",
            Self::Tokenizer => "tokenizer",
            Self::Calibration => "calibration",
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ArtifactKind>,
    pub path: String,
    pub digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Bounded length for the artifact `description` diagnostic field.
pub const MAX_ARTIFACT_DESCRIPTION_BYTES: usize = 256;

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
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
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
    #[serde(default, skip_serializing_if = "is_false")]
    pub deterministic_latency: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub control_loop_integration: bool,
    /// Optional list of operations the bundle relies on. The verifier
    /// records but does not reject on this field in v0.1.0; backends use
    /// it for diagnostic op-coverage reports.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub op_coverage_limits: Vec<String>,
    /// Optional duplicate of `target_hardware.memory_estimate_bytes`
    /// scoped to backend-aware reporting. Either may be present; the
    /// verifier prefers the top-level `target_hardware` value when both
    /// are set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_estimate_bytes: Option<u64>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

/// Modality slug attached to a named input.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputModality {
    Image,
    Video,
    Audio,
    Text,
    Tokens,
    #[default]
    Tensor,
    State,
    Control,
    Custom,
}

impl InputModality {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Text => "text",
            Self::Tokens => "tokens",
            Self::Tensor => "tensor",
            Self::State => "state",
            Self::Control => "control",
            Self::Custom => "custom",
        }
    }
}

/// Bounded length for the `encoding` and `semantics` fields.
pub const MAX_IO_LABEL_BYTES: usize = 64;

/// One named input declared by the bundle.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BundleInput {
    pub name: String,
    #[serde(default, skip_serializing_if = "is_default_modality")]
    pub modality: InputModality,
    pub dtype: DType,
    pub shape: Vec<i64>,
    #[serde(default, skip_serializing_if = "is_default_layout")]
    pub layout: Layout,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub optional: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantics: Option<String>,
}

/// One named output declared by the bundle.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BundleOutput {
    pub name: String,
    pub dtype: DType,
    pub shape: Vec<i64>,
    #[serde(default, skip_serializing_if = "is_default_layout")]
    pub layout: Layout,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantics: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub control_loop: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_default_modality(m: &InputModality) -> bool {
    *m == InputModality::Tensor
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_default_layout(l: &Layout) -> bool {
    *l == Layout::RowMajor
}

/// Jetson-shaped precision metadata.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct JetsonPrecision {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_profiles: Vec<PrecisionHint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tensorrt_engine_profile: Option<String>,
}

/// Vitis-AI-shaped precision metadata. The bundle declares quantization /
/// calibration intent; v0.1.0 parses but does not execute against Vitis.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VitisQuantizeStrategy {
    PostTraining,
    Calibration,
    Qat,
}

impl VitisQuantizeStrategy {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PostTraining => "post_training",
            Self::Calibration => "calibration",
            Self::Qat => "qat",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct VitisAiPrecision {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quantize_strategy: Option<VitisQuantizeStrategy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration_dataset_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration_sample_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dpu_arch: Option<String>,
}

/// Backend-shaped precision metadata. Distinct from the top-level
/// `precision_hint`; the `profile` field here mirrors the hint so a
/// bundle can declare both with consistent values, but neither field
/// implies the other.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct PrecisionMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<PrecisionHint>,
    #[serde(default, skip_serializing_if = "is_default_jetson")]
    pub jetson: JetsonPrecision,
    #[serde(default, skip_serializing_if = "is_default_vitis")]
    pub vitis_ai: VitisAiPrecision,
}

fn is_default_jetson(j: &JetsonPrecision) -> bool {
    j == &JetsonPrecision::default()
}

fn is_default_vitis(v: &VitisAiPrecision) -> bool {
    v == &VitisAiPrecision::default()
}

/// Vision-class block. Optional metadata used by vision fixtures.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VisionBlock {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_size: Option<VisionInputSize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_space: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalization: Option<VisionNormalization>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VisionInputSize {
    pub height: u32,
    pub width: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VisionNormalization {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mean: Vec<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub std: Vec<f64>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SpeechBlock {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate_hz: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature_extractor: Option<String>,
}

/// Reserved language-block metadata. Parsed but not exercised in v0.1.0.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LanguageBlock {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokenizer: Option<LanguageTokenizer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GenerationConfig>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenizerKind {
    Sentencepiece,
    Tiktoken,
    Huggingface,
    ByteLevelBpe,
    Custom,
}

impl TokenizerKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sentencepiece => "sentencepiece",
            Self::Tiktoken => "tiktoken",
            Self::Huggingface => "huggingface",
            Self::ByteLevelBpe => "byte_level_bpe",
            Self::Custom => "custom",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguageTokenizer {
    pub reference: String,
    pub kind: TokenizerKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_or_digest: Option<String>,
}

/// Reserved generation parameters. Bundle authors may declare defaults
/// here; v0.1.0 parses and round-trips but does not execute generation.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GenerationConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_new_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub streaming: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VlaBlock {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_frequency_hz: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_horizon_steps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_chunk_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_modalities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_dim: Option<u32>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingBlock {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dim: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalize: Option<bool>,
}

/// Optional per-class metadata. Each block is validated only when the
/// manifest's `model_class` matches; mismatched blocks raise
/// [`BundleManifestError::MismatchedModelClassBlock`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelBlocks {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<VisionBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speech: Option<SpeechBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<LanguageBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vla: Option<VlaBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<EmbeddingBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Optional manifest signature. v0.1.0 parses but does not verify.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ManifestSignature {
    pub algorithm: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    pub value: String,
}

/// Optional provenance fields. v0.1.0 parses but does not verify.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProvenanceMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_timestamp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sbom: Option<SbomReference>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SbomReference {
    pub format: String,
    pub path: String,
    pub digest: String,
}

/// V01-E13 manifest envelope. Mirrors
/// `protocol/schemas/bundle_manifest.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<BundleInput>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<BundleOutput>,
    #[serde(default, skip_serializing_if = "is_default_target_hardware")]
    pub target_hardware: TargetHardware,
    #[serde(default, skip_serializing_if = "is_default_runtime_compatibility")]
    pub runtime_compatibility: RuntimeCompatibility,
    #[serde(default, skip_serializing_if = "is_default_capability_requirements")]
    pub capability_requirements: CapabilityRequirements,
    #[serde(default, skip_serializing_if = "is_default_precision_metadata")]
    pub precision: PrecisionMetadata,
    #[serde(default, skip_serializing_if = "is_default_model_blocks")]
    pub model_blocks: ModelBlocks,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<ManifestSignature>,
    #[serde(default, skip_serializing_if = "is_default_provenance")]
    pub provenance: ProvenanceMetadata,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_digest: Option<String>,
    /// Forward-compatible extra fields preserved for future minor
    /// additions without breaking older readers.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty", flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

// BundleManifest carries `f64` fields (e.g. VLA control frequency), so
// `Eq` is not derivable. Equality between two parsed manifests is
// available via the `PartialEq` impl; tests that need a stable comparison
// already round-trip through JSON.

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

fn is_default_capability_requirements(c: &CapabilityRequirements) -> bool {
    c == &CapabilityRequirements::default()
}

fn is_default_precision_metadata(p: &PrecisionMetadata) -> bool {
    p == &PrecisionMetadata::default()
}

fn is_default_model_blocks(m: &ModelBlocks) -> bool {
    m == &ModelBlocks::default()
}

fn is_default_provenance(p: &ProvenanceMetadata) -> bool {
    p == &ProvenanceMetadata::default()
}

/// Validation errors raised by [`BundleManifest::validate`].
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
    #[error(
        "BundleManifest.backend_hint `{0}` is not recognized; v0.1.0 accepts `tensorrt`, `libtorch`, `python_pytorch`; `vitis_ai` and `onnxruntime` are reserved extension slots"
    )]
    UnknownBackendHint(String),
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
    #[error("BundleManifest.inputs[{index}].name must be non-empty")]
    EmptyInputName { index: usize },
    #[error("BundleManifest.outputs[{index}].name must be non-empty")]
    EmptyOutputName { index: usize },
    #[error("BundleManifest.inputs contains a duplicate name `{name}`")]
    DuplicateInputName { name: String },
    #[error("BundleManifest.outputs contains a duplicate name `{name}`")]
    DuplicateOutputName { name: String },
    #[error(
        "BundleManifest.inputs[{index}].shape axis `{axis}` is invalid (must be `-1` or a positive integer)"
    )]
    InvalidInputShape { index: usize, axis: i64 },
    #[error(
        "BundleManifest.outputs[{index}].shape axis `{axis}` is invalid (must be `-1` or a positive integer)"
    )]
    InvalidOutputShape { index: usize, axis: i64 },
    #[error("BundleManifest.inputs[{index}].encoding `{encoding}` exceeds the {limit}-byte bound")]
    InputEncodingTooLong {
        index: usize,
        encoding: String,
        limit: usize,
    },
    #[error(
        "BundleManifest.model_blocks.{block} is set but model_class is `{model_class}`; populate only the block matching the declared class"
    )]
    MismatchedModelClassBlock {
        block: &'static str,
        model_class: &'static str,
    },
    #[error(
        "BundleManifest.model_blocks.language.tokenizer.reference must be non-empty for the `language` class"
    )]
    EmptyTokenizerReference,
    #[error(
        "BundleManifest.precision.vitis_ai.calibration_dataset_digest must follow the `algo:hex` form"
    )]
    InvalidVitisCalibrationDigest,
    #[error("BundleManifest.signature is present but missing required `algorithm` or `value`")]
    IncompleteSignature,
}

fn validate_model_blocks(
    class: ModelClass,
    blocks: &ModelBlocks,
) -> Result<(), BundleManifestError> {
    let mismatch = |block: &'static str| BundleManifestError::MismatchedModelClassBlock {
        block,
        model_class: class.as_str(),
    };
    if blocks.vision.is_some() && !matches!(class, ModelClass::Vision | ModelClass::Custom) {
        return Err(mismatch("vision"));
    }
    if blocks.speech.is_some() && !matches!(class, ModelClass::Speech | ModelClass::Custom) {
        return Err(mismatch("speech"));
    }
    if blocks.language.is_some()
        && !matches!(
            class,
            ModelClass::Language | ModelClass::Vla | ModelClass::Custom
        )
    {
        return Err(mismatch("language"));
    }
    if blocks.vla.is_some() && !matches!(class, ModelClass::Vla | ModelClass::Custom) {
        return Err(mismatch("vla"));
    }
    if blocks.embedding.is_some() && !matches!(class, ModelClass::Embedding | ModelClass::Custom) {
        return Err(mismatch("embedding"));
    }
    if let Some(language) = blocks.language.as_ref() {
        if let Some(tok) = language.tokenizer.as_ref() {
            if tok.reference.is_empty() {
                return Err(BundleManifestError::EmptyTokenizerReference);
            }
        }
    }
    Ok(())
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

/// v0.1.0-recognized backend hint values. Unknown values are rejected at
/// parse time so a bundle cannot smuggle a typo past the verifier; the
/// agent's separate `available_backends` config still owns runtime
/// availability.
pub const RECOGNIZED_BACKEND_HINTS: &[&str] = &[
    "tensorrt",
    "libtorch",
    "python_pytorch",
    "vitis_ai",
    "onnxruntime",
    "mock",
];

impl BundleManifest {
    /// Validate the manifest envelope. Returns the manifest itself on success
    /// so callers can chain the call after `serde_json::from_str`.
    ///
    /// # Errors
    ///
    /// See [`BundleManifestError`].
    #[allow(clippy::too_many_lines)]
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
        if !RECOGNIZED_BACKEND_HINTS.contains(&self.backend_hint.as_str()) {
            return Err(BundleManifestError::UnknownBackendHint(
                self.backend_hint.clone(),
            ));
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
            if let Some(desc) = art.description.as_deref() {
                if desc.len() > MAX_ARTIFACT_DESCRIPTION_BYTES {
                    return Err(BundleManifestError::InvalidArtifactDigest { index });
                }
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

        // Inputs.
        let mut seen_inputs: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for (index, input) in self.inputs.iter().enumerate() {
            if input.name.is_empty() {
                return Err(BundleManifestError::EmptyInputName { index });
            }
            if !seen_inputs.insert(input.name.as_str()) {
                return Err(BundleManifestError::DuplicateInputName {
                    name: input.name.clone(),
                });
            }
            for axis in &input.shape {
                if *axis < -1 || *axis == 0 {
                    return Err(BundleManifestError::InvalidInputShape { index, axis: *axis });
                }
            }
            if let Some(encoding) = input.encoding.as_deref() {
                if encoding.len() > MAX_IO_LABEL_BYTES {
                    return Err(BundleManifestError::InputEncodingTooLong {
                        index,
                        encoding: encoding.to_string(),
                        limit: MAX_IO_LABEL_BYTES,
                    });
                }
            }
        }

        // Outputs.
        let mut seen_outputs: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for (index, output) in self.outputs.iter().enumerate() {
            if output.name.is_empty() {
                return Err(BundleManifestError::EmptyOutputName { index });
            }
            if !seen_outputs.insert(output.name.as_str()) {
                return Err(BundleManifestError::DuplicateOutputName {
                    name: output.name.clone(),
                });
            }
            for axis in &output.shape {
                if *axis < -1 || *axis == 0 {
                    return Err(BundleManifestError::InvalidOutputShape { index, axis: *axis });
                }
            }
        }

        // Model-class block consistency.
        validate_model_blocks(self.model_class, &self.model_blocks)?;

        // Vitis precision metadata digest, when present, must look right.
        if let Some(d) = self
            .precision
            .vitis_ai
            .calibration_dataset_digest
            .as_deref()
        {
            if !looks_like_digest(d) {
                return Err(BundleManifestError::InvalidVitisCalibrationDigest);
            }
        }

        // Signature shape.
        if let Some(ref sig) = self.signature {
            if sig.algorithm.is_empty() || sig.value.is_empty() {
                return Err(BundleManifestError::IncompleteSignature);
            }
        }

        // SBOM digest format.
        if let Some(sbom) = self.provenance.sbom.as_ref() {
            if !looks_like_digest(&sbom.digest) {
                return Err(BundleManifestError::InvalidVitisCalibrationDigest);
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
            inputs: Vec::new(),
            outputs: Vec::new(),
            target_hardware: TargetHardware::default(),
            runtime_compatibility: RuntimeCompatibility::default(),
            capability_requirements: CapabilityRequirements::default(),
            precision: PrecisionMetadata::default(),
            model_blocks: ModelBlocks::default(),
            signature: None,
            provenance: ProvenanceMetadata::default(),
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
                kind: None,
                path: "model.engine".into(),
                digest: "sha256:cafebabe".into(),
                byte_size: Some(1024),
                description: None,
            }],
            inputs: Vec::new(),
            outputs: Vec::new(),
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
            precision: super::PrecisionMetadata::default(),
            model_blocks: super::ModelBlocks::default(),
            signature: None,
            provenance: super::ProvenanceMetadata::default(),
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

    #[test]
    fn rejects_unknown_backend_hint() {
        let mut m = vision_manifest();
        m.backend_hint = "tensorrt_optimized_xyz".into();
        let err = m.validate().expect_err("must reject");
        assert!(matches!(err, BundleManifestError::UnknownBackendHint(_)));
    }

    #[test]
    fn rejects_duplicate_input_names() {
        use super::{BundleInput, InputModality};
        use crate::tensor_view::{DType, Layout};
        let mut m = vision_manifest();
        m.inputs = vec![
            BundleInput {
                name: "image".into(),
                modality: InputModality::Image,
                dtype: DType::Uint8,
                shape: vec![1, 3, 640, 640],
                layout: Layout::RowMajor,
                encoding: Some("rgb24".into()),
                optional: false,
                semantics: None,
            },
            BundleInput {
                name: "image".into(),
                modality: InputModality::Image,
                dtype: DType::Uint8,
                shape: vec![1, 3, 640, 640],
                layout: Layout::RowMajor,
                encoding: None,
                optional: false,
                semantics: None,
            },
        ];
        let err = m.validate().expect_err("must reject");
        assert!(matches!(
            err,
            BundleManifestError::DuplicateInputName { .. }
        ));
    }

    #[test]
    fn rejects_invalid_input_shape_axis() {
        use super::{BundleInput, InputModality};
        use crate::tensor_view::{DType, Layout};
        let mut m = vision_manifest();
        m.inputs = vec![BundleInput {
            name: "img".into(),
            modality: InputModality::Tensor,
            dtype: DType::Float32,
            shape: vec![1, -2, 3],
            layout: Layout::RowMajor,
            encoding: None,
            optional: false,
            semantics: None,
        }];
        let err = m.validate().expect_err("must reject");
        assert!(matches!(err, BundleManifestError::InvalidInputShape { .. }));
    }

    #[test]
    fn language_block_requires_consistent_model_class() {
        use super::{LanguageBlock, LanguageTokenizer, ModelBlocks, TokenizerKind};
        let mut m = vision_manifest();
        m.model_blocks = ModelBlocks {
            language: Some(LanguageBlock {
                tokenizer: Some(LanguageTokenizer {
                    reference: "spiece.model".into(),
                    kind: TokenizerKind::Sentencepiece,
                    revision_or_digest: None,
                }),
                ..LanguageBlock::default()
            }),
            ..ModelBlocks::default()
        };
        let err = m.validate().expect_err("must reject");
        assert!(matches!(
            err,
            BundleManifestError::MismatchedModelClassBlock { .. }
        ));
    }

    #[test]
    fn signature_must_be_complete() {
        use super::ManifestSignature;
        let mut m = vision_manifest();
        m.signature = Some(ManifestSignature {
            algorithm: String::new(),
            key_id: None,
            value: "sig".into(),
        });
        let err = m.validate().expect_err("must reject");
        assert!(matches!(err, BundleManifestError::IncompleteSignature));
    }

    #[test]
    fn vitis_calibration_digest_must_be_algo_hex() {
        use super::{PrecisionMetadata, VitisAiPrecision};
        let mut m = vision_manifest();
        m.precision = PrecisionMetadata {
            vitis_ai: VitisAiPrecision {
                calibration_dataset_digest: Some("not-a-digest".into()),
                ..VitisAiPrecision::default()
            },
            ..PrecisionMetadata::default()
        };
        let err = m.validate().expect_err("must reject");
        assert!(matches!(
            err,
            BundleManifestError::InvalidVitisCalibrationDigest
        ));
    }
}
