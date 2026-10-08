// SPDX-License-Identifier: Apache-2.0
//
// Format 0.2 bundle profile: the general profile fields and the speech
// contract a manifest opts into with `format_version: "0.2"`. Mirrors the
// `format_0_2` branch of `protocol/schemas/bundle_manifest.json`; the
// schema's `definitions.format_0_2` description lists the checks this
// decoder makes beyond the schema.

use std::collections::BTreeSet;
use std::fmt;

use serde::de::{DeserializeOwned, Deserializer, IgnoredAny};
use serde::{Deserialize, Serialize};

use crate::backend_descriptor::ComputeType;
use crate::bundle::digests_equal;
use crate::bundle_manifest::BundleArtifact;
use crate::json_numbers;
use crate::member_quota::MAX_MEMBER_SESSIONS;
use crate::memory_budget::{
    MemoryBudgetBreakdown, MEMORY_BUDGET_LINE_MAX_BYTES, MEMORY_BUDGET_LINE_NAMES,
};
use crate::model_spec::{ModelClass, PrecisionHint};
use crate::platform_memory_profile::BudgetDomainName;
use crate::serde_shape::{
    deserialize_map_only, deserialize_some, deserialize_some_map_only, deserialize_vec_map_only,
    is_canonical_identifier, is_canonical_snake_identifier,
};

/// The bundle format a manifest declares to opt into these fields.
pub const PROFILE_FORMAT_VERSION: &str = "0.2";

/// Built-in stage ids a `runtime_owned` pipeline stage may name, in
/// pipeline order.
pub const RUNTIME_PIPELINE_STAGES: [&str; 6] = [
    "ingress",
    "vad",
    "preprocessing",
    "backend",
    "postprocessing",
    "egress",
];

/// Audio formats the STT ingress stage accepts.
pub const STT_INPUT_AUDIO_FORMATS: [AudioFormat; 2] = [
    AudioFormat {
        encoding: AudioEncoding::PcmS16le,
        sample_rate_hz: 16_000,
        channels: 1,
    },
    AudioFormat {
        encoding: AudioEncoding::Mulaw,
        sample_rate_hz: 8_000,
        channels: 1,
    },
];

/// Audio formats the TTS egress stage produces.
pub const TTS_OUTPUT_AUDIO_FORMATS: [AudioFormat; 1] = [AudioFormat {
    encoding: AudioEncoding::PcmS16le,
    sample_rate_hz: 24_000,
    channels: 1,
}];

/// Keys format 0.2 accepts under `model_blocks`: the class slugs.
const MODEL_BLOCK_KEYS: [&str; 6] = ["vision", "speech", "language", "vla", "embedding", "custom"];
const MAX_IDENTIFIER_BYTES: usize = 64;
const MAX_HARDWARE_ROWS: usize = 64;
const MAX_PIPELINE_STAGES: usize = 16;
const MAX_STAGE_INTERFACE_CHARS: usize = 64;
const MAX_WARMUP_FIXTURES: usize = 16;
const MAX_WARMUP_FIXTURE_PATH_CHARS: usize = 256;
const MAX_WARMUP_REPETITIONS: u64 = 100;
const MAX_WARMUP_TIMEOUT_MS: u64 = 600_000;
const MAX_LANGUAGES: usize = 64;
const MAX_LANGUAGE_TAG_BYTES: usize = 35;
const MAX_VOICES: usize = 64;
const MAX_VARIANT_REVISION_BYTES: usize = 64;
const STT_FRAME_MS_MIN: u64 = 20;
const STT_FRAME_MS_MAX: u64 = 320;
const STT_MAX_UTTERANCE_MS: u64 = 30_000;
const TTS_MAX_SEGMENT_TEXT_BYTES: u64 = 4_096;
const TTS_MAX_SEGMENT_PHONEME_TOKENS: u64 = 200;
const TTS_MAX_SEGMENT_AUDIO_MS: u64 = 30_000;
const TTS_MAX_SYNTHESIS_TEXT_BYTES: u64 = 16_384;
const TTS_MAX_SYNTHESIS_AUDIO_MS: u64 = 120_000;

/// Stable manifest-local rejection codes, also carried in agent error context.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BundleRuleCode {
    ModelBlock,
    ReservedClass,
    StreamingState,
    BudgetLine,
    CallerExecution,
    ClassPayload,
    RequiredField,
    AmbiguousSelector,
    PrecisionConflict,
    ExplicitPrecision,
    BaseReference,
    VariantIdentity,
    VariantSupportLevel,
    ReservedVariant,
}

impl BundleRuleCode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ModelBlock => "bundle_r1_model_block",
            Self::ReservedClass => "bundle_r2_reserved_class",
            Self::StreamingState => "bundle_r3_streaming_state",
            Self::BudgetLine => "bundle_r7_budget_line",
            Self::CallerExecution => "bundle_r10_caller_execution",
            Self::ClassPayload => "bundle_r11_class_payload",
            Self::RequiredField => "bundle_r12_required_field",
            Self::AmbiguousSelector => "bundle_r12_ambiguous_selector",
            Self::PrecisionConflict => "bundle_r12_precision_conflict",
            Self::ExplicitPrecision => "bundle_r12_explicit_precision",
            Self::BaseReference => "bundle_r8_base_reference",
            Self::VariantIdentity => "bundle_r8_variant_identity",
            Self::VariantSupportLevel => "bundle_r8_variant_support_level",
            Self::ReservedVariant => "bundle_r8_reserved_variant",
        }
    }
}

impl fmt::Display for BundleRuleCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Typed failures decoding the format 0.2 fields of a manifest. Every
/// variant names the field it concerns.
#[derive(Debug, thiserror::Error)]
pub enum BundleProfileError {
    /// A manifest-local rule failed; the code is independent of diagnostic text.
    #[error("{code}: format 0.2 `{field}`: {reason}")]
    Rule {
        code: BundleRuleCode,
        field: String,
        reason: String,
    },
    /// The field has the wrong JSON shape or type, an unknown key, or a
    /// missing required key.
    #[error("format 0.2 `{field}` is malformed: {detail}")]
    Malformed { field: String, detail: String },

    /// The same key appears twice in one object.
    #[error("format 0.2 `{field}` declares `{key}` more than once")]
    DuplicateKey { field: String, key: String },

    /// A number is outside the nonnegative integer domain every format 0.2
    /// number shares, judged on its exact decimal lexeme.
    #[error("format 0.2 `{field}` holds an invalid number `{token}`: {reason}")]
    InvalidNumber {
        field: String,
        token: String,
        reason: &'static str,
    },

    /// The field decodes but its value is not allowed.
    #[error("format 0.2 `{field}`: {reason}")]
    Invalid { field: String, reason: String },
}

impl BundleProfileError {
    #[must_use]
    pub fn rule_code(&self) -> Option<BundleRuleCode> {
        match self {
            Self::Rule { code, .. } => Some(*code),
            _ => None,
        }
    }
}

pub(crate) fn rule(
    code: BundleRuleCode,
    field: &str,
    reason: impl Into<String>,
) -> BundleProfileError {
    BundleProfileError::Rule {
        code,
        field: field.into(),
        reason: reason.into(),
    }
}

pub(crate) fn required(field: &str, present: bool) -> Result<(), BundleProfileError> {
    if present {
        Ok(())
    } else {
        Err(rule(
            BundleRuleCode::RequiredField,
            field,
            "is required by this format 0.2 profile",
        ))
    }
}

pub(crate) fn check_model_class(class: ModelClass) -> Result<(), BundleProfileError> {
    if matches!(
        class,
        ModelClass::Language | ModelClass::Embedding | ModelClass::Custom
    ) {
        return Err(rule(
            BundleRuleCode::ReservedClass,
            "model_class",
            format!(
                "`{}` is reserved and cannot deploy under format 0.2",
                class.as_str()
            ),
        ));
    }
    Ok(())
}

