"""Sidecar IPC protocol constants.

Mirrors the JSON Schema in ``protocol/schemas/python_pytorch_ipc.json``.
Keep the literals here in sync; the schema is the source of truth.
"""

from __future__ import annotations

from typing import Final

#: The JSON header always carries this field with this value.
SCHEMA_VERSION: Final[str] = "0.1"

# Request kinds
KIND_LOAD_MODEL: Final[str] = "load_model"
KIND_PRIME: Final[str] = "prime"
KIND_INFER: Final[str] = "infer"
KIND_INFER_ASYNC: Final[str] = "infer_async"
KIND_CANCEL: Final[str] = "cancel"
KIND_UNLOAD: Final[str] = "unload"
KIND_HEALTH_CHECK: Final[str] = "health_check"

# Response kinds (each request kind has a matching response kind)
KIND_LOAD_MODEL_RESPONSE: Final[str] = "load_model_response"
KIND_PRIME_RESPONSE: Final[str] = "prime_response"
KIND_INFER_RESPONSE: Final[str] = "infer_response"
KIND_INFER_ASYNC_RESPONSE: Final[str] = "infer_async_response"
KIND_CANCEL_RESPONSE: Final[str] = "cancel_response"
KIND_UNLOAD_RESPONSE: Final[str] = "unload_response"
KIND_HEALTH_CHECK_RESPONSE: Final[str] = "health_check_response"

# Event kinds (unsolicited)
KIND_READY_EVENT: Final[str] = "ready_event"
KIND_ERROR_EVENT: Final[str] = "error_event"
KIND_METRIC_EVENT: Final[str] = "metric_event"

# Job and session kinds, enabled by CAPABILITY_SPEECH_JOBS_V1. The adapter
# sends job_submit, job_cancel and session_release; the sidecar the others.
KIND_JOB_SUBMIT: Final[str] = "job_submit"
KIND_JOB_CANCEL: Final[str] = "job_cancel"
KIND_JOB_ACCEPTED: Final[str] = "job_accepted"
KIND_JOB_PROGRESS: Final[str] = "job_progress"
KIND_JOB_COMPLETED: Final[str] = "job_completed"
KIND_JOB_FAILED: Final[str] = "job_failed"
KIND_JOB_CANCEL_ACKNOWLEDGED: Final[str] = "job_cancel_acknowledged"
KIND_JOB_RELEASED: Final[str] = "job_released"
KIND_SESSION_RELEASE: Final[str] = "session_release"
KIND_SESSION_RELEASED: Final[str] = "session_released"

CAPABILITY_SPEECH_JOBS_V1: Final[str] = "speech_jobs_v1"

JOB_CLASS_STT_DECODE: Final[str] = "stt_decode"
JOB_CLASS_TTS_SYNTHESIS: Final[str] = "tts_synthesis"
JOB_CLASS_VAD_FRAMES: Final[str] = "vad_frames"

JOB_INPUT_AUDIO_FRAMES: Final[str] = "audio_frames"
JOB_INPUT_TEXT_SEGMENT: Final[str] = "text_segment"
JOB_INPUT_VAD_FRAMES: Final[str] = "vad_frames"

JOB_RESULT_TRANSCRIPT: Final[str] = "transcript"
JOB_RESULT_AUDIO_CHUNK: Final[str] = "audio_chunk"
JOB_RESULT_VAD: Final[str] = "vad"

AUDIO_ENCODING_PCM_S16LE: Final[str] = "pcm_s16le"

# Ceilings of the typed job seam, mirrored from protocol/fixtures/job_seam.json.
LIMIT_JOB_INPUT_SAMPLE_RATE_HZ: Final[int] = 16000
LIMIT_JOB_OUTPUT_SAMPLE_RATE_HZ: Final[int] = 24000
LIMIT_AUDIO_FRAMES_MAX_BYTES: Final[int] = 960000
LIMIT_TEXT_SEGMENT_MAX_BYTES: Final[int] = 4096
LIMIT_VAD_FRAMES_MAX_PER_JOB: Final[int] = 32
LIMIT_VAD_FRAMES_MAX_BYTES: Final[int] = 32768
LIMIT_PROGRESS_EVENTS_MAX: Final[int] = 1500
LIMIT_LANGUAGE_TAG_MAX_BYTES: Final[int] = 16
LIMIT_VOICE_ID_MAX_BYTES: Final[int] = 64
LIMIT_TRANSCRIPT_TEXT_MAX_BYTES: Final[int] = 8192
LIMIT_TRANSCRIPT_TOKENS_MAX: Final[int] = 448
LIMIT_AUDIO_CHUNK_MAX_BYTES: Final[int] = 1440000
LIMIT_VAD_PROBABILITIES_MAX: Final[int] = 32
LIMIT_ERROR_TEXT_MAX_BYTES: Final[int] = 512

REQUEST_TO_RESPONSE: Final[dict[str, str]] = {
    KIND_LOAD_MODEL: KIND_LOAD_MODEL_RESPONSE,
    KIND_PRIME: KIND_PRIME_RESPONSE,
    KIND_INFER: KIND_INFER_RESPONSE,
    KIND_INFER_ASYNC: KIND_INFER_ASYNC_RESPONSE,
    KIND_CANCEL: KIND_CANCEL_RESPONSE,
    KIND_UNLOAD: KIND_UNLOAD_RESPONSE,
    KIND_HEALTH_CHECK: KIND_HEALTH_CHECK_RESPONSE,
}

