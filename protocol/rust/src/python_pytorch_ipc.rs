// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F07-T06: Rust mirror of `protocol/schemas/python_pytorch_ipc.json`.
//
// The wire format for one IPC message is:
//   `[4-byte big-endian header_length][JSON header][raw tensor payload bytes]`
//
// The Rust mirror covers only the JSON header. C++ tensorplate-serving and
// the Python sidecar share the framing implementation; this module is the
// authoritative schema for the header itself. v0.1.0 does not require the
// Rust agent to speak the IPC protocol, but the schema lives in the
// shared crate so configuration tooling and future Rust-side test harness
// utilities can reuse it.
//
// The job and session messages carry the typed jobs of
// `include/tensorplate/core/job_*.hpp`; their ceilings and validation reasons
// are the ones `protocol/fixtures/job_seam.json` lists.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{ErrorCode, ProtocolError};
use crate::json_numbers::MAX_SAFE_BYTES;
use crate::model_spec::ModelSpec;
use crate::serde_shape::{
    deserialize_map_only, deserialize_some, deserialize_some_map_only, deserialize_vec_map_only,
};
use crate::tensor_view::TensorView;
use crate::{DecodeError, ValidatePayload, SCHEMA_VERSION};

/// Capability that enables the job and session messages on a connection.
pub const CAPABILITY_SPEECH_JOBS_V1: &str = "speech_jobs_v1";
/// Most capabilities one message lists.
pub const MAX_CAPABILITIES: usize = 16;
/// Longest capability name, in bytes.
pub const MAX_CAPABILITY_BYTES: usize = 64;
/// Largest identifier or sample index a job message carries: 2^53 - 1, so
/// every JSON reader represents it exactly.
pub const MAX_JOB_WIRE_INTEGER: u64 = MAX_SAFE_BYTES;
/// Sample rate of job input PCM.
pub const JOB_INPUT_SAMPLE_RATE_HZ: u32 = 16_000;
/// Sample rate of synthesized PCM.
pub const JOB_OUTPUT_SAMPLE_RATE_HZ: u32 = 24_000;
/// Largest `audio_frames` input in bytes: 30 s of input PCM.
pub const MAX_AUDIO_FRAMES_BYTES: u64 = 960_000;
/// Largest `text_segment` input in bytes.
pub const MAX_TEXT_SEGMENT_BYTES: u64 = 4_096;
/// Most frames in one `vad_frames` input.
pub const MAX_VAD_FRAMES_PER_JOB: u32 = 32;
/// Largest `vad_frames` input in bytes.
pub const MAX_VAD_FRAMES_BYTES: u64 = 32_768;
/// Ceiling of `progress_limit` and `progress_sequence`.
pub const MAX_JOB_PROGRESS_EVENTS: u32 = 1_500;
/// Longest language tag in bytes.
pub const MAX_LANGUAGE_TAG_BYTES: usize = 16;
/// Longest voice identifier in bytes.
pub const MAX_VOICE_ID_BYTES: usize = 64;
/// Largest transcript text in bytes; a transcript's word texts together obey
/// the same ceiling.
pub const MAX_TRANSCRIPT_TEXT_BYTES: usize = 8_192;
/// Most tokens in one transcript.
pub const MAX_TRANSCRIPT_TOKENS: usize = 448;
/// Largest `audio_chunk` result in bytes: 30 s of synthesized PCM.
pub const MAX_AUDIO_CHUNK_BYTES: u64 = 1_440_000;
/// Most probabilities in one `vad` result.
pub const MAX_VAD_PROBABILITIES: usize = 32;
/// Largest `job_failed` error message, and context, in bytes.
pub const MAX_JOB_ERROR_TEXT_BYTES: usize = 512;

/// Sidecar IPC message kind. `*_response` messages, and an `error_event`
/// that refuses a request, carry the originating request's `message_id`;
/// other `*_event` messages are unsolicited and carry their own.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcMessageKind {
    LoadModel,
    LoadModelResponse,
    Prime,
    PrimeResponse,
    Infer,
    InferResponse,
    InferAsync,
    InferAsyncResponse,
    Cancel,
    CancelResponse,
    Unload,
    UnloadResponse,
    HealthCheck,
    HealthCheckResponse,
    ReadyEvent,
    ErrorEvent,
    MetricEvent,
    JobSubmit,
    JobCancel,
    JobAccepted,
    JobProgress,
    JobCompleted,
    JobFailed,
    JobCancelAcknowledged,
    JobReleased,
    SessionRelease,
    SessionReleased,
}

impl IpcMessageKind {
    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LoadModel => "load_model",
            Self::LoadModelResponse => "load_model_response",
            Self::Prime => "prime",
            Self::PrimeResponse => "prime_response",
            Self::Infer => "infer",
            Self::InferResponse => "infer_response",
            Self::InferAsync => "infer_async",
            Self::InferAsyncResponse => "infer_async_response",
            Self::Cancel => "cancel",
            Self::CancelResponse => "cancel_response",
            Self::Unload => "unload",
            Self::UnloadResponse => "unload_response",
            Self::HealthCheck => "health_check",
            Self::HealthCheckResponse => "health_check_response",
            Self::ReadyEvent => "ready_event",
            Self::ErrorEvent => "error_event",
            Self::MetricEvent => "metric_event",
            Self::JobSubmit => "job_submit",
            Self::JobCancel => "job_cancel",
            Self::JobAccepted => "job_accepted",
            Self::JobProgress => "job_progress",
            Self::JobCompleted => "job_completed",
            Self::JobFailed => "job_failed",
            Self::JobCancelAcknowledged => "job_cancel_acknowledged",
            Self::JobReleased => "job_released",
            Self::SessionRelease => "session_release",
            Self::SessionReleased => "session_released",
        }
    }

    /// Whether the kind concerns one job and carries its identity.
    #[must_use]
    pub fn is_job(self) -> bool {
        matches!(
            self,
            Self::JobSubmit
                | Self::JobCancel
                | Self::JobAccepted
                | Self::JobProgress
                | Self::JobCompleted
                | Self::JobFailed
                | Self::JobCancelAcknowledged
                | Self::JobReleased
        )
    }

    /// Whether the kind concerns one logical session.
    #[must_use]
    pub fn is_session(self) -> bool {
        matches!(self, Self::SessionRelease | Self::SessionReleased)
    }
}