/// Ends manifest validation, so a declaration is refused as reserved only
/// once the manifest's own rules pass. Artifact digests are not yet
/// verified against the files.
pub(crate) fn check_variant_kind(profile: &BundleProfile) -> Result<(), BundleProfileError> {
    match &profile.lineage {
        Some(lineage) => Err(rule(
            BundleRuleCode::ReservedVariant,
            "variant_identity.variant_kind",
            format!(
                "`{}` variants are reserved and cannot deploy",
                lineage.identity.kind.as_str()
            ),
        )),
        None => Ok(()),
    }
}

pub(crate) fn check_speech_precision(
    precision: PrecisionHint,
    compute: ComputeType,
) -> Result<(), BundleProfileError> {
    if precision == PrecisionHint::Auto {
        return Err(rule(
            BundleRuleCode::ExplicitPrecision,
            "precision_hint",
            "speech requires explicit precision, including during qualification",
        ));
    }
    let compatible = matches!(
        (precision, compute),
        (PrecisionHint::Fp32, ComputeType::Float32)
            | (PrecisionHint::Fp16, ComputeType::Float16)
            | (PrecisionHint::Bfloat16, ComputeType::Bfloat16)
            | (
                PrecisionHint::Int8,
                ComputeType::Int8
                    | ComputeType::Int8Float32
                    | ComputeType::Int8Float16
                    | ComputeType::Int8Bfloat16
            )
    );
    if !compatible {
        return Err(rule(
            BundleRuleCode::PrecisionConflict,
            "compute_type",
            "does not match precision_hint",
        ));
    }
    Ok(())
}

pub(crate) fn check_streaming_state(
    speech: &SpeechContract,
    budget: Option<&MemoryBudgetByDomain>,
) -> Result<(), BundleProfileError> {
    if speech.serving_mode == SpeechServingMode::Streaming
        && !budget.is_some_and(|b| {
            [&b.shared_pool, &b.guest_ram, &b.device_vram]
                .into_iter()
                .flatten()
                .any(|d| d.per_session_state_bytes > 0)
        })
    {
        return Err(rule(
            BundleRuleCode::StreamingState,
            "memory_budget_by_domain",
            "streaming speech requires nonzero per_session_state_bytes",
        ));
    }
    Ok(())
}

/// Requested support claim. A bundle cannot grant itself one; registry
/// evidence does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupportLevel {
    Production,
    Preview,
    Experimental,
}

impl SupportLevel {
    fn rank(self) -> u8 {
        match self {
            Self::Experimental => 0,
            Self::Preview => 1,
            Self::Production => 2,
        }
    }
}

/// What a variant is relative to its base. Every kind is reserved: it is
/// declared and validated, and never deploys.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VariantKind {
    /// Input data for the base.
    SpeakerEmbedding,
    /// State applied on the base.
    Adapter,
    /// An independent set of weights derived from the base.
    FullCheckpoint,
}

impl VariantKind {
    pub const ALL: [Self; 3] = [Self::SpeakerEmbedding, Self::Adapter, Self::FullCheckpoint];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SpeakerEmbedding => "speaker_embedding",
            Self::Adapter => "adapter",
            Self::FullCheckpoint => "full_checkpoint",
        }
    }
}

/// The base bundle a variant derives from, pinned by the canonical digest
/// the parser computes for its manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BaseModelRef {
    pub name: String,
    pub version: String,
    pub manifest_digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VariantIdentity {
    /// Stable across revisions of the variant.
    pub id: String,
    pub revision: String,
    pub kind: VariantKind,
}

/// A manifest's `base_model_ref` and `variant_identity`, declared together.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VariantLineage {
    pub base: BaseModelRef,
    pub identity: VariantIdentity,
}

/// What a deployment target knows about one base bundle. The caller
/// supplies it; no manifest can.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KnownBase {
    pub name: String,
    pub version: String,
    pub manifest_digest: String,
    /// The level the base holds on the target's platform support row, not
    /// the one its manifest asks for. A base on a planned row, or on no
    /// row, has no level here and must not be listed.
    pub support_level: SupportLevel,
    /// Identities other bundles already declared on this base. An entry
    /// carries no declarer, so the caller leaves out what the judged bundle
    /// itself declared before.
    pub variants: Vec<VariantIdentity>,
}

/// `degraded_profile` as declared: `null` disables quality-changing
/// degradation; a name reserves a separately qualified profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DegradedProfile {
    Disabled,
    Named(String),
}

/// Declared readiness warmup: inputs taken from hashed bundle artifacts.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Warmup {
    /// Bundle-relative artifact paths, each listed in `artifacts[]`.
    pub fixtures: Vec<String>,
    pub repetitions: u32,
    pub timeout_ms: u64,
}

/// Who executes a pipeline stage.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StageOwnership {
    RuntimeOwned,
    CallerOwned,
}

/// One ordered pipeline stage.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PipelineStage {
    /// A [`RUNTIME_PIPELINE_STAGES`] id when runtime-owned; the caller's
    /// span identity when caller-owned.
    pub stage: String,
    pub ownership: StageOwnership,
    /// False when the stage runs fused inside another and reports
    /// `not_observable` rather than its own span.
    pub observable: bool,
    /// Caller-owned stages only: where the caller reports the stage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface: Option<String>,
}

/// Per-domain budgets, each a canonical line-item breakdown.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct MemoryBudgetByDomain {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_pool: Option<MemoryBudgetBreakdown>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guest_ram: Option<MemoryBudgetBreakdown>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_vram: Option<MemoryBudgetBreakdown>,
}

impl MemoryBudgetByDomain {
    #[must_use]
    pub fn get(&self, domain: BudgetDomainName) -> Option<&MemoryBudgetBreakdown> {
        match domain {
            BudgetDomainName::SharedPool => self.shared_pool.as_ref(),
            BudgetDomainName::GuestRam => self.guest_ram.as_ref(),
            BudgetDomainName::DeviceVram => self.device_vram.as_ref(),
        }
    }

    /// Per-line sum over the declared domains, or `None` when a line's sum
    /// leaves the byte-line domain.
    #[must_use]
    pub fn line_sum(&self) -> Option<MemoryBudgetBreakdown> {
        let mut total = [0u64; 11];
        for domain in [&self.shared_pool, &self.guest_ram, &self.device_vram]
            .into_iter()
            .flatten()
        {
            for (sum, line) in total.iter_mut().zip(lines(domain)) {
                *sum = sum
                    .checked_add(line)
                    .filter(|v| *v <= MEMORY_BUDGET_LINE_MAX_BYTES)?;
            }
        }
        Some(from_lines(total))
    }
}

/// Speech task a bundle serves.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeechTask {
    Stt,
    Tts,
}

/// Serving mode of the speech contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeechServingMode {
    Streaming,
    Batch,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioEncoding {
    /// Signed 16-bit little-endian PCM.
    PcmS16le,
    /// G.711 μ-law.
    Mulaw,
}

impl AudioEncoding {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PcmS16le => "pcm_s16le",
            Self::Mulaw => "mulaw",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct AudioFormat {
    pub encoding: AudioEncoding,
    pub sample_rate_hz: u32,
    pub channels: u16,
}

/// Declared STT chunking limits, in milliseconds.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct SttChunking {
    pub frame_ms_min: u32,
    pub frame_ms_max: u32,
    pub max_utterance_ms: u32,
}

/// Declared TTS segment and synthesis limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct TtsChunking {
    pub max_segment_text_bytes: u32,
    pub max_segment_phoneme_tokens: u32,
    pub max_segment_audio_ms: u32,
    pub max_synthesis_text_bytes: u32,
    pub max_synthesis_audio_ms: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum SpeechChunking {
    Stt(SttChunking),
    Tts(TtsChunking),
}

/// A profile named by id and pinned by digest.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProfileReference {
    pub id: String,
    pub digest: String,
}

