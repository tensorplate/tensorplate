// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F04: Rust mirror of `protocol/schemas/infer_result.json` and the
// C++ `tensorplate::InferResult` value object.

use serde::{Deserialize, Serialize};

use crate::buffer_ref::BufferRef;
use crate::error::ProtocolError;
use crate::tensor_view::TensorView;
use crate::{DecodeError, ValidatePayload, SCHEMA_VERSION};

/// One named output binding: stable name plus payload buffer plus tensor
/// metadata, with an optional semantic tag.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NamedOutput {
    pub name: String,
    pub buffer: BufferRef,
    pub tensor: TensorView,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_tag: Option<String>,
}

/// Latency breakdown in nanoseconds. All fields optional; populated by the
/// ExecutionSession NVI wrapper in V01-E04.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct InferenceTiming {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_latency_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_latency_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_latency_ns: Option<u64>,
}

impl InferenceTiming {
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.queue_latency_ns.is_none()
            && self.execution_latency_ns.is_none()
            && self.total_latency_ns.is_none()
    }
}

/// Result discriminator. Mirrors the JSON Schema `status` enum.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferResultStatus {
    Success,
    Failure,
}

/// Mirror of `tensorplate::InferResult` (C++) and
/// `protocol/schemas/infer_result.json`.
///
/// On the wire `status` discriminates between success (`outputs` set) and
/// failure (`error` set); the validating constructor enforces the same
/// invariant.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InferResult {
    pub schema_version: String,
    pub request_id: String,
    pub status: InferResultStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<NamedOutput>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ProtocolError>,
    #[serde(default, skip_serializing_if = "InferenceTiming::is_default")]
    pub timing: InferenceTiming,
}

/// Validation errors raised by [`InferResult::success`]. Mirrors the C++
/// `InferResult::create_success` rules.
#[derive(Debug, thiserror::Error)]
pub enum InferResultError {
    #[error("InferResult success must carry at least one named output")]
    EmptyOutputs,
    #[error("InferResult.outputs entry has empty `name`")]
    EmptyOutputName,
    #[error("InferResult.outputs has duplicate name `{0}`")]
    DuplicateOutputName(String),
    #[error("InferResult.outputs entry `{0}` has empty `semantic_tag`")]
    EmptySemanticTag(String),
    #[error("InferResult output `{name}` has invalid buffer: {reason}")]
    InvalidOutputBuffer { name: String, reason: String },
    #[error("InferResult output `{name}` has invalid tensor: {reason}")]
    InvalidOutputTensor { name: String, reason: String },
}

impl InferResult {
    /// Build a success result.
    ///
    /// # Errors
    ///
    /// See [`InferResultError`].
    pub fn success(
        request_id: impl Into<String>,
        outputs: Vec<NamedOutput>,
        timing: InferenceTiming,
    ) -> Result<Self, InferResultError> {
        if outputs.is_empty() {
            return Err(InferResultError::EmptyOutputs);
        }
        let mut validated_outputs = Vec::with_capacity(outputs.len());
        let mut seen: std::collections::HashSet<String> =
            std::collections::HashSet::with_capacity(outputs.len());
        for o in outputs {
            if o.name.is_empty() {
                return Err(InferResultError::EmptyOutputName);
            }
            if !seen.insert(o.name.clone()) {
                return Err(InferResultError::DuplicateOutputName(o.name.clone()));
            }
            if matches!(o.semantic_tag.as_deref(), Some("")) {
                return Err(InferResultError::EmptySemanticTag(o.name));
            }
            let name = o.name;
            let buffer = o.buffer.validate_payload().map_err(|err| {
                InferResultError::InvalidOutputBuffer {
                    name: name.clone(),
                    reason: err.to_string(),
                }
            })?;
            let tensor = o.tensor.validate_payload().map_err(|err| {
                InferResultError::InvalidOutputTensor {
                    name: name.clone(),
                    reason: err.to_string(),
                }
            })?;
            validated_outputs.push(NamedOutput {
                name,
                buffer,
                tensor,
                semantic_tag: o.semantic_tag,
            });
        }
        Ok(Self {
            schema_version: SCHEMA_VERSION.to_string(),
            request_id: request_id.into(),
            status: InferResultStatus::Success,
            outputs: Some(validated_outputs),
            error: None,
            timing,
        })
    }

    /// Build a failure result. Failure construction never fails: an
    /// empty request_id is allowed for ingress-time errors.
    #[must_use]
    pub fn failure(
        request_id: impl Into<String>,
        error: ProtocolError,
        timing: InferenceTiming,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            request_id: request_id.into(),
            status: InferResultStatus::Failure,
            outputs: None,
            error: Some(error),
            timing,
        }
    }

    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self.status, InferResultStatus::Success)
    }
}

