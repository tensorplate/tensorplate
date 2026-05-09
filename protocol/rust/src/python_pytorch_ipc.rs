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

use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;
use crate::model_spec::ModelSpec;
use crate::tensor_view::TensorView;
use crate::SCHEMA_VERSION;

/// Sidecar IPC message kind. `*_response` messages carry the same
/// `message_id` as the originating request; `*_event` messages are
/// unsolicited and carry their own `message_id`.
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

/// Mirror of `protocol/schemas/python_pytorch_ipc.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IpcMessage {
    pub schema_version: String,
    pub message_id: String,
    pub kind: IpcMessageKind,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ns: Option<u64>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tensors: Vec<IpcTensor>,

    /// Present on `LoadModel`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_spec: Option<ModelSpec>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<IpcStatus>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ProtocolError>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<IpcMetric>,
}

impl Eq for IpcMessage {}

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
}

impl IpcMessage {
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
            schema_version: SCHEMA_VERSION.to_string(),
            message_id,
            kind: IpcMessageKind::LoadModel,
            correlation_id: None,
            deadline_ns: None,
            tensors: Vec::new(),
            model_spec: Some(model_spec),
            status: None,
            error: None,
            metric: None,
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
        match self.kind {
            IpcMessageKind::LoadModel if self.model_spec.is_none() => {
                Err(IpcMessageError::LoadModelMissingSpec)
            }
            IpcMessageKind::ErrorEvent if self.error.is_none() => {
                Err(IpcMessageError::ErrorEventMissingError)
            }
            IpcMessageKind::MetricEvent if self.metric.is_none() => {
                Err(IpcMessageError::MetricEventMissingMetric)
            }
            _ => Ok(self),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{
        IpcMessage, IpcMessageError, IpcMessageKind, IpcMetric, IpcStatus, IpcTensor,
        SCHEMA_VERSION,
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
            schema_version: SCHEMA_VERSION.to_string(),
            message_id: "msg-2".into(),
            kind: IpcMessageKind::Infer,
            correlation_id: Some("req-7".into()),
            deadline_ns: Some(1_700_000_000_000),
            tensors: vec![IpcTensor {
                name: "image".into(),
                tensor: view,
                payload_offset: 0,
                payload_length: 3 * 224 * 224 * 2,
            }],
            model_spec: None,
            status: None,
            error: None,
            metric: None,
        };
        let json = serde_json::to_string(&m).expect("serialize");
        let back: IpcMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(m, back);
        assert_eq!(back.tensors.len(), 1);
        assert_eq!(back.tensors[0].payload_length, 3 * 224 * 224 * 2);
    }

    #[test]
    fn infer_response_with_status_ok_round_trips() {
        let m = IpcMessage {
            schema_version: SCHEMA_VERSION.to_string(),
            message_id: "msg-2".into(),
            kind: IpcMessageKind::InferResponse,
            correlation_id: Some("req-7".into()),
            deadline_ns: None,
            tensors: vec![],
            model_spec: None,
            status: Some(IpcStatus::Ok),
            error: None,
            metric: None,
        };
        let json = serde_json::to_string(&m).expect("serialize");
        let back: IpcMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(m, back);
    }

    #[test]
    fn error_event_round_trips() {
        let m = IpcMessage {
            schema_version: SCHEMA_VERSION.to_string(),
            message_id: "evt-1".into(),
            kind: IpcMessageKind::ErrorEvent,
            correlation_id: None,
            deadline_ns: None,
            tensors: vec![],
            model_spec: None,
            status: Some(IpcStatus::Error),
            error: Some(ProtocolError::new(ErrorCode::LoadFailed, "weights missing")),
            metric: None,
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
            schema_version: SCHEMA_VERSION.to_string(),
            message_id: "evt-2".into(),
            kind: IpcMessageKind::MetricEvent,
            correlation_id: None,
            deadline_ns: None,
            tensors: vec![],
            model_spec: None,
            status: None,
            error: None,
            metric: Some(IpcMetric {
                name: "infer_latency_ms".into(),
                value_f64: Some(8.5),
                labels,
            }),
        }
        .validate()
        .expect("valid");
        let json = serde_json::to_string(&m).expect("serialize");
        let back: IpcMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(m, back);
    }

    #[test]
    fn validate_enforces_load_model_requires_spec() {
        let bad = IpcMessage {
            schema_version: SCHEMA_VERSION.to_string(),
            message_id: "msg".into(),
            kind: IpcMessageKind::LoadModel,
            correlation_id: None,
            deadline_ns: None,
            tensors: vec![],
            model_spec: None,
            status: None,
            error: None,
            metric: None,
        }
        .validate();
        assert!(matches!(bad, Err(IpcMessageError::LoadModelMissingSpec)));
    }

    #[test]
    fn validate_rejects_empty_message_id() {
        let bad = IpcMessage {
            schema_version: SCHEMA_VERSION.to_string(),
            message_id: String::new(),
            kind: IpcMessageKind::HealthCheck,
            correlation_id: None,
            deadline_ns: None,
            tensors: vec![],
            model_spec: None,
            status: None,
            error: None,
            metric: None,
        }
        .validate();
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
}