/// The format 0.2 speech block. A session's requested languages, voices
/// and formats must be a subset of these. Serializes as the manifest block
/// it was decoded from, in normalized form.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SpeechContract {
    pub task: SpeechTask,
    pub serving_mode: SpeechServingMode,
    pub languages: Vec<String>,
    /// TTS only.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub voices: Vec<String>,
    /// STT only; includes 16 kHz PCM.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub input_audio_formats: Vec<AudioFormat>,
    /// TTS only.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub output_audio_formats: Vec<AudioFormat>,
    pub chunking: SpeechChunking,
    pub algorithm_profile: ProfileReference,
    pub quality_profile_digest: String,
    pub benchmark_profile_digest: String,
}

impl SpeechContract {
    /// The stream serving mode this contract resolves to; batch contracts
    /// have none.
    #[must_use]
    pub fn wire_serving_mode(&self) -> Option<&'static str> {
        match (self.serving_mode, self.task) {
            (SpeechServingMode::Streaming, SpeechTask::Stt) => Some("stt_streaming"),
            (SpeechServingMode::Streaming, SpeechTask::Tts) => Some("tts_streaming"),
            (SpeechServingMode::Batch, _) => None,
        }
    }
}

/// The normalized format 0.2 fields of one manifest.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BundleProfile {
    pub runner_profile: Option<String>,
    pub hardware_compatibility: Vec<String>,
    pub compute_type: Option<ComputeType>,
    pub warmup: Option<Warmup>,
    pub pipeline_stages: Vec<PipelineStage>,
    pub memory_budget_by_domain: Option<MemoryBudgetByDomain>,
    pub memory_budget_breakdown_bytes: Option<MemoryBudgetBreakdown>,
    pub max_concurrent_sessions: Option<u32>,
    pub degraded_profile: Option<DegradedProfile>,
    pub support_level: Option<SupportLevel>,
    pub speech: Option<SpeechContract>,
    pub lineage: Option<VariantLineage>,
}

impl BundleProfile {
    /// Decode the format 0.2 fields from the manifest's own text, so each
    /// number is judged on its exact lexeme and no object may repeat a key.
    ///
    /// # Errors
    ///
    /// See [`BundleProfileError`].
    pub fn from_manifest_text(raw: &str) -> Result<Self, BundleProfileError> {
        let mut profile = Self::default();
        let mut capabilities_declared = false;
        let mut base = None;
        let mut identity = None;
        for (key, value) in object_members(raw, "manifest")? {
            match key.as_str() {
                "base_model_ref" => base = Some(base_model_ref(value)?),
                "variant_identity" => identity = Some(variant_identity(value)?),
                "capability_requirements" => capabilities_declared = true,
                "runner_profile" => {
                    profile.runner_profile = Some(identifier(&key, value, true)?);
                }
                "hardware_compatibility" => {
                    profile.hardware_compatibility = hardware_rows(value)?;
                }
                "compute_type" => profile.compute_type = Some(decode(&key, value)?),
                "warmup" => profile.warmup = Some(warmup(value)?),
                "pipeline_stages" => profile.pipeline_stages = pipeline_stages(value)?,
                "memory_budget_by_domain" => {
                    profile.memory_budget_by_domain = Some(budget_by_domain(value)?);
                }
                "memory_budget_breakdown_bytes" => {
                    check_budget_keys(&key, value)?;
                    profile.memory_budget_breakdown_bytes = Some(decode_object(&key, value)?);
                }
                "max_concurrent_sessions" => {
                    let sessions: u64 = decode(&key, value)?;
                    profile.max_concurrent_sessions =
                        Some(bounded(&key, sessions, 1, u64::from(MAX_MEMBER_SESSIONS))?);
                }
                "degraded_profile" => profile.degraded_profile = Some(degraded_profile(value)?),
                "support_level" => profile.support_level = Some(support_level(value)?),
                "model_blocks" => {
                    for (block, text) in object_members(value, "model_blocks")? {
                        if !MODEL_BLOCK_KEYS.contains(&block.as_str()) {
                            return Err(invalid(
                                "model_blocks",
                                format!("`{block}` is not a model class block"),
                            ));
                        }
                        if text.trim() == "null" {
                            return Err(rule(
                                BundleRuleCode::ModelBlock,
                                "model_blocks",
                                "a declared class block must be an object, not null",
                            ));
                        }
                        if block == "speech" {
                            profile.speech = Some(speech_contract(text)?);
                        }
                        if block == "vla"
                            && object_members(text, "model_blocks.vla")?
                                .iter()
                                .any(|(key, _)| key == "serving_mode")
                        {
                            return Err(rule(BundleRuleCode::ClassPayload, "model_blocks.vla.serving_mode", "VLA serving modes are reserved; omit the selector for the supported tensor payload"));
                        }
                    }
                }
                _ => {}
            }
        }
        if profile.support_level == Some(SupportLevel::Production) {
            required("capability_requirements", capabilities_declared)?;
        }
        profile.lineage = match (base, identity) {
            (Some(base), Some(identity)) => Some(VariantLineage { base, identity }),
            (None, None) => None,
            (Some(_), None) => {
                return Err(rule(
                    BundleRuleCode::BaseReference,
                    "variant_identity",
                    "is required beside `base_model_ref`",
                ))
            }
            (None, Some(_)) => {
                return Err(rule(
                    BundleRuleCode::BaseReference,
                    "base_model_ref",
                    "is required beside `variant_identity`",
                ))
            }
        };
        if let (Some(by_domain), Some(declared)) = (
            profile.memory_budget_by_domain.as_ref(),
            profile.memory_budget_breakdown_bytes.as_ref(),
        ) {
            if by_domain.line_sum().as_ref() != Some(declared) {
                return Err(invalid(
                    "memory_budget_breakdown_bytes",
                    "must equal the per-line sum of `memory_budget_by_domain`",
                ));
            }
        }
        Ok(profile)
    }