impl fmt::Display for IpcMessageKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Status discriminator on `*_response` and `error_event` messages.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcStatus {
    Ok,
    Error,
}

/// Descriptor for one tensor's region within the message payload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IpcTensor {
    pub name: String,
    pub tensor: TensorView,
    pub payload_offset: u64,
    pub payload_length: u64,
}

/// Payload of `metric_event` messages.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IpcMetric {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_f64: Option<f64>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub labels: std::collections::BTreeMap<String, String>,
}

impl Eq for IpcMetric {}

/// Payload of successful `health_check_response` messages.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IpcHealth {
    pub ready: bool,
    pub backend_factory: Option<String>,
    pub uptime_ns: u64,
    pub last_error: Option<String>,
}

/// Vendor-neutral accelerator-runtime facts collected by the backend package.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IpcRuntimeCapability {
    pub backend_name: String,
    pub framework_version: String,
    pub accelerator_runtime_version: String,
    pub accelerator_runtime_built: bool,
    pub accelerator_runtime_available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
}

/// Class of a job. It fixes the job's input, options and result.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcJobClass {
    /// `audio_frames` in, `transcript` out.
    SttDecode,
    /// `text_segment` in, `audio_chunk` out.
    TtsSynthesis,
    /// `vad_frames` in, `vad` out.
    VadFrames,
}

/// PCM sample encoding.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum IpcAudioEncoding {
    /// Signed 16-bit little-endian samples.
    #[serde(rename = "pcm_s16le")]
    PcmS16le,
}

/// Format of job PCM.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IpcAudioFormat {
    pub encoding: IpcAudioEncoding,
    pub sample_rate_hz: u32,
    pub channels: u16,
}

/// The only format of `audio_frames` and `vad_frames` inputs.
pub const JOB_INPUT_AUDIO_FORMAT: IpcAudioFormat = IpcAudioFormat {
    encoding: IpcAudioEncoding::PcmS16le,
    sample_rate_hz: JOB_INPUT_SAMPLE_RATE_HZ,
    channels: 1,
};

/// The only format of `audio_chunk` results.
pub const JOB_OUTPUT_AUDIO_FORMAT: IpcAudioFormat = IpcAudioFormat {
    encoding: IpcAudioEncoding::PcmS16le,
    sample_rate_hz: JOB_OUTPUT_SAMPLE_RATE_HZ,
    channels: 1,
};

/// Selections the job's session made; an option the job class does not take
/// is absent.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IpcJobOptions {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub language: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub voice: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub speed_milli: Option<u16>,
}

/// The input of a `job_submit`. Its bytes are the message's whole payload
/// region, `payload_length` long.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum IpcJobInput {
    AudioFrames {
        #[serde(deserialize_with = "deserialize_map_only")]
        format: IpcAudioFormat,
        start_sample: u64,
        payload_length: u64,
    },
    TextSegment {
        payload_length: u64,
    },
    VadFrames {
        #[serde(deserialize_with = "deserialize_map_only")]
        format: IpcAudioFormat,
        frame_samples: u32,
        frame_count: u32,
        utterance_id: u64,
        payload_length: u64,
    },
}

/// One aligned word of a transcript.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IpcWordUnit {
    pub text: String,
    pub token_begin: u32,
    pub token_end: u32,
    pub start_sample: u64,
    pub end_sample: u64,
}

/// A job's result on `job_completed`, or a fragment of it on `job_progress`.
/// An `audio_chunk`'s bytes are the message's whole payload region.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum IpcJobResult {
    Transcript {
        text: String,
        tokens: Vec<u32>,
        #[serde(deserialize_with = "deserialize_vec_map_only")]
        words: Vec<IpcWordUnit>,
    },
    AudioChunk {
        #[serde(deserialize_with = "deserialize_map_only")]
        format: IpcAudioFormat,
        clipped_samples: u32,
        payload_length: u64,
    },
    Vad {
        probabilities: Vec<f64>,
    },
}

/// Mirror of `protocol/schemas/python_pytorch_ipc.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IpcMessage {
    pub schema_version: String,
    pub message_id: String,
    pub kind: IpcMessageKind,

    /// On job messages.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub job_id: Option<u64>,

    /// On job and session messages.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub session_key: Option<u64>,

    /// On job and session messages.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub generation: Option<u64>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub correlation_id: Option<String>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub deadline_ns: Option<u64>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub async_id: Option<u64>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub tensors: Option<Vec<IpcTensor>>,

    /// Present on `LoadModel`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub model_spec: Option<ModelSpec>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub status: Option<IpcStatus>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_strict_error"
    )]
    pub error: Option<ProtocolError>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub runtime_capability: Option<IpcRuntimeCapability>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub metric: Option<IpcMetric>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub health: Option<IpcHealth>,

    /// On `job_submit`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub job_class: Option<IpcJobClass>,

    /// On `job_submit`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub progress_limit: Option<u32>,

    /// On `job_submit`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some_map_only"
    )]
    pub options: Option<IpcJobOptions>,

    /// On `job_submit`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some_map_only"
    )]
    pub input: Option<IpcJobInput>,

    /// On `job_progress`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub progress_sequence: Option<u32>,

    /// On `job_progress` and `job_completed`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some_map_only"
    )]
    pub result: Option<IpcJobResult>,

    /// On `ready_event` and `load_model`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub capabilities: Option<Vec<String>>,

    /// On a successful `load_model_response` to a `load_model` that enabled
    /// [`CAPABILITY_SPEECH_JOBS_V1`].
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub job_classes: Option<Vec<IpcJobClass>>,
}