# Status discriminator on response and error-event messages.
STATUS_OK: Final[str] = "ok"
STATUS_ERROR: Final[str] = "error"

# Error codes mirror tensorplate::Error::Code snake_case wire names.
ERR_CONFIG_INVALID: Final[str] = "config_invalid"
ERR_LOAD_FAILED: Final[str] = "load_failed"
ERR_NOT_READY: Final[str] = "not_ready"
ERR_SHAPE_MISMATCH: Final[str] = "shape_mismatch"
ERR_UNSUPPORTED: Final[str] = "unsupported"
ERR_OOM_ERROR: Final[str] = "oom_error"
ERR_TIMEOUT: Final[str] = "timeout"
ERR_INFERENCE_FAILED: Final[str] = "inference_failed"
ERR_INTERNAL: Final[str] = "internal"
ERR_CANCELLED: Final[str] = "cancelled"
ERR_UNAVAILABLE: Final[str] = "unavailable"
ERR_RESOURCE_EXHAUSTED: Final[str] = "resource_exhausted"

# Vendor-neutral platform reason carried by a runtime capability record.
REASON_ACCELERATOR_RUNTIME_UNAVAILABLE: Final[str] = "accelerator_runtime_unavailable"


__all__ = [
    "AUDIO_ENCODING_PCM_S16LE",
    "CAPABILITY_SPEECH_JOBS_V1",
    "ERR_CANCELLED",
    "ERR_CONFIG_INVALID",
    "ERR_INFERENCE_FAILED",
    "ERR_INTERNAL",
    "ERR_LOAD_FAILED",
    "ERR_NOT_READY",
    "ERR_OOM_ERROR",
    "ERR_RESOURCE_EXHAUSTED",
    "ERR_SHAPE_MISMATCH",
    "ERR_TIMEOUT",
    "ERR_UNAVAILABLE",
    "ERR_UNSUPPORTED",
    "JOB_CLASS_STT_DECODE",
    "JOB_CLASS_TTS_SYNTHESIS",
    "JOB_CLASS_VAD_FRAMES",
    "JOB_INPUT_AUDIO_FRAMES",
    "JOB_INPUT_TEXT_SEGMENT",
    "JOB_INPUT_VAD_FRAMES",
    "JOB_RESULT_AUDIO_CHUNK",
    "JOB_RESULT_TRANSCRIPT",
    "JOB_RESULT_VAD",
    "KIND_CANCEL",
    "KIND_CANCEL_RESPONSE",
    "KIND_ERROR_EVENT",
    "KIND_HEALTH_CHECK",
    "KIND_HEALTH_CHECK_RESPONSE",
    "KIND_INFER",
    "KIND_INFER_ASYNC",
    "KIND_INFER_ASYNC_RESPONSE",
    "KIND_INFER_RESPONSE",
    "KIND_JOB_ACCEPTED",
    "KIND_JOB_CANCEL",
    "KIND_JOB_CANCEL_ACKNOWLEDGED",
    "KIND_JOB_COMPLETED",
    "KIND_JOB_FAILED",
    "KIND_JOB_PROGRESS",
    "KIND_JOB_RELEASED",
    "KIND_JOB_SUBMIT",
    "KIND_LOAD_MODEL",
    "KIND_LOAD_MODEL_RESPONSE",
    "KIND_METRIC_EVENT",
    "KIND_PRIME",
    "KIND_PRIME_RESPONSE",
    "KIND_READY_EVENT",
    "KIND_SESSION_RELEASE",
    "KIND_SESSION_RELEASED",
    "KIND_UNLOAD",
    "KIND_UNLOAD_RESPONSE",
    "LIMIT_AUDIO_CHUNK_MAX_BYTES",
    "LIMIT_AUDIO_FRAMES_MAX_BYTES",
    "LIMIT_ERROR_TEXT_MAX_BYTES",
    "LIMIT_JOB_INPUT_SAMPLE_RATE_HZ",
    "LIMIT_JOB_OUTPUT_SAMPLE_RATE_HZ",
    "LIMIT_LANGUAGE_TAG_MAX_BYTES",
    "LIMIT_PROGRESS_EVENTS_MAX",
    "LIMIT_TEXT_SEGMENT_MAX_BYTES",
    "LIMIT_TRANSCRIPT_TEXT_MAX_BYTES",
    "LIMIT_TRANSCRIPT_TOKENS_MAX",
    "LIMIT_VAD_FRAMES_MAX_BYTES",
    "LIMIT_VAD_FRAMES_MAX_PER_JOB",
    "LIMIT_VAD_PROBABILITIES_MAX",
    "LIMIT_VOICE_ID_MAX_BYTES",
    "REASON_ACCELERATOR_RUNTIME_UNAVAILABLE",
    "REQUEST_TO_RESPONSE",
    "SCHEMA_VERSION",
    "STATUS_ERROR",
    "STATUS_OK",
]