    /// Check the fields that refer to the rest of the manifest.
    ///
    /// # Errors
    ///
    /// [`BundleProfileError::Invalid`] when a warmup fixture is not an
    /// artifact the manifest lists (and therefore hashes).
    pub fn check_artifact_references(
        &self,
        artifacts: &[BundleArtifact],
    ) -> Result<(), BundleProfileError> {
        if let Some(warmup) = &self.warmup {
            for fixture in &warmup.fixtures {
                if !artifacts.iter().any(|a| &a.path == fixture) {
                    return Err(invalid(
                        "warmup.fixtures",
                        format!("`{fixture}` is not an `artifacts[].path`"),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Judge a variant declaration against the bases the caller knows: its
    /// base is one of them, its identity collides with no other variant of
    /// that base, and it asks for no more support than the base holds. A
    /// profile without a declaration passes. Passing does not make the
    /// variant deployable. `bases` lists each base once; the first match is
    /// the one judged.
    ///
    /// # Errors
    ///
    /// [`BundleProfileError::Rule`] naming the clause that failed.
    pub fn check_lineage(&self, bases: &[KnownBase]) -> Result<(), BundleProfileError> {
        let Some(lineage) = &self.lineage else {
            return Ok(());
        };
        let declared = &lineage.base;
        let Some(base) = bases.iter().find(|b| {
            b.name == declared.name
                && b.version == declared.version
                && digests_equal(&b.manifest_digest, &declared.manifest_digest)
        }) else {
            return Err(rule(
                BundleRuleCode::BaseReference,
                "base_model_ref",
                format!(
                    "no known base bundle is `{}` version `{}` with that manifest digest",
                    declared.name, declared.version
                ),
            ));
        };
        let identity = &lineage.identity;
        for other in base.variants.iter().filter(|v| v.id == identity.id) {
            if other.kind != identity.kind {
                return Err(rule(
                    BundleRuleCode::VariantIdentity,
                    "variant_identity.variant_kind",
                    format!(
                        "`{}` is already a `{}` variant of this base",
                        identity.id,
                        other.kind.as_str()
                    ),
                ));
            }
            if other.revision == identity.revision {
                return Err(rule(
                    BundleRuleCode::VariantIdentity,
                    "variant_identity.revision",
                    format!(
                        "`{}` revision `{}` is already declared on this base",
                        identity.id, identity.revision
                    ),
                ));
            }
        }
        let Some(requested) = self.support_level else {
            return Err(rule(
                BundleRuleCode::VariantSupportLevel,
                "support_level",
                "a variant must declare the support level it asks for",
            ));
        };
        if requested.rank() > base.support_level.rank() {
            return Err(rule(
                BundleRuleCode::VariantSupportLevel,
                "support_level",
                "a variant cannot ask for more support than its base holds",
            ));
        }
        Ok(())
    }
}

fn malformed(field: &str, detail: impl fmt::Display) -> BundleProfileError {
    BundleProfileError::Malformed {
        field: field.to_string(),
        detail: detail.to_string(),
    }
}

fn invalid(field: &str, reason: impl Into<String>) -> BundleProfileError {
    BundleProfileError::Invalid {
        field: field.to_string(),
        reason: reason.into(),
    }
}

fn skip_whitespace(bytes: &[u8], mut i: usize) -> usize {
    while bytes
        .get(i)
        .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
    {
        i += 1;
    }
    i
}

/// Index just past the string whose opening quote is at `i`.
fn skip_string(bytes: &[u8], mut i: usize) -> usize {
    i += 1;
    while let Some(b) = bytes.get(i) {
        match b {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    i
}

/// Index just past the JSON value that starts at `i`.
fn skip_value(bytes: &[u8], i: usize) -> usize {
    match bytes.get(i) {
        Some(b'"') => skip_string(bytes, i),
        Some(b'{' | b'[') => {
            let mut depth = 0usize;
            let mut j = i;
            while let Some(b) = bytes.get(j) {
                match b {
                    b'"' => {
                        j = skip_string(bytes, j);
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            return j + 1;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            j
        }
        _ => {
            let mut j = i;
            while bytes
                .get(j)
                .is_some_and(|b| !matches!(b, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r'))
            {
                j += 1;
            }
            j
        }
    }
}

/// The members of the JSON object `text`, each with its value's exact text,
/// in document order. The text is parsed first, so the scan only walks
/// well-formed JSON; a repeated key rejects.
fn object_members<'a>(
    text: &'a str,
    field: &str,
) -> Result<Vec<(String, &'a str)>, BundleProfileError> {
    serde_json::from_str::<IgnoredAny>(text).map_err(|e| malformed(field, e))?;
    let bytes = text.as_bytes();
    let mut i = skip_whitespace(bytes, 0);
    if bytes.get(i) != Some(&b'{') {
        return Err(malformed(field, "expected a JSON object"));
    }
    i = skip_whitespace(bytes, i + 1);
    let mut members: Vec<(String, &'a str)> = Vec::new();
    while bytes.get(i) == Some(&b'"') {
        let key_end = skip_string(bytes, i);
        let key: String =
            serde_json::from_str(&text[i..key_end]).map_err(|e| malformed(field, e))?;
        let colon = skip_whitespace(bytes, key_end);
        let start = skip_whitespace(bytes, colon + 1);
        let end = skip_value(bytes, start);
        if members.iter().any(|(seen, _)| *seen == key) {
            return Err(BundleProfileError::DuplicateKey {
                field: field.to_string(),
                key,
            });
        }
        members.push((key, &text[start..end]));
        i = skip_whitespace(bytes, end);
        if bytes.get(i) == Some(&b',') {
            i = skip_whitespace(bytes, i + 1);
        }
    }
    Ok(members)
}

/// Every format 0.2 number is a nonnegative integer, so the byte-line
/// lexeme pass applies to each field's text as a whole.
fn canonical(field: &str, raw: &str) -> Result<String, BundleProfileError> {
    json_numbers::canonicalize_byte_lexemes(raw).map_err(|(token, reason)| {
        BundleProfileError::InvalidNumber {
            field: field.to_string(),
            token,
            reason,
        }
    })
}

fn decode<T: DeserializeOwned>(field: &str, raw: &str) -> Result<T, BundleProfileError> {
    serde_json::from_str(&canonical(field, raw)?).map_err(|e| malformed(field, e))
}

fn decode_object<T: DeserializeOwned>(field: &str, raw: &str) -> Result<T, BundleProfileError> {
    let text = canonical(field, raw)?;
    let mut deserializer = serde_json::Deserializer::from_str(&text);
    deserialize_map_only(&mut deserializer).map_err(|e| malformed(field, e))
}

fn bounded<T: TryFrom<u64>>(
    field: &str,
    value: u64,
    min: u64,
    max: u64,
) -> Result<T, BundleProfileError> {
    if !(min..=max).contains(&value) {
        return Err(invalid(field, format!("{value} is outside {min}..={max}")));
    }
    T::try_from(value).map_err(|_| invalid(field, format!("{value} does not fit")))
}

fn check_identifier(field: &str, value: &str, snake: bool) -> Result<(), BundleProfileError> {
    let well_formed = if snake {
        is_canonical_snake_identifier(value)
    } else {
        is_canonical_identifier(value)
    };
    if !well_formed || value.len() > MAX_IDENTIFIER_BYTES {
        let form = if snake {
            "lower_snake_case"
        } else {
            "lower-kebab-case"
        };
        return Err(invalid(
            field,
            format!("`{value}` is not a {form} identifier of at most {MAX_IDENTIFIER_BYTES} bytes"),
        ));
    }
    Ok(())
}

fn identifier(field: &str, raw: &str, snake: bool) -> Result<String, BundleProfileError> {
    let value: String = decode(field, raw)?;
    check_identifier(field, &value, snake)?;
    Ok(value)
}

fn unique_list(
    field: &str,
    values: &[String],
    max: usize,
    check: impl Fn(&str) -> Result<(), BundleProfileError>,
) -> Result<(), BundleProfileError> {
    if values.is_empty() || values.len() > max {
        return Err(invalid(field, format!("must list 1..={max} entries")));
    }
    let mut seen = BTreeSet::new();
    for value in values {
        check(value)?;
        if !seen.insert(value.as_str()) {
            return Err(invalid(field, format!("lists `{value}` twice")));
        }
    }
    Ok(())
}

fn hardware_rows(raw: &str) -> Result<Vec<String>, BundleProfileError> {
    const FIELD: &str = "hardware_compatibility";
    let rows: Vec<String> = decode(FIELD, raw)?;
    unique_list(FIELD, &rows, MAX_HARDWARE_ROWS, |row| {
        check_identifier(FIELD, row, false)
    })?;
    Ok(rows)
}

fn support_level(raw: &str) -> Result<SupportLevel, BundleProfileError> {
    const FIELD: &str = "support_level";
    let value: String = decode(FIELD, raw)?;
    match value.as_str() {
        "production" => Ok(SupportLevel::Production),
        "preview" => Ok(SupportLevel::Preview),
        "experimental" => Ok(SupportLevel::Experimental),
        other => Err(invalid(
            FIELD,
            format!("`{other}` is not a bundle support level"),
        )),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BaseModelRefWire {
    name: String,
    version: String,
    manifest_digest: String,
}

fn base_model_ref(raw: &str) -> Result<BaseModelRef, BundleProfileError> {
    const FIELD: &str = "base_model_ref";
    let wire: BaseModelRefWire = decode_object(FIELD, raw)?;
    for (key, value) in [("name", &wire.name), ("version", &wire.version)] {
        if value.is_empty() {
            return Err(invalid(&format!("{FIELD}.{key}"), "must not be empty"));
        }
    }
    if !is_sha256_digest(&wire.manifest_digest) {
        return Err(invalid(
            "base_model_ref.manifest_digest",
            "must be `sha256:` and 64 lowercase hex digits",
        ));
    }
    Ok(BaseModelRef {
        name: wire.name,
        version: wire.version,
        manifest_digest: wire.manifest_digest,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VariantIdentityWire {
    id: String,
    revision: String,
    variant_kind: String,
}

fn is_variant_revision(revision: &str) -> bool {
    revision.len() <= MAX_VARIANT_REVISION_BYTES
        && revision.split(['.', '_', '-']).all(|segment| {
            !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_alphanumeric())
        })
}

fn variant_identity(raw: &str) -> Result<VariantIdentity, BundleProfileError> {
    const FIELD: &str = "variant_identity";
    let wire: VariantIdentityWire = decode_object(FIELD, raw)?;
    check_identifier("variant_identity.id", &wire.id, true)?;
    if !is_variant_revision(&wire.revision) {
        return Err(invalid(
            "variant_identity.revision",
            format!(
                "`{}` is not alphanumeric segments joined by `.`, `_` or `-`, at most {MAX_VARIANT_REVISION_BYTES} bytes",
                wire.revision
            ),
        ));
    }
    let Some(kind) = VariantKind::ALL
        .into_iter()
        .find(|k| k.as_str() == wire.variant_kind)
    else {
        return Err(invalid(
            "variant_identity.variant_kind",
            format!("`{}` is not a variant kind", wire.variant_kind),
        ));
    };
    Ok(VariantIdentity {
        id: wire.id,
        revision: wire.revision,
        kind,
    })
}

fn degraded_profile(raw: &str) -> Result<DegradedProfile, BundleProfileError> {
    const FIELD: &str = "degraded_profile";
    let value: Option<String> = decode(FIELD, raw)?;
    match value {
        None => Ok(DegradedProfile::Disabled),
        Some(name) => {
            check_identifier(FIELD, &name, true)?;
            Ok(DegradedProfile::Named(name))
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WarmupWire {
    fixtures: Vec<String>,
    repetitions: u64,
    timeout_ms: u64,
}

fn warmup(raw: &str) -> Result<Warmup, BundleProfileError> {
    let wire: WarmupWire = decode_object("warmup", raw)?;
    unique_list(
        "warmup.fixtures",
        &wire.fixtures,
        MAX_WARMUP_FIXTURES,
        |path| {
            if path.is_empty() || path.chars().count() > MAX_WARMUP_FIXTURE_PATH_CHARS {
                return Err(invalid(
                    "warmup.fixtures",
                    format!("paths are 1..={MAX_WARMUP_FIXTURE_PATH_CHARS} characters"),
                ));
            }
            Ok(())
        },
    )?;
    Ok(Warmup {
        fixtures: wire.fixtures,
        repetitions: bounded(
            "warmup.repetitions",
            wire.repetitions,
            1,
            MAX_WARMUP_REPETITIONS,
        )?,
        timeout_ms: bounded(
            "warmup.timeout_ms",
            wire.timeout_ms,
            1,
            MAX_WARMUP_TIMEOUT_MS,
        )?,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PipelineStageWire {
    stage: String,
    ownership: String,
    #[serde(default = "observable_by_default")]
    observable: bool,
    #[serde(default, deserialize_with = "deserialize_some")]
    interface: Option<String>,
}

fn observable_by_default() -> bool {
    true
}

fn pipeline_stages(raw: &str) -> Result<Vec<PipelineStage>, BundleProfileError> {
    const FIELD: &str = "pipeline_stages";
    let stages: Vec<serde_json::Value> =
        serde_json::from_str(raw).map_err(|e| malformed(FIELD, e))?;
    for stage in &stages {
        if stage.get("ownership").and_then(serde_json::Value::as_str) == Some("caller_owned") {
            if let Some(key) = stage.as_object().and_then(|o| {
                o.keys().find(|k| {
                    !["stage", "ownership", "observable", "interface"].contains(&k.as_str())
                })
            }) {
                return Err(rule(BundleRuleCode::CallerExecution, FIELD, format!("caller-owned stage cannot declare `{key}`; only interface/span identity is allowed")));
            }
        }
    }
    let text = canonical(FIELD, raw)?;
    let mut deserializer = serde_json::Deserializer::from_str(&text);
    let wires: Vec<PipelineStageWire> =
        deserialize_vec_map_only(&mut deserializer).map_err(|e| malformed(FIELD, e))?;
    if wires.is_empty() || wires.len() > MAX_PIPELINE_STAGES {
        return Err(invalid(
            FIELD,
            format!("must list 1..={MAX_PIPELINE_STAGES} stages"),
        ));
    }
    let mut seen = BTreeSet::new();
    let mut stages = Vec::with_capacity(wires.len());
    for wire in wires {
        check_identifier(FIELD, &wire.stage, true)?;
        if !seen.insert(wire.stage.clone()) {
            return Err(invalid(
                FIELD,
                format!("names stage `{}` twice", wire.stage),
            ));
        }
        let ownership = match wire.ownership.as_str() {
            "runtime_owned" => StageOwnership::RuntimeOwned,
            "caller_owned" => StageOwnership::CallerOwned,
            other => {
                return Err(invalid(
                    FIELD,
                    format!("`{other}` is not a stage ownership"),
                ))
            }
        };
        if ownership == StageOwnership::RuntimeOwned {
            if !RUNTIME_PIPELINE_STAGES.contains(&wire.stage.as_str()) {
                return Err(invalid(
                    FIELD,
                    format!("`{}` is not a built-in runtime stage", wire.stage),
                ));
            }
            if wire.interface.is_some() {
                return Err(invalid(
                    FIELD,
                    format!(
                        "runtime-owned stage `{}` declares a caller interface",
                        wire.stage
                    ),
                ));
            }
        }
        if let Some(interface) = &wire.interface {
            if interface.is_empty() || interface.chars().count() > MAX_STAGE_INTERFACE_CHARS {
                return Err(invalid(
                    FIELD,
                    format!("interface labels are 1..={MAX_STAGE_INTERFACE_CHARS} characters"),
                ));
            }
        }
        stages.push(PipelineStage {
            stage: wire.stage,
            ownership,
            observable: wire.observable,
            interface: wire.interface,
        });
    }
    Ok(stages)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BudgetByDomainWire {
    #[serde(default, deserialize_with = "deserialize_some_map_only")]
    shared_pool: Option<MemoryBudgetBreakdown>,
    #[serde(default, deserialize_with = "deserialize_some_map_only")]
    guest_ram: Option<MemoryBudgetBreakdown>,
    #[serde(default, deserialize_with = "deserialize_some_map_only")]
    device_vram: Option<MemoryBudgetBreakdown>,
}

fn budget_by_domain(raw: &str) -> Result<MemoryBudgetByDomain, BundleProfileError> {
    const FIELD: &str = "memory_budget_by_domain";
    for (domain, text) in object_members(raw, FIELD)? {
        if ["shared_pool", "guest_ram", "device_vram"].contains(&domain.as_str()) {
            check_budget_keys(&format!("{FIELD}.{domain}"), text)?;
        }
    }
    let wire: BudgetByDomainWire = decode_object(FIELD, raw)?;
    let by_domain = MemoryBudgetByDomain {
        shared_pool: wire.shared_pool,
        guest_ram: wire.guest_ram,
        device_vram: wire.device_vram,
    };
    if by_domain == MemoryBudgetByDomain::default() {
        return Err(invalid(FIELD, "must declare at least one domain"));
    }
    Ok(by_domain)
}

fn check_budget_keys(field: &str, text: &str) -> Result<(), BundleProfileError> {
    for (key, _) in object_members(text, field)? {
        if !MEMORY_BUDGET_LINE_NAMES.contains(&key.as_str()) {
            return Err(rule(
                BundleRuleCode::BudgetLine,
                field,
                format!("`{key}` is not a canonical budget line"),
            ));
        }
    }
    Ok(())
}

fn lines(b: &MemoryBudgetBreakdown) -> [u64; 11] {
    [
        b.model_weights_bytes,
        b.runtime_overhead_bytes,
        b.session_scratch_bytes,
        b.cache_bytes,
        b.step_scratch_bytes,
        b.output_queue_bytes,
        b.io_buffer_bytes,
        b.per_session_state_bytes,
        b.sidecar_process_bytes,
        b.os_reserve_bytes,
        b.backend_reserve_bytes,
    ]
}

fn from_lines(l: [u64; 11]) -> MemoryBudgetBreakdown {
    MemoryBudgetBreakdown {
        model_weights_bytes: l[0],
        runtime_overhead_bytes: l[1],
        session_scratch_bytes: l[2],
        cache_bytes: l[3],
        step_scratch_bytes: l[4],
        output_queue_bytes: l[5],
        io_buffer_bytes: l[6],
        per_session_state_bytes: l[7],
        sidecar_process_bytes: l[8],
        os_reserve_bytes: l[9],
        backend_reserve_bytes: l[10],
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AudioFormatWire {
    encoding: String,
    sample_rate_hz: u64,
    channels: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileReferenceWire {
    id: String,
    digest: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SttChunkingWire {
    frame_ms_min: u64,
    frame_ms_max: u64,
    max_utterance_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_field_names)]
struct TtsChunkingWire {
    max_segment_text_bytes: u64,
    max_segment_phoneme_tokens: u64,
    max_segment_audio_ms: u64,
    max_synthesis_text_bytes: u64,
    max_synthesis_audio_ms: u64,
}

fn some_vec_map_only<'de, D, T>(deserializer: D) -> Result<Option<Vec<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    deserialize_vec_map_only(deserializer).map(Some)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpeechContractWire {
    task: String,
    serving_mode: String,
    languages: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_some")]
    voices: Option<Vec<String>>,
    #[serde(default, deserialize_with = "some_vec_map_only")]
    input_audio_formats: Option<Vec<AudioFormatWire>>,
    #[serde(default, deserialize_with = "some_vec_map_only")]
    output_audio_formats: Option<Vec<AudioFormatWire>>,
    /// Decoded by task from its own text; required here.
    #[serde(rename = "chunking")]
    _chunking: IgnoredAny,
    #[serde(deserialize_with = "deserialize_map_only")]
    algorithm_profile: ProfileReferenceWire,
    quality_profile_digest: String,
    benchmark_profile_digest: String,
}

const SPEECH: &str = "model_blocks.speech";

fn check_speech_required_fields(raw: &str) -> Result<(), BundleProfileError> {
    let fields = object_members(raw, SPEECH)?;
    for key in [
        "task",
        "serving_mode",
        "languages",
        "algorithm_profile",
        "quality_profile_digest",
        "benchmark_profile_digest",
    ] {
        required(
            &format!("{SPEECH}.{key}"),
            fields.iter().any(|(name, _)| name == key),
        )?;
    }
    let chunking = fields.iter().find(|(key, _)| key == "chunking");
    if chunking.is_none() || chunking.is_some_and(|(_, text)| text.trim() == "null") {
        let streaming = fields.iter().any(|(key, text)| {
            key == "serving_mode"
                && serde_json::from_str::<String>(text).ok().as_deref() == Some("streaming")
        });
        return Err(rule(
            if streaming {
                BundleRuleCode::StreamingState
            } else {
                BundleRuleCode::RequiredField
            },
            "model_blocks.speech.chunking",
            "speech requires a chunking declaration",
        ));
    }
    Ok(())
}

fn speech_contract(raw: &str) -> Result<SpeechContract, BundleProfileError> {
    check_speech_required_fields(raw)?;
    let wire: SpeechContractWire = decode_object(SPEECH, raw)?;
    let task = match wire.task.as_str() {
        "stt" => SpeechTask::Stt,
        "tts" => SpeechTask::Tts,
        other => {
            return Err(invalid(
                SPEECH,
                format!("`{other}` is not a format 0.2 speech task"),
            ))
        }
    };
    let serving_mode = match wire.serving_mode.as_str() {
        "streaming" => SpeechServingMode::Streaming,
        "batch" => SpeechServingMode::Batch,
        other => {
            return Err(invalid(
                SPEECH,
                format!("`{other}` is not a speech serving mode"),
            ))
        }
    };
    unique_list(
        "model_blocks.speech.languages",
        &wire.languages,
        MAX_LANGUAGES,
        |tag| {
            if is_language_tag(tag) {
                Ok(())
            } else {
                Err(invalid(
                    "model_blocks.speech.languages",
                    format!("`{tag}` is not a language tag"),
                ))
            }
        },
    )?;
    let (voices, input_audio_formats, output_audio_formats) = task_declarations(
        task,
        wire.voices,
        wire.input_audio_formats,
        wire.output_audio_formats,
    )?;
    let chunking_text = object_members(raw, SPEECH)?
        .into_iter()
        .find_map(|(key, text)| (key == "chunking").then_some(text))
        .ok_or_else(|| malformed(SPEECH, "missing field `chunking`"))?;
    let chunking = speech_chunking(task, chunking_text)?;
    check_identifier(
        "model_blocks.speech.algorithm_profile.id",
        &wire.algorithm_profile.id,
        true,
    )?;
    for (field, digest) in [
        (
            "model_blocks.speech.algorithm_profile.digest",
            &wire.algorithm_profile.digest,
        ),
        (
            "model_blocks.speech.quality_profile_digest",
            &wire.quality_profile_digest,
        ),
        (
            "model_blocks.speech.benchmark_profile_digest",
            &wire.benchmark_profile_digest,
        ),
    ] {
        if !is_sha256_digest(digest) {
            return Err(invalid(
                field,
                format!("`{digest}` is not `sha256:` and 64 lowercase hex digits"),
            ));
        }
    }
    Ok(SpeechContract {
        task,
        serving_mode,
        languages: wire.languages,
        voices,
        input_audio_formats,
        output_audio_formats,
        chunking,
        algorithm_profile: ProfileReference {
            id: wire.algorithm_profile.id,
            digest: wire.algorithm_profile.digest,
        },
        quality_profile_digest: wire.quality_profile_digest,
        benchmark_profile_digest: wire.benchmark_profile_digest,
    })
}

type TaskDeclarations = (Vec<String>, Vec<AudioFormat>, Vec<AudioFormat>);

/// The voices, input formats and output formats a task declares.
fn task_declarations(
    task: SpeechTask,
    voices: Option<Vec<String>>,
    input_audio_formats: Option<Vec<AudioFormatWire>>,
    output_audio_formats: Option<Vec<AudioFormatWire>>,
) -> Result<TaskDeclarations, BundleProfileError> {
    match task {
        SpeechTask::Stt => {
            if voices.is_some() || output_audio_formats.is_some() {
                return Err(invalid(
                    SPEECH,
                    "an STT contract declares no voices or output audio",
                ));
            }
            let inputs = audio_formats(
                "model_blocks.speech.input_audio_formats",
                input_audio_formats,
                &STT_INPUT_AUDIO_FORMATS,
            )?;
            if !inputs.contains(&STT_INPUT_AUDIO_FORMATS[0]) {
                return Err(invalid(
                    "model_blocks.speech.input_audio_formats",
                    "an STT contract accepts 16 kHz mono `pcm_s16le`",
                ));
            }
            Ok((Vec::new(), inputs, Vec::new()))
        }
        SpeechTask::Tts => {
            if input_audio_formats.is_some() {
                return Err(invalid(SPEECH, "a TTS contract declares no input audio"));
            }
            let voices = voices.ok_or_else(|| {
                rule(
                    BundleRuleCode::RequiredField,
                    "model_blocks.speech.voices",
                    "a TTS contract names its voices",
                )
            })?;
            unique_list("model_blocks.speech.voices", &voices, MAX_VOICES, |voice| {
                check_identifier("model_blocks.speech.voices", voice, true)
            })?;
            let outputs = audio_formats(
                "model_blocks.speech.output_audio_formats",
                output_audio_formats,
                &TTS_OUTPUT_AUDIO_FORMATS,
            )?;
            Ok((voices, Vec::new(), outputs))
        }
    }
}

fn audio_formats(
    field: &str,
    declared: Option<Vec<AudioFormatWire>>,
    supported: &[AudioFormat],
) -> Result<Vec<AudioFormat>, BundleProfileError> {
    let declared = declared.ok_or_else(|| {
        rule(
            BundleRuleCode::RequiredField,
            field,
            "is required for this task",
        )
    })?;
    if declared.is_empty() {
        return Err(invalid(field, "must list at least one format"));
    }
    let mut formats = Vec::with_capacity(declared.len());
    for wire in declared {
        let format = supported
            .iter()
            .copied()
            .find(|f| {
                f.encoding.as_str() == wire.encoding
                    && u64::from(f.sample_rate_hz) == wire.sample_rate_hz
                    && u64::from(f.channels) == wire.channels
            })
            .ok_or_else(|| {
                invalid(
                    field,
                    format!(
                        "`{}` at {} Hz with {} channel(s) is not a supported format",
                        wire.encoding, wire.sample_rate_hz, wire.channels
                    ),
                )
            })?;
        if formats.contains(&format) {
            return Err(invalid(field, "lists a format twice"));
        }
        formats.push(format);
    }
    Ok(formats)
}

fn speech_chunking(task: SpeechTask, raw: &str) -> Result<SpeechChunking, BundleProfileError> {
    const FIELD: &str = "model_blocks.speech.chunking";
    match task {
        SpeechTask::Stt => {
            let wire: SttChunkingWire = decode_object(FIELD, raw)?;
            let chunking = SttChunking {
                frame_ms_min: bounded(
                    FIELD,
                    wire.frame_ms_min,
                    STT_FRAME_MS_MIN,
                    STT_FRAME_MS_MAX,
                )?,
                frame_ms_max: bounded(
                    FIELD,
                    wire.frame_ms_max,
                    STT_FRAME_MS_MIN,
                    STT_FRAME_MS_MAX,
                )?,
                max_utterance_ms: bounded(FIELD, wire.max_utterance_ms, 1, STT_MAX_UTTERANCE_MS)?,
            };
            if chunking.frame_ms_min > chunking.frame_ms_max {
                return Err(invalid(FIELD, "`frame_ms_min` exceeds `frame_ms_max`"));
            }
            Ok(SpeechChunking::Stt(chunking))
        }
        SpeechTask::Tts => {
            let wire: TtsChunkingWire = decode_object(FIELD, raw)?;
            let chunking = TtsChunking {
                max_segment_text_bytes: bounded(
                    FIELD,
                    wire.max_segment_text_bytes,
                    1,
                    TTS_MAX_SEGMENT_TEXT_BYTES,
                )?,
                max_segment_phoneme_tokens: bounded(
                    FIELD,
                    wire.max_segment_phoneme_tokens,
                    1,
                    TTS_MAX_SEGMENT_PHONEME_TOKENS,
                )?,
                max_segment_audio_ms: bounded(
                    FIELD,
                    wire.max_segment_audio_ms,
                    1,
                    TTS_MAX_SEGMENT_AUDIO_MS,
                )?,
                max_synthesis_text_bytes: bounded(
                    FIELD,
                    wire.max_synthesis_text_bytes,
                    1,
                    TTS_MAX_SYNTHESIS_TEXT_BYTES,
                )?,
                max_synthesis_audio_ms: bounded(
                    FIELD,
                    wire.max_synthesis_audio_ms,
                    1,
                    TTS_MAX_SYNTHESIS_AUDIO_MS,
                )?,
            };
            if chunking.max_segment_text_bytes > chunking.max_synthesis_text_bytes
                || chunking.max_segment_audio_ms > chunking.max_synthesis_audio_ms
            {
                return Err(invalid(
                    FIELD,
                    "a segment limit exceeds its synthesis limit",
                ));
            }
            Ok(SpeechChunking::Tts(chunking))
        }
    }
}

/// `language[-subtag]*`: a 2–3 letter lowercase primary subtag, then
/// 2–8 character alphanumeric subtags, at most 35 bytes.
fn is_language_tag(tag: &str) -> bool {
    let mut subtags = tag.split('-');
    let primary_ok = subtags
        .next()
        .is_some_and(|p| (2..=3).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_lowercase()));
    primary_ok
        && tag.len() <= MAX_LANGUAGE_TAG_BYTES
        && subtags
            .all(|s| (2..=8).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric()))
}

pub(crate) fn is_sha256_digest(digest: &str) -> bool {
    digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]

    use super::{
        AudioEncoding, AudioFormat, BundleProfile, BundleProfileError, DegradedProfile,
        MODEL_BLOCK_KEYS, RUNTIME_PIPELINE_STAGES, STT_INPUT_AUDIO_FORMATS,
        TTS_OUTPUT_AUDIO_FORMATS,
    };
    use serde_json::Value;

    fn manifest_schema() -> Value {
        serde_json::from_str(include_str!("../../schemas/bundle_manifest.json"))
            .expect("schema parses")
    }

    fn strings(value: &Value) -> Vec<String> {
        value
            .as_array()
            .expect("an array")
            .iter()
            .map(|v| v.as_str().expect("a string").to_string())
            .collect()
    }

    fn formats(value: &Value) -> Vec<AudioFormat> {
        value["enum"]
            .as_array()
            .expect("an enum of objects")
            .iter()
            .map(|f| AudioFormat {
                encoding: match f["encoding"].as_str() {
                    Some("pcm_s16le") => AudioEncoding::PcmS16le,
                    Some("mulaw") => AudioEncoding::Mulaw,
                    other => panic!("unexpected encoding {other:?}"),
                },
                sample_rate_hz: u32::try_from(f["sample_rate_hz"].as_u64().expect("rate"))
                    .expect("u32"),
                channels: u16::try_from(f["channels"].as_u64().expect("channels")).expect("u16"),
            })
            .collect()
    }

    #[test]
    fn enumerations_match_the_schema_that_documents_them() {
        let schema = manifest_schema();
        let defs = &schema["definitions"];
        let stages =
            strings(&defs["pipeline_stage"]["allOf"][0]["then"]["properties"]["stage"]["enum"]);
        assert_eq!(stages, RUNTIME_PIPELINE_STAGES);
        assert_eq!(
            formats(&defs["stt_input_audio_format"]),
            STT_INPUT_AUDIO_FORMATS
        );
        assert_eq!(
            formats(&defs["tts_output_audio_format"]),
            TTS_OUTPUT_AUDIO_FORMATS
        );
        assert_eq!(
            strings(&defs["variant_identity"]["properties"]["variant_kind"]["enum"]),
            super::VariantKind::ALL.map(super::VariantKind::as_str)
        );
        let blocks: Vec<&String> = schema["properties"]["model_blocks"]["properties"]
            .as_object()
            .expect("model_blocks properties")
            .keys()
            .collect();
        let mut expected: Vec<&str> = MODEL_BLOCK_KEYS.to_vec();
        expected.sort_unstable();
        assert_eq!(blocks, expected);

        let descriptor: Value =
            serde_json::from_str(include_str!("../../schemas/backend_descriptor.json"))
                .expect("descriptor schema parses");
        assert_eq!(
            strings(&defs["format_0_2"]["properties"]["compute_type"]["enum"]),
            strings(
                &descriptor["properties"]["runner_profiles"]["items"]["properties"]
                    ["compute_types"]["items"]["enum"]
            ),
            "a bundle's compute type is one a runner profile can declare"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn bounds_match_the_schema_that_documents_them() {
        let schema = manifest_schema();
        let defs = &schema["definitions"];
        let general = &defs["format_0_2"]["properties"];
        let speech = &defs["speech_contract"]["properties"];
        let stage = &defs["pipeline_stage"]["properties"];
        let warmup = &defs["warmup"]["properties"];
        let stt = &defs["stt_chunking"]["properties"];
        let tts = &defs["tts_chunking"]["properties"];
        let n = |v: &Value, key: &str| v[key].as_u64().expect(key);
        let pairs: [(u64, u64, &str); 26] = [
            (
                n(&defs["snake_identifier"], "maxLength"),
                super::MAX_IDENTIFIER_BYTES as u64,
                "identifier length",
            ),
            (
                n(&general["hardware_compatibility"]["items"], "maxLength"),
                super::MAX_IDENTIFIER_BYTES as u64,
                "row id length",
            ),
            (
                n(&general["hardware_compatibility"], "maxItems"),
                super::MAX_HARDWARE_ROWS as u64,
                "row count",
            ),
            (
                n(&general["pipeline_stages"], "maxItems"),
                super::MAX_PIPELINE_STAGES as u64,
                "stage count",
            ),
            (
                n(&general["max_concurrent_sessions"], "maximum"),
                u64::from(crate::member_quota::MAX_MEMBER_SESSIONS),
                "sessions",
            ),
            (
                n(&stage["interface"], "maxLength"),
                super::MAX_STAGE_INTERFACE_CHARS as u64,
                "interface length",
            ),
            (
                n(&warmup["fixtures"], "maxItems"),
                super::MAX_WARMUP_FIXTURES as u64,
                "warmup fixtures",
            ),
            (
                n(&warmup["fixtures"]["items"], "maxLength"),
                super::MAX_WARMUP_FIXTURE_PATH_CHARS as u64,
                "warmup path length",
            ),
            (
                n(&warmup["repetitions"], "maximum"),
                super::MAX_WARMUP_REPETITIONS,
                "repetitions",
            ),
            (
                n(&warmup["timeout_ms"], "maximum"),
                super::MAX_WARMUP_TIMEOUT_MS,
                "warmup timeout",
            ),
            (
                n(&speech["languages"], "maxItems"),
                super::MAX_LANGUAGES as u64,
                "languages",
            ),
            (
                n(&speech["languages"]["items"], "maxLength"),
                super::MAX_LANGUAGE_TAG_BYTES as u64,
                "language tag length",
            ),
            (
                n(&speech["voices"], "maxItems"),
                super::MAX_VOICES as u64,
                "voices",
            ),
            (
                n(&stt["frame_ms_min"], "minimum"),
                super::STT_FRAME_MS_MIN,
                "frame minimum",
            ),
            (
                n(&stt["frame_ms_min"], "maximum"),
                super::STT_FRAME_MS_MAX,
                "frame minimum's ceiling",
            ),
            (
                n(&stt["frame_ms_max"], "minimum"),
                super::STT_FRAME_MS_MIN,
                "frame maximum's floor",
            ),
            (
                n(&stt["frame_ms_max"], "maximum"),
                super::STT_FRAME_MS_MAX,
                "frame maximum",
            ),
            (
                n(&stt["max_utterance_ms"], "maximum"),
                super::STT_MAX_UTTERANCE_MS,
                "utterance",
            ),
            (
                n(&tts["max_segment_text_bytes"], "maximum"),
                super::TTS_MAX_SEGMENT_TEXT_BYTES,
                "segment text",
            ),
            (
                n(&tts["max_segment_phoneme_tokens"], "maximum"),
                super::TTS_MAX_SEGMENT_PHONEME_TOKENS,
                "segment tokens",
            ),
            (
                n(&tts["max_segment_audio_ms"], "maximum"),
                super::TTS_MAX_SEGMENT_AUDIO_MS,
                "segment audio",
            ),
            (
                n(&tts["max_synthesis_text_bytes"], "maximum"),
                super::TTS_MAX_SYNTHESIS_TEXT_BYTES,
                "synthesis text",
            ),
            (
                n(&tts["max_synthesis_audio_ms"], "maximum"),
                super::TTS_MAX_SYNTHESIS_AUDIO_MS,
                "synthesis audio",
            ),
            (n(&stt["max_utterance_ms"], "minimum"), 1, "utterance floor"),
            (
                n(&tts["max_segment_text_bytes"], "minimum"),
                1,
                "segment text floor",
            ),
            (
                n(
                    &defs["variant_identity"]["properties"]["revision"],
                    "maxLength",
                ),
                super::MAX_VARIANT_REVISION_BYTES as u64,
                "variant revision length",
            ),
        ];
        for (schema_value, rust_value, what) in pairs {
            assert_eq!(schema_value, rust_value, "{what}");
        }
    }

    #[test]
    fn duplicate_keys_reject_at_every_level() {
        for raw in [
            r#"{"runner_profile":"kokoro","runner_profile":"faster_whisper"}"#,
            r#"{"model_blocks":{"speech":{},"speech":{}}}"#,
            r#"{"runner_profile":"kokoro","runner_profile":"kokoro"}"#,
        ] {
            let err = BundleProfile::from_manifest_text(raw).expect_err("duplicate key");
            assert!(
                matches!(err, BundleProfileError::DuplicateKey { .. }),
                "{raw}: {err:?}"
            );
        }
        let raw = r#"{"memory_budget_breakdown_bytes":{"model_weights_bytes":1,"model_weights_bytes":2}}"#;
        let err = BundleProfile::from_manifest_text(raw).expect_err("duplicate line");
        assert!(
            matches!(err, BundleProfileError::DuplicateKey { .. }),
            "{err}"
        );
    }

    #[test]
    fn number_lexemes_are_judged_exactly() {
        let raw = r#"{"memory_budget_breakdown_bytes":{"model_weights_bytes":1.0000000000000001}}"#;
        let err = BundleProfile::from_manifest_text(raw).expect_err("fractional lexeme");
        assert!(
            matches!(err, BundleProfileError::InvalidNumber { .. }),
            "{err:?}"
        );
        let raw = r#"{"max_concurrent_sessions":2.0,"memory_budget_breakdown_bytes":{"model_weights_bytes":4096.0}}"#;
        let profile = BundleProfile::from_manifest_text(raw).expect("integral spellings decode");
        assert_eq!(profile.max_concurrent_sessions, Some(2));
        assert_eq!(
            profile
                .memory_budget_breakdown_bytes
                .map(|b| b.model_weights_bytes),
            Some(4096)
        );
    }

    #[test]
    fn array_shaped_budgets_reject() {
        for raw in [
            r#"{"memory_budget_breakdown_bytes":[1]}"#,
            r#"{"memory_budget_by_domain":{"guest_ram":[1]}}"#,
            r#"{"memory_budget_by_domain":[{"model_weights_bytes":1}]}"#,
        ] {
            let err = BundleProfile::from_manifest_text(raw).expect_err("array shape");
            assert!(
                matches!(err, BundleProfileError::Malformed { .. }),
                "{raw}: {err:?}"
            );
        }
    }

    #[test]
    fn present_null_is_not_absence() {
        for raw in [
            r#"{"runner_profile":null}"#,
            r#"{"memory_budget_by_domain":{"guest_ram":null}}"#,
            r#"{"pipeline_stages":[{"stage":"text_prep","ownership":"caller_owned","interface":null}]}"#,
        ] {
            BundleProfile::from_manifest_text(raw).expect_err(raw);
        }
        let profile = BundleProfile::from_manifest_text(r#"{"degraded_profile":null}"#)
            .expect("null disables degradation");
        assert_eq!(profile.degraded_profile, Some(DegradedProfile::Disabled));
        assert_eq!(
            BundleProfile::from_manifest_text("{}").expect("empty"),
            BundleProfile::default()
        );
    }

    #[test]
    fn member_text_survives_strings_escapes_and_nesting() {
        let raw = r#" { "support_level" : "preview" , "x" : "}\"{,]" , "y" : [ {"a": [1, {"b": "]"}]} ] ,
            "max_concurrent_sessions" : 7 } "#;
        let profile = BundleProfile::from_manifest_text(raw).expect("decodes");
        assert_eq!(profile.max_concurrent_sessions, Some(7));
        assert_eq!(profile.support_level, Some(super::SupportLevel::Preview));
    }
}