impl ValidatePayload for InferResult {
    fn validate_payload(self) -> Result<Self, DecodeError> {
        match self.status {
            InferResultStatus::Success => {
                if self.error.is_some() {
                    return Err(DecodeError::InvalidPayload(
                        "InferResult success must not carry `error`".into(),
                    ));
                }
                Self::success(
                    self.request_id,
                    self.outputs.ok_or_else(|| {
                        DecodeError::InvalidPayload(
                            "InferResult success must carry at least one named output".into(),
                        )
                    })?,
                    self.timing,
                )
                .map_err(|err| DecodeError::InvalidPayload(err.to_string()))
            }
            InferResultStatus::Failure => {
                if self.outputs.is_some() {
                    return Err(DecodeError::InvalidPayload(
                        "InferResult failure must not carry `outputs`".into(),
                    ));
                }
                let error = self.error.ok_or_else(|| {
                    DecodeError::InvalidPayload("InferResult failure must carry `error`".into())
                })?;
                Ok(Self::failure(
                    self.request_id,
                    error.validate_payload()?,
                    self.timing,
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{
        InferResult, InferResultError, InferResultStatus, InferenceTiming, NamedOutput,
        SCHEMA_VERSION,
    };
    use crate::buffer_ref::{BufferOwnership, BufferRef};
    use crate::decode_with_version_check;
    use crate::error::{ErrorCode, ProtocolError};
    use crate::tensor_view::{DType, Layout, TensorView};

    fn vla_chunk_output() -> NamedOutput {
        NamedOutput {
            name: "action_chunk".into(),
            // chunk_size = 16, action_dim = 7, dtype f32 -> 16 * 7 * 4 = 448 bytes
            buffer: BufferRef::new(101, 16 * 7 * 4, BufferOwnership::Owned).expect("buf"),
            tensor: TensorView::new(DType::Float32, vec![16, 7], Layout::RowMajor, 0, 0)
                .expect("view"),
            semantic_tag: Some("action_chunk".into()),
        }
    }

    #[test]
    fn success_round_trip_preserves_chunk_shape() {
        let r = InferResult::success(
            "req-1",
            vec![vla_chunk_output()],
            InferenceTiming::default(),
        )
        .expect("valid success");
        let json = serde_json::to_string(&r).expect("serialize");
        let back: InferResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(r, back);
        assert!(back.is_success());
        let outputs = back.outputs.as_ref().expect("present");
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].tensor.shape, vec![16, 7]);
    }

    #[test]
    fn failure_round_trip_preserves_typed_error() {
        let err = ProtocolError::new(ErrorCode::Timeout, "deadline exceeded");
        let r = InferResult::failure(
            "req-2",
            err.clone(),
            InferenceTiming {
                queue_latency_ns: Some(2_000_000),
                ..Default::default()
            },
        );
        let json = serde_json::to_string(&r).expect("serialize");
        let back: InferResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(r, back);
        assert!(!back.is_success());
        assert_eq!(back.error, Some(err));
        assert_eq!(back.timing.queue_latency_ns, Some(2_000_000));
    }

    #[test]
    fn success_rejects_empty_outputs() {
        assert!(matches!(
            InferResult::success("req", vec![], InferenceTiming::default()),
            Err(InferResultError::EmptyOutputs)
        ));
    }

    #[test]
    fn success_rejects_empty_output_name() {
        let mut o = vla_chunk_output();
        o.name.clear();
        assert!(matches!(
            InferResult::success("req", vec![o], InferenceTiming::default()),
            Err(InferResultError::EmptyOutputName)
        ));
    }

    #[test]
    fn success_rejects_duplicate_output_names() {
        let outs = vec![vla_chunk_output(), vla_chunk_output()];
        assert!(matches!(
            InferResult::success("req", outs, InferenceTiming::default()),
            Err(InferResultError::DuplicateOutputName(_))
        ));
    }

    #[test]
    fn ingress_time_failure_allows_empty_request_id() {
        let err = ProtocolError::new(ErrorCode::ConfigInvalid, "malformed payload");
        let r = InferResult::failure("", err, InferenceTiming::default());
        assert_eq!(r.request_id, "");
        assert_eq!(r.status, InferResultStatus::Failure);
    }

    #[test]
    fn version_check_decoder_accepts_current_schema() {
        let r = InferResult::success(
            "req-1",
            vec![vla_chunk_output()],
            InferenceTiming::default(),
        )
        .expect("valid");
        let json = serde_json::to_string(&r).expect("serialize");
        let back: InferResult = decode_with_version_check(&json).expect("decode");
        assert_eq!(back.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn version_check_decoder_rejects_current_schema_status_invariant_violation() {
        let json = format!(
            r#"{{
                "schema_version":"{SCHEMA_VERSION}",
                "request_id":"req-1",
                "status":"success",
                "outputs":[{{
                    "name":"action_chunk",
                    "buffer":{{"schema_version":"{SCHEMA_VERSION}","id":101,"size_bytes":448,"ownership":"owned"}},
                    "tensor":{{"schema_version":"{SCHEMA_VERSION}","dtype":"float32","shape":[16,7]}}
                }}],
                "error":{{"schema_version":"{SCHEMA_VERSION}","code":"internal","message":"should not be present"}}
            }}"#
        );
        let err = decode_with_version_check::<InferResult>(&json).expect_err("rejected");
        assert!(matches!(err, crate::DecodeError::InvalidPayload(_)));
    }
}