impl Eq for IpcMessage {}

/// `error.json` as its closed schema reads it: no unknown field, no explicit
/// null and no array form. The shared `ProtocolError` decoder is more lenient.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireError {
    schema_version: String,
    code: ErrorCode,
    message: String,
    #[serde(default, deserialize_with = "deserialize_some")]
    context: Option<String>,
}

fn deserialize_strict_error<'de, D>(deserializer: D) -> Result<Option<ProtocolError>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let wire: WireError = deserialize_map_only(deserializer)?;
    Ok(Some(ProtocolError {
        schema_version: wire.schema_version,
        code: wire.code,
        message: wire.message,
        context: wire.context,
    }))
}

/// Validation errors raised by [`IpcMessage::new_load_model`] and the
/// other typed builders. Mirrors the JSON Schema `allOf` constraints.
#[derive(Debug, thiserror::Error)]
pub enum IpcMessageError {
    #[error("IpcMessage.message_id must be non-empty")]
    EmptyMessageId,
    #[error("LoadModel messages require `model_spec`")]
    LoadModelMissingSpec,
    #[error("error_event messages require `error`")]
    ErrorEventMissingError,
    #[error("metric_event messages require `metric`")]
    MetricEventMissingMetric,
    #[error("IpcMessage.correlation_id, if present, must be non-empty")]
    EmptyCorrelationId,
    #[error("IpcMessage tensors require non-empty `name`")]
    EmptyTensorName,
    #[error("IpcMessage tensor `{0}` must have payload_length > 0")]
    EmptyTensorPayload(String),
    #[error("IpcMessage tensor `{name}` has invalid tensor metadata: {reason}")]
    InvalidTensor { name: String, reason: String },
    #[error("IpcMessage.model_spec is invalid: {0}")]
    InvalidModelSpec(String),
    #[error("IpcMessage.error is invalid: {0}")]
    InvalidError(String),
    #[error("metric_event.metric.name must be non-empty")]
    EmptyMetricName,
    #[error("infer and infer_async messages require correlation_id")]
    InferMissingCorrelationId,
    #[error("infer and infer_async messages require at least one tensor")]
    InferMissingTensors,
    #[error("response messages require `status`")]
    ResponseMissingStatus,
    #[error("error status messages require `error`")]
    ErrorStatusMissingError,
    #[error("error_event messages require status=error")]
    ErrorEventRequiresErrorStatus,
    #[error("successful infer responses require at least one output tensor")]
    InferOkResponseMissingTensors,
    #[error("successful infer_async responses require async_id")]
    InferAsyncOkResponseMissingAsyncId,
    #[error("IpcMessage.async_id, if present, must be > 0")]
    ZeroAsyncId,
    #[error("successful health_check_response messages require health")]
    HealthResponseMissingHealth,
    #[error("health.backend_factory, if present, must be non-empty")]
    EmptyHealthBackendFactory,
    #[error("runtime_capability is invalid: {0}")]
    InvalidRuntimeCapability(String),
    #[error("{kind} messages require `{field}`")]
    MissingField {
        kind: IpcMessageKind,
        field: &'static str,
    },
    #[error("{kind} messages do not carry `{field}`")]
    FieldNotAllowed {
        kind: IpcMessageKind,
        field: &'static str,
    },
    /// A job field the typed job objects refuse, with their stable reason.
    #[error("invalid job message: {0}")]
    InvalidJob(&'static str),
    #[error("`{0}` exceeds the largest integer a job message carries")]
    IntegerOutOfRange(&'static str),
    #[error("capabilities are invalid: {0}")]
    InvalidCapabilities(&'static str),
    #[error("job_classes are invalid: {0}")]
    InvalidJobClasses(&'static str),
}

impl IpcMessage {
    /// A message of `kind` with every optional field absent. Set the fields
    /// the kind carries, then call [`IpcMessage::validate`].
    #[must_use]
    pub fn envelope(message_id: impl Into<String>, kind: IpcMessageKind) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            message_id: message_id.into(),
            kind,
            job_id: None,
            session_key: None,
            generation: None,
            correlation_id: None,
            deadline_ns: None,
            async_id: None,
            tensors: None,
            model_spec: None,
            status: None,
            error: None,
            runtime_capability: None,
            metric: None,
            health: None,
            job_class: None,
            progress_limit: None,
            options: None,
            input: None,
            progress_sequence: None,
            result: None,
            capabilities: None,
            job_classes: None,
        }
    }

    /// Build a `load_model` request with the required `model_spec`.
    ///
    /// # Errors
    ///
    /// Returns [`IpcMessageError::EmptyMessageId`] if `message_id` is empty.
    pub fn new_load_model(
        message_id: impl Into<String>,
        model_spec: ModelSpec,
    ) -> Result<Self, IpcMessageError> {
        let message_id = message_id.into();
        if message_id.is_empty() {
            return Err(IpcMessageError::EmptyMessageId);
        }
        Ok(Self {
            model_spec: Some(model_spec),
            ..Self::envelope(message_id, IpcMessageKind::LoadModel)
        })
    }

    /// Build a generic message and validate the JSON Schema `allOf`
    /// invariants.
    ///
    /// # Errors
    ///
    /// See [`IpcMessageError`].
    pub fn validate(self) -> Result<Self, IpcMessageError> {
        if self.message_id.is_empty() {
            return Err(IpcMessageError::EmptyMessageId);
        }
        if matches!(self.correlation_id.as_deref(), Some("")) {
            return Err(IpcMessageError::EmptyCorrelationId);
        }
        let tensors = self.tensors.map(validate_tensors).transpose()?;
        let model_spec = validate_model_spec(self.model_spec)?;
        let error = validate_error(self.error)?;
        validate_metric(&self.metric)?;
        validate_health(&self.health)?;
        validate_runtime_capability(&self.runtime_capability)?;
        let normalized = Self {
            schema_version: SCHEMA_VERSION.to_string(),
            tensors,
            model_spec,
            error,
            ..self
        };

        validate_kind_invariants(&normalized)?;
        validate_field_placement(&normalized)?;
        validate_capabilities(&normalized)?;
        validate_job_fields(&normalized)?;
        match normalized.kind {
            IpcMessageKind::LoadModel if normalized.model_spec.is_none() => {
                Err(IpcMessageError::LoadModelMissingSpec)
            }
            IpcMessageKind::ErrorEvent if normalized.error.is_none() => {
                Err(IpcMessageError::ErrorEventMissingError)
            }
            IpcMessageKind::MetricEvent if normalized.metric.is_none() => {
                Err(IpcMessageError::MetricEventMissingMetric)
            }
            _ => Ok(normalized),
        }
    }
}

fn validate_tensors(tensors: Vec<IpcTensor>) -> Result<Vec<IpcTensor>, IpcMessageError> {
    let mut validated = Vec::with_capacity(tensors.len());
    for tensor in tensors {
        if tensor.name.is_empty() {
            return Err(IpcMessageError::EmptyTensorName);
        }
        if tensor.payload_length == 0 {
            return Err(IpcMessageError::EmptyTensorPayload(tensor.name));
        }
        let name = tensor.name;
        let metadata =
            tensor
                .tensor
                .validate_payload()
                .map_err(|err| IpcMessageError::InvalidTensor {
                    name: name.clone(),
                    reason: err.to_string(),
                })?;
        validated.push(IpcTensor {
            name,
            tensor: metadata,
            payload_offset: tensor.payload_offset,
            payload_length: tensor.payload_length,
        });
    }
    Ok(validated)
}

fn validate_model_spec(
    model_spec: Option<ModelSpec>,
) -> Result<Option<ModelSpec>, IpcMessageError> {
    model_spec
        .map(ValidatePayload::validate_payload)
        .transpose()
        .map_err(|err| IpcMessageError::InvalidModelSpec(err.to_string()))
}

fn validate_error(error: Option<ProtocolError>) -> Result<Option<ProtocolError>, IpcMessageError> {
    if let Some(error) = &error {
        if error.schema_version != SCHEMA_VERSION {
            return Err(IpcMessageError::InvalidError(format!(
                "schema_version `{}` is not `{SCHEMA_VERSION}`",
                error.schema_version
            )));
        }
    }
    error
        .map(ValidatePayload::validate_payload)
        .transpose()
        .map_err(|err| IpcMessageError::InvalidError(err.to_string()))
}

fn validate_metric(metric: &Option<IpcMetric>) -> Result<(), IpcMessageError> {
    if matches!(metric.as_ref().map(|metric| metric.name.as_str()), Some("")) {
        return Err(IpcMessageError::EmptyMetricName);
    }
    Ok(())
}

fn validate_health(health: &Option<IpcHealth>) -> Result<(), IpcMessageError> {
    if matches!(
        health
            .as_ref()
            .and_then(|health| health.backend_factory.as_deref()),
        Some("")
    ) {
        return Err(IpcMessageError::EmptyHealthBackendFactory);
    }
    Ok(())
}

fn validate_runtime_capability(
    capability: &Option<IpcRuntimeCapability>,
) -> Result<(), IpcMessageError> {
    let Some(capability) = capability else {
        return Ok(());
    };
    if capability.backend_name.is_empty()
        || capability.framework_version.is_empty()
        || capability.accelerator_runtime_version.is_empty()
    {
        return Err(IpcMessageError::InvalidRuntimeCapability(
            "name and version fields must be non-empty".into(),
        ));
    }
    if capability.accelerator_runtime_available && !capability.accelerator_runtime_built {
        return Err(IpcMessageError::InvalidRuntimeCapability(
            "available runtimes require accelerator_runtime_built".into(),
        ));
    }
    match (
        capability.accelerator_runtime_available,
        capability.unavailable_reason.as_deref(),
    ) {
        (true, None) | (false, Some("accelerator_runtime_unavailable")) => Ok(()),
        (true, Some(_)) => Err(IpcMessageError::InvalidRuntimeCapability(
            "available runtimes must not carry unavailable_reason".into(),
        )),
        (false, _) => Err(IpcMessageError::InvalidRuntimeCapability(
            "unavailable runtimes require accelerator_runtime_unavailable".into(),
        )),
    }
}

fn validate_kind_invariants(message: &IpcMessage) -> Result<(), IpcMessageError> {
    let no_tensors = message.tensors.as_ref().map_or(true, Vec::is_empty);
    if message.async_id == Some(0) {
        return Err(IpcMessageError::ZeroAsyncId);
    }
    if matches!(
        message.kind,
        IpcMessageKind::Infer | IpcMessageKind::InferAsync
    ) && message.correlation_id.is_none()
    {
        return Err(IpcMessageError::InferMissingCorrelationId);
    }
    if matches!(
        message.kind,
        IpcMessageKind::Infer | IpcMessageKind::InferAsync
    ) && no_tensors
    {
        return Err(IpcMessageError::InferMissingTensors);
    }
    if is_response_kind(message.kind) && message.status.is_none() {
        return Err(IpcMessageError::ResponseMissingStatus);
    }
    if matches!(message.status, Some(IpcStatus::Error)) && message.error.is_none() {
        return Err(IpcMessageError::ErrorStatusMissingError);
    }
    if message.kind == IpcMessageKind::ErrorEvent && message.status != Some(IpcStatus::Error) {
        return Err(IpcMessageError::ErrorEventRequiresErrorStatus);
    }
    if matches!(
        (message.kind, message.status),
        (
            IpcMessageKind::InferResponse | IpcMessageKind::InferAsyncResponse,
            Some(IpcStatus::Ok)
        )
    ) && no_tensors
    {
        return Err(IpcMessageError::InferOkResponseMissingTensors);
    }
    if matches!(
        (message.kind, message.status),
        (IpcMessageKind::InferAsyncResponse, Some(IpcStatus::Ok))
    ) && message.async_id.is_none()
    {
        return Err(IpcMessageError::InferAsyncOkResponseMissingAsyncId);
    }
    if matches!(
        (message.kind, message.status),
        (IpcMessageKind::HealthCheckResponse, Some(IpcStatus::Ok))
    ) && message.health.is_none()
    {
        return Err(IpcMessageError::HealthResponseMissingHealth);
    }
    Ok(())
}

fn is_response_kind(kind: IpcMessageKind) -> bool {
    matches!(
        kind,
        IpcMessageKind::LoadModelResponse
            | IpcMessageKind::PrimeResponse
            | IpcMessageKind::InferResponse
            | IpcMessageKind::InferAsyncResponse
            | IpcMessageKind::CancelResponse
            | IpcMessageKind::UnloadResponse
            | IpcMessageKind::HealthCheckResponse
    )
}

/// Each job-message field on the kinds that carry it and nowhere else, and
/// job and session messages free of the unary fields.
fn validate_field_placement(message: &IpcMessage) -> Result<(), IpcMessageError> {
    let kind = message.kind;
    let job = kind.is_job();
    let scoped = job || kind.is_session();
    let submit = kind == IpcMessageKind::JobSubmit;
    let progress = kind == IpcMessageKind::JobProgress;
    let with_result = progress || kind == IpcMessageKind::JobCompleted;
    let advertises = matches!(kind, IpcMessageKind::ReadyEvent | IpcMessageKind::LoadModel);
    let loaded = kind == IpcMessageKind::LoadModelResponse && message.status == Some(IpcStatus::Ok);
    let failed = kind == IpcMessageKind::JobFailed;
    // (field, present, allowed on this kind, required on this kind)
    let fields = [
        ("job_id", message.job_id.is_some(), job, job),
        ("session_key", message.session_key.is_some(), scoped, scoped),
        ("generation", message.generation.is_some(), scoped, scoped),
        ("job_class", message.job_class.is_some(), submit, submit),
        (
            "progress_limit",
            message.progress_limit.is_some(),
            submit,
            submit,
        ),
        ("options", message.options.is_some(), submit, submit),
        ("input", message.input.is_some(), submit, submit),
        (
            "progress_sequence",
            message.progress_sequence.is_some(),
            progress,
            progress,
        ),
        ("result", message.result.is_some(), with_result, with_result),
        (
            "capabilities",
            message.capabilities.is_some(),
            advertises,
            false,
        ),
        ("job_classes", message.job_classes.is_some(), loaded, false),
        ("error", message.error.is_some(), !scoped || failed, failed),
        (
            "correlation_id",
            message.correlation_id.is_some(),
            !scoped,
            false,
        ),
        ("deadline_ns", message.deadline_ns.is_some(), !scoped, false),
        ("async_id", message.async_id.is_some(), !scoped, false),
        ("tensors", message.tensors.is_some(), !scoped, false),
        ("model_spec", message.model_spec.is_some(), !scoped, false),
        ("status", message.status.is_some(), !scoped, false),
        (
            "runtime_capability",
            message.runtime_capability.is_some(),
            !scoped,
            false,
        ),
        ("metric", message.metric.is_some(), !scoped, false),
        ("health", message.health.is_some(), !scoped, false),
    ];
    for (field, present, allowed, required) in fields {
        if present && !allowed {
            return Err(IpcMessageError::FieldNotAllowed { kind, field });
        }
        if required && !present {
            return Err(IpcMessageError::MissingField { kind, field });
        }
    }
    Ok(())
}

fn validate_capabilities(message: &IpcMessage) -> Result<(), IpcMessageError> {
    if let Some(names) = &message.capabilities {
        if names.is_empty() {
            return Err(IpcMessageError::InvalidCapabilities("empty"));
        }
        if names.len() > MAX_CAPABILITIES {
            return Err(IpcMessageError::InvalidCapabilities("too_many"));
        }
        for (index, name) in names.iter().enumerate() {
            if !is_capability_name(name) {
                return Err(IpcMessageError::InvalidCapabilities("malformed"));
            }
            if names[..index].contains(name) {
                return Err(IpcMessageError::InvalidCapabilities("duplicate"));
            }
        }
    }
    if let Some(classes) = &message.job_classes {
        if classes.is_empty() {
            return Err(IpcMessageError::InvalidJobClasses("empty"));
        }
        for (index, class) in classes.iter().enumerate() {
            if classes[..index].contains(class) {
                return Err(IpcMessageError::InvalidJobClasses("duplicate"));
            }
        }
    }
    Ok(())
}

fn is_capability_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() <= MAX_CAPABILITY_BYTES
        && bytes.first().is_some_and(u8::is_ascii_lowercase)
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
}

/// Job and session content, checked in the order of the typed job objects so
/// a message fails with the reason they would give.
fn validate_job_fields(message: &IpcMessage) -> Result<(), IpcMessageError> {
    if !(message.kind.is_job() || message.kind.is_session()) {
        return Ok(());
    }
    check_identity(message.job_id, "job_id_zero", "job_id")?;
    check_identity(message.session_key, "session_key_zero", "session_key")?;
    check_identity(message.generation, "generation_zero", "generation")?;
    if let (Some(class), Some(limit), Some(options), Some(input)) = (
        message.job_class,
        message.progress_limit,
        &message.options,
        &message.input,
    ) {
        validate_submit(class, limit, options, input)?;
    }
    if let Some(sequence) = message.progress_sequence {
        if sequence == 0 {
            return invalid_job("progress_sequence_zero");
        }
        if sequence > MAX_JOB_PROGRESS_EVENTS {
            return invalid_job("progress_sequence_too_large");
        }
    }
    if let Some(result) = &message.result {
        validate_result(result)?;
    }
    if let (IpcMessageKind::JobFailed, Some(error)) = (message.kind, &message.error) {
        if error.message.len() > MAX_JOB_ERROR_TEXT_BYTES
            || error
                .context
                .as_ref()
                .is_some_and(|context| context.len() > MAX_JOB_ERROR_TEXT_BYTES)
        {
            return invalid_job("error_text_too_large");
        }
    }
    Ok(())
}

fn invalid_job(reason: &'static str) -> Result<(), IpcMessageError> {
    Err(IpcMessageError::InvalidJob(reason))
}

fn check_identity(
    value: Option<u64>,
    zero: &'static str,
    field: &'static str,
) -> Result<(), IpcMessageError> {
    match value {
        Some(0) => invalid_job(zero),
        Some(value) => check_wire_integer(value, field),
        None => Ok(()),
    }
}

fn check_wire_integer(value: u64, field: &'static str) -> Result<(), IpcMessageError> {
    if value > MAX_JOB_WIRE_INTEGER {
        return Err(IpcMessageError::IntegerOutOfRange(field));
    }
    Ok(())
}

fn validate_submit(
    class: IpcJobClass,
    progress_limit: u32,
    options: &IpcJobOptions,
    input: &IpcJobInput,
) -> Result<(), IpcMessageError> {
    let input_matches_class = matches!(
        (class, input),
        (IpcJobClass::SttDecode, IpcJobInput::AudioFrames { .. })
            | (IpcJobClass::TtsSynthesis, IpcJobInput::TextSegment { .. })
            | (IpcJobClass::VadFrames, IpcJobInput::VadFrames { .. })
    );
    if !input_matches_class {
        return invalid_job("payload_class_mismatch");
    }
    if progress_limit > MAX_JOB_PROGRESS_EVENTS {
        return invalid_job("progress_limit_too_large");
    }
    validate_options(class, options)?;
    validate_input(input)
}

fn validate_options(class: IpcJobClass, options: &IpcJobOptions) -> Result<(), IpcMessageError> {
    let (takes_language, takes_voice, takes_speed) = match class {
        IpcJobClass::SttDecode => (true, false, false),
        IpcJobClass::TtsSynthesis => (true, true, true),
        IpcJobClass::VadFrames => (false, false, false),
    };
    if (!takes_language && options.language.is_some())
        || (!takes_voice && options.voice.is_some())
        || (!takes_speed && options.speed_milli.is_some())
    {
        return invalid_job("option_not_applicable");
    }
    if takes_language && !options.language.as_deref().is_some_and(is_language_tag) {
        return invalid_job("language_invalid");
    }
    if takes_voice && !options.voice.as_deref().is_some_and(is_voice_id) {
        return invalid_job("voice_invalid");
    }
    if takes_speed && options.speed_milli.map_or(true, |speed| speed == 0) {
        return invalid_job("speed_invalid");
    }
    Ok(())
}

/// Two or three ASCII letters, then subtags of `-` and one to eight ASCII
/// letters or digits.
fn is_language_tag(tag: &str) -> bool {
    let mut parts = tag.split('-');
    let primary_ok = parts.next().is_some_and(|primary| {
        (2..=3).contains(&primary.len()) && primary.bytes().all(|b| b.is_ascii_alphabetic())
    });
    tag.len() <= MAX_LANGUAGE_TAG_BYTES
        && primary_ok
        && parts.all(|subtag| {
            (1..=8).contains(&subtag.len()) && subtag.bytes().all(|b| b.is_ascii_alphanumeric())
        })
}

fn is_voice_id(voice: &str) -> bool {
    (1..=MAX_VOICE_ID_BYTES).contains(&voice.len())
        && voice
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

fn validate_pcm_window(
    format: IpcAudioFormat,
    expected: IpcAudioFormat,
    payload_length: u64,
) -> Result<(), IpcMessageError> {
    if format != expected {
        return invalid_job("audio_format_unsupported");
    }
    if payload_length == 0 {
        return invalid_job("pcm_window_empty");
    }
    if payload_length % 2 != 0 {
        return invalid_job("pcm_misaligned");
    }
    Ok(())
}

fn validate_input(input: &IpcJobInput) -> Result<(), IpcMessageError> {
    match *input {
        IpcJobInput::AudioFrames {
            format,
            start_sample,
            payload_length,
        } => {
            validate_pcm_window(format, JOB_INPUT_AUDIO_FORMAT, payload_length)?;
            if payload_length > MAX_AUDIO_FRAMES_BYTES {
                return invalid_job("pcm_too_large");
            }
            check_wire_integer(start_sample, "start_sample")
        }
        IpcJobInput::TextSegment { payload_length } => {
            if payload_length == 0 {
                return invalid_job("text_empty");
            }
            if payload_length > MAX_TEXT_SEGMENT_BYTES {
                return invalid_job("text_too_large");
            }
            Ok(())
        }
        IpcJobInput::VadFrames {
            format,
            frame_samples,
            frame_count,
            utterance_id,
            payload_length,
        } => {
            validate_pcm_window(format, JOB_INPUT_AUDIO_FORMAT, payload_length)?;
            if frame_count == 0 || frame_count > MAX_VAD_FRAMES_PER_JOB {
                return invalid_job("vad_frame_count_out_of_range");
            }
            if payload_length > MAX_VAD_FRAMES_BYTES {
                return invalid_job("pcm_too_large");
            }
            if u64::from(frame_count) * u64::from(frame_samples) * 2 != payload_length {
                return invalid_job("vad_frame_bytes_mismatch");
            }
            check_identity(Some(utterance_id), "utterance_id_zero", "utterance_id")
        }
    }
}

fn validate_result(result: &IpcJobResult) -> Result<(), IpcMessageError> {
    match result {
        IpcJobResult::Transcript {
            text,
            tokens,
            words,
        } => validate_transcript(text, tokens, words),
        IpcJobResult::AudioChunk {
            format,
            clipped_samples,
            payload_length,
        } => {
            validate_pcm_window(*format, JOB_OUTPUT_AUDIO_FORMAT, *payload_length)?;
            if *payload_length > MAX_AUDIO_CHUNK_BYTES {
                return invalid_job("pcm_too_large");
            }
            if u64::from(*clipped_samples) > payload_length / 2 {
                return invalid_job("clipped_samples_out_of_range");
            }
            Ok(())
        }
        IpcJobResult::Vad { probabilities } => {
            if probabilities.is_empty() || probabilities.len() > MAX_VAD_PROBABILITIES {
                return invalid_job("vad_probability_count_out_of_range");
            }
            if probabilities.iter().any(|p| !(0.0..=1.0).contains(p)) {
                return invalid_job("vad_probability_out_of_range");
            }
            Ok(())
        }
    }
}

fn validate_transcript(
    text: &str,
    tokens: &[u32],
    words: &[IpcWordUnit],
) -> Result<(), IpcMessageError> {
    if text.len() > MAX_TRANSCRIPT_TEXT_BYTES {
        return invalid_job("text_too_large");
    }
    if tokens.len() > MAX_TRANSCRIPT_TOKENS {
        return invalid_job("token_count_exceeded");
    }
    let mut word_bytes = 0;
    let mut previous: Option<&IpcWordUnit> = None;
    for word in words {
        if word.text.is_empty() {
            return invalid_job("word_text_empty");
        }
        word_bytes += word.text.len();
        if word_bytes > MAX_TRANSCRIPT_TEXT_BYTES {
            return invalid_job("text_too_large");
        }
        if word.token_begin >= word.token_end {
            return invalid_job("word_token_interval_empty");
        }
        if usize::try_from(word.token_end).map_or(true, |end| end > tokens.len()) {
            return invalid_job("word_token_interval_out_of_range");
        }
        if previous.is_some_and(|previous| word.token_begin < previous.token_end) {
            return invalid_job("word_token_interval_overlap");
        }
        if word.start_sample > word.end_sample {
            return invalid_job("word_sample_interval_reversed");
        }
        if previous.is_some_and(|previous| word.start_sample < previous.start_sample) {
            return invalid_job("word_sample_order");
        }
        check_wire_integer(word.end_sample, "end_sample")?;
        previous = Some(word);
    }
    Ok(())
}

impl ValidatePayload for IpcMessage {
    fn validate_payload(self) -> Result<Self, DecodeError> {
        self.validate()
            .map_err(|err| DecodeError::InvalidPayload(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{
        IpcHealth, IpcMessage, IpcMessageError, IpcMessageKind, IpcMetric, IpcRuntimeCapability,
        IpcStatus, IpcTensor, SCHEMA_VERSION,
    };
    use crate::decode_with_version_check;
    use crate::error::{ErrorCode, ProtocolError};
    use crate::model_spec::{ModelClass, ModelSpec, PrecisionHint};
    use crate::tensor_view::{DType, Layout, TensorView};

    fn sample_spec() -> ModelSpec {
        ModelSpec::new(
            "smolvla-450m",
            ModelClass::Vla,
            "models/smolvla.pt",
            "python_pytorch",
            PrecisionHint::Auto,
            None,
        )
        .expect("spec")
    }

    fn sample_tensor(name: &str) -> IpcTensor {
        IpcTensor {
            name: name.into(),
            tensor: TensorView::new(DType::Float32, vec![16, 7], Layout::RowMajor, 0, 0)
                .expect("view"),
            payload_offset: 0,
            payload_length: 16 * 7 * 4,
        }
    }

    #[test]
    fn load_model_round_trips() {
        let m = IpcMessage::new_load_model("msg-1", sample_spec()).expect("valid");
        let json = serde_json::to_string(&m).expect("serialize");
        let back: IpcMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(m, back);
        assert_eq!(back.schema_version, SCHEMA_VERSION);
        assert_eq!(back.kind, IpcMessageKind::LoadModel);
    }

    #[test]
    fn infer_request_with_tensor_manifest_round_trips() {
        let view = TensorView::new(DType::Float16, vec![1, 3, 224, 224], Layout::RowMajor, 0, 0)
            .expect("view");
        let m = IpcMessage {
            correlation_id: Some("req-7".into()),
            deadline_ns: Some(1_700_000_000_000),
            tensors: Some(vec![IpcTensor {
                name: "image".into(),
                tensor: view,
                payload_offset: 0,
                payload_length: 3 * 224 * 224 * 2,
            }]),
            ..IpcMessage::envelope("msg-2", IpcMessageKind::Infer)
        };
        let json = serde_json::to_string(&m).expect("serialize");
        let back: IpcMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(m, back);
        let tensors = back.tensors.expect("tensors");
        assert_eq!(tensors.len(), 1);
        assert_eq!(tensors[0].payload_length, 3 * 224 * 224 * 2);
    }

    #[test]
    fn infer_response_with_status_ok_round_trips() {
        let m = IpcMessage {
            correlation_id: Some("req-7".into()),
            tensors: Some(vec![sample_tensor("action_chunk")]),
            status: Some(IpcStatus::Ok),
            ..IpcMessage::envelope("msg-2", IpcMessageKind::InferResponse)
        }
        .validate()
        .expect("valid");
        let json = serde_json::to_string(&m).expect("serialize");
        let back: IpcMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(m, back);
    }

    #[test]
    fn infer_async_response_requires_async_id() {
        let bad = IpcMessage {
            correlation_id: Some("req-7".into()),
            tensors: Some(vec![sample_tensor("action_chunk")]),
            status: Some(IpcStatus::Ok),
            ..IpcMessage::envelope("msg-async", IpcMessageKind::InferAsyncResponse)
        }
        .validate();
        assert!(matches!(
            bad,
            Err(IpcMessageError::InferAsyncOkResponseMissingAsyncId)
        ));
    }

    #[test]
    fn health_check_response_round_trips() {
        let m = IpcMessage {
            status: Some(IpcStatus::Ok),
            runtime_capability: Some(IpcRuntimeCapability {
                backend_name: "python_pytorch".into(),
                framework_version: "2.9.1".into(),
                accelerator_runtime_version: "26.0".into(),
                accelerator_runtime_built: true,
                accelerator_runtime_available: true,
                unavailable_reason: None,
            }),
            health: Some(IpcHealth {
                ready: true,
                backend_factory: Some("fixture".into()),
                uptime_ns: 42,
                last_error: None,
            }),
            ..IpcMessage::envelope("health-1", IpcMessageKind::HealthCheckResponse)
        }
        .validate()
        .expect("valid");
        let json = serde_json::to_string(&m).expect("serialize");
        let back: IpcMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(m, back);
    }

    #[test]
    fn unavailable_runtime_requires_typed_reason() {
        let raw = r#"{
            "schema_version":"0.1",
            "message_id":"health-2",
            "kind":"health_check_response",
            "status":"ok",
            "runtime_capability":{
                "backend_name":"python_pytorch",
                "framework_version":"2.9.1",
                "accelerator_runtime_version":"26.0",
                "accelerator_runtime_built":true,
                "accelerator_runtime_available":false
            },
            "health":{
                "ready":false,
                "backend_factory":null,
                "uptime_ns":42,
                "last_error":"runtime unavailable"
            }
        }"#;
        let message: IpcMessage = serde_json::from_str(raw).expect("shape decodes");
        assert!(matches!(
            message.validate(),
            Err(IpcMessageError::InvalidRuntimeCapability(_))
        ));
    }

    #[test]
    fn error_event_round_trips() {
        let m = IpcMessage {
            status: Some(IpcStatus::Error),
            error: Some(ProtocolError::new(ErrorCode::LoadFailed, "weights missing")),
            ..IpcMessage::envelope("evt-1", IpcMessageKind::ErrorEvent)
        }
        .validate()
        .expect("valid");
        let json = serde_json::to_string(&m).expect("serialize");
        let back: IpcMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(m, back);
    }

    #[test]
    fn metric_event_round_trips() {
        let mut labels = std::collections::BTreeMap::new();
        labels.insert("backend".into(), "python_pytorch".into());
        let m = IpcMessage {
            metric: Some(IpcMetric {
                name: "infer_latency_ms".into(),
                value_f64: Some(8.5),
                labels,
            }),
            ..IpcMessage::envelope("evt-2", IpcMessageKind::MetricEvent)
        }
        .validate()
        .expect("valid");
        let json = serde_json::to_string(&m).expect("serialize");
        let back: IpcMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(m, back);
    }

    #[test]
    fn validate_enforces_load_model_requires_spec() {
        let bad = IpcMessage::envelope("msg", IpcMessageKind::LoadModel).validate();
        assert!(matches!(bad, Err(IpcMessageError::LoadModelMissingSpec)));
    }

    #[test]
    fn validate_rejects_empty_message_id() {
        let bad = IpcMessage::envelope(String::new(), IpcMessageKind::HealthCheck).validate();
        assert!(matches!(bad, Err(IpcMessageError::EmptyMessageId)));
    }

    #[test]
    fn version_check_decoder_rejects_old_schema() {
        let json = r#"{"schema_version":"0.0","message_id":"m","kind":"health_check"}"#;
        let err = decode_with_version_check::<IpcMessage>(json).expect_err("rejected");
        assert!(matches!(
            err,
            crate::DecodeError::UnsupportedSchemaVersion { .. }
        ));
    }

    #[test]
    fn version_check_decoder_rejects_current_schema_load_model_without_spec() {
        let json = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","message_id":"msg","kind":"load_model"}}"#
        );
        let err = decode_with_version_check::<IpcMessage>(&json).expect_err("rejected");
        assert!(matches!(err, crate::DecodeError::InvalidPayload(_)));
    }

    #[test]
    fn version_check_decoder_rejects_current_schema_infer_response_without_tensors() {
        let json = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","message_id":"msg","kind":"infer_response","status":"ok"}}"#
        );
        let err = decode_with_version_check::<IpcMessage>(&json).expect_err("rejected");
        assert!(matches!(err, crate::DecodeError::InvalidPayload(_)));
    }
}
