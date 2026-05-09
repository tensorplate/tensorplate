// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F03: Rust mirror of `protocol/schemas/infer_request.json` and the
// C++ `tensorplate::InferRequest` value object.
//
// The Rust struct is the authoritative wire representation. The C++ runtime
// takes ownership of the payload at the HTTP/IPC boundary (V01-E07) and
// converts the relative `deadline_ms` to an absolute monotonic deadline by
// sampling its own steady clock.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::buffer_ref::BufferRef;
use crate::tensor_view::TensorView;
use crate::SCHEMA_VERSION;

/// One named input binding: stable name plus payload buffer plus tensor
/// metadata. Mirrors C++ `tensorplate::NamedInput`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NamedInput {
    pub name: String,
    pub buffer: BufferRef,
    pub tensor: TensorView,
}

/// Request-scoped metadata. Explicit fields cover LeRobot-compatible async
/// inference (correlation, action-chunk identity / sequence, and stale-
/// request cancellation); `extra` carries caller-defined free-form
/// strings without leaking deployment-specific behavior into the runtime.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RequestMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_chunk_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_chunk_sequence: Option<i64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_after_sequence: Option<i64>,

    /// BTreeMap so JSON serialization is deterministic for round-trip
    /// fixtures. The C++ side uses unordered_map but compares element-
    /// wise on equality.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, String>,
}

/// Mirror of `tensorplate::InferRequest` (C++) and
/// `protocol/schemas/infer_request.json`.
///
/// Wire format only; the C++ runtime owns in-process scheduling, so the
/// Rust mirror does not carry an absolute deadline.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InferRequest {
    pub schema_version: String,
    pub request_id: String,
    pub endpoint: String,
    pub inputs: Vec<NamedInput>,
    #[serde(default, skip_serializing_if = "is_default_metadata")]
    pub metadata: RequestMetadata,
    /// Relative deadline in milliseconds from the moment the receiver
    /// decodes this payload. Receivers sample their monotonic clock and
    /// convert to an absolute deadline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
}

fn is_default_metadata(m: &RequestMetadata) -> bool {
    m == &RequestMetadata::default()
}

/// Validation errors raised by [`InferRequest::new`]. Mirrors the C++
/// `InferRequest::create` rules.
#[derive(Debug, thiserror::Error)]
pub enum InferRequestError {
    #[error("InferRequest.request_id must be non-empty")]
    EmptyRequestId,
    #[error("InferRequest.endpoint must be non-empty")]
    EmptyEndpoint,
    #[error("InferRequest.inputs must contain at least one named input")]
    EmptyInputs,
    #[error("InferRequest.inputs entry has empty `name`")]
    EmptyInputName,
    #[error("InferRequest.inputs has duplicate name `{0}`")]
    DuplicateInputName(String),
}

impl InferRequest {
    /// Build and validate an [`InferRequest`] at the v0.1 schema version.
    ///
    /// # Errors
    ///
    /// See [`InferRequestError`].
    pub fn new(
        request_id: impl Into<String>,
        endpoint: impl Into<String>,
        inputs: Vec<NamedInput>,
        metadata: RequestMetadata,
        deadline_ms: Option<u64>,
    ) -> Result<Self, InferRequestError> {
        let request_id = request_id.into();
        if request_id.is_empty() {
            return Err(InferRequestError::EmptyRequestId);
        }
        let endpoint = endpoint.into();
        if endpoint.is_empty() {
            return Err(InferRequestError::EmptyEndpoint);
        }
        if inputs.is_empty() {
            return Err(InferRequestError::EmptyInputs);
        }
        let mut seen: std::collections::HashSet<&str> =
            std::collections::HashSet::with_capacity(inputs.len());
        for input in &inputs {
            if input.name.is_empty() {
                return Err(InferRequestError::EmptyInputName);
            }
            if !seen.insert(input.name.as_str()) {
                return Err(InferRequestError::DuplicateInputName(input.name.clone()));
            }
        }
        Ok(Self {
            schema_version: SCHEMA_VERSION.to_string(),
            request_id,
            endpoint,
            inputs,
            metadata,
            deadline_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{
        BTreeMap, InferRequest, InferRequestError, NamedInput, RequestMetadata, SCHEMA_VERSION,
    };
    use crate::buffer_ref::{BufferOwnership, BufferRef};
    use crate::decode_with_version_check;
    use crate::tensor_view::{DType, Layout, TensorView};

    fn vision_input() -> NamedInput {
        NamedInput {
            name: "image".into(),
            buffer: BufferRef::new(1, 3 * 224 * 224 * 2, BufferOwnership::Owned).expect("buf"),
            tensor: TensorView::new(DType::Float16, vec![1, 3, 224, 224], Layout::RowMajor, 0, 0)
                .expect("view"),
        }
    }

    fn smolvla_inputs() -> Vec<NamedInput> {
        vec![
            NamedInput {
                name: "image_front".into(),
                buffer: BufferRef::new(1, 480 * 640 * 3, BufferOwnership::Owned).expect("buf"),
                tensor: TensorView::new(DType::Uint8, vec![480, 640, 3], Layout::RowMajor, 0, 0)
                    .expect("view"),
            },
            NamedInput {
                name: "image_wrist".into(),
                buffer: BufferRef::new(2, 480 * 640 * 3, BufferOwnership::Owned).expect("buf"),
                tensor: TensorView::new(DType::Uint8, vec![480, 640, 3], Layout::RowMajor, 0, 0)
                    .expect("view"),
            },
            NamedInput {
                name: "state".into(),
                buffer: BufferRef::new(3, 7 * 4, BufferOwnership::Owned).expect("buf"),
                tensor: TensorView::new(DType::Float32, vec![7], Layout::RowMajor, 0, 0)
                    .expect("view"),
            },
            NamedInput {
                name: "instruction".into(),
                buffer: BufferRef::new(4, 256 * 4, BufferOwnership::Owned).expect("buf"),
                tensor: TensorView::new(DType::Int32, vec![256], Layout::RowMajor, 0, 0)
                    .expect("view"),
            },
        ]
    }

    #[test]
    fn single_input_vision_round_trips() {
        let req = InferRequest::new(
            "req-1",
            "yolov8n",
            vec![vision_input()],
            RequestMetadata::default(),
            None,
        )
        .expect("valid");
        let json = serde_json::to_string(&req).expect("serialize");
        let back: InferRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(req, back);
    }

    #[test]
    fn smolvla_multi_input_round_trips() {
        let mut metadata = RequestMetadata {
            correlation_id: Some("corr-42".into()),
            action_chunk_id: Some("chunk-7".into()),
            action_chunk_sequence: Some(7),
            stale_after_sequence: Some(5),
            extra: BTreeMap::new(),
        };
        metadata.extra.insert("episode".into(), "20240501-1".into());

        let req = InferRequest::new(
            "req-2",
            "smolvla-450m",
            smolvla_inputs(),
            metadata,
            Some(33),
        )
        .expect("valid");
        let json = serde_json::to_string(&req).expect("serialize");
        let back: InferRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(req, back);
        assert_eq!(back.deadline_ms, Some(33));
        assert_eq!(back.inputs.len(), 4);
    }

    #[test]
    fn rejects_empty_required_fields() {
        let v = vec![vision_input()];
        assert!(matches!(
            InferRequest::new("", "ep", v.clone(), RequestMetadata::default(), None),
            Err(InferRequestError::EmptyRequestId)
        ));
        assert!(matches!(
            InferRequest::new("id", "", v.clone(), RequestMetadata::default(), None),
            Err(InferRequestError::EmptyEndpoint)
        ));
        assert!(matches!(
            InferRequest::new("id", "ep", vec![], RequestMetadata::default(), None),
            Err(InferRequestError::EmptyInputs)
        ));

        let bad_name = NamedInput {
            name: String::new(),
            ..vision_input()
        };
        assert!(matches!(
            InferRequest::new("id", "ep", vec![bad_name], RequestMetadata::default(), None),
            Err(InferRequestError::EmptyInputName)
        ));
    }

    #[test]
    fn rejects_duplicate_input_names() {
        let dup = vec![vision_input(), vision_input()];
        let err = InferRequest::new("id", "ep", dup, RequestMetadata::default(), None)
            .expect_err("rejected");
        assert!(matches!(err, InferRequestError::DuplicateInputName(_)));
    }

    #[test]
    fn version_check_decoder_accepts_current_schema() {
        let req = InferRequest::new(
            "req-3",
            "yolo",
            vec![vision_input()],
            RequestMetadata::default(),
            None,
        )
        .expect("valid");
        let json = serde_json::to_string(&req).expect("serialize");
        let back: InferRequest = decode_with_version_check(&json).expect("decode");
        assert_eq!(back.schema_version, SCHEMA_VERSION);
    }
}
