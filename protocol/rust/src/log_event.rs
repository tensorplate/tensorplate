// SPDX-License-Identifier: Apache-2.0
//
// V01-E12-F01: Rust mirror of `protocol/schemas/log_event.json`.
//
// `LogEvent` is the shared structured log envelope emitted by the
// agent, serving worker, runtime/adapters, Python/PyTorch sidecar, and
// observability service. The struct is a value object; emitters
// construct it through the bounded helpers in this module so that
// context size, key cardinality, and prohibited payloads are checked
// at construction time, not at sink time.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::correlation_id::validate_correlation_id;
use crate::error::ErrorCode;
use crate::failure_reason::FailureReason;
use crate::model_spec::ModelClass;
use crate::{DecodeError, ValidatePayload, SCHEMA_VERSION};

/// Maximum allowed number of context entries on a single log event.
/// Producers exceeding this drop additional entries with a bounded
/// counter rather than allocating unbounded maps.
pub const MAX_LOG_CONTEXT_ENTRIES: usize = 16;

/// Maximum allowed byte length of any single context string value.
/// Mirrors the JSON schema constraint.
pub const MAX_LOG_CONTEXT_STRING_BYTES: usize = 256;

/// Producer component identifier. Bounded so log readers can dispatch
/// on a stable enum and sinks can shard by component without unbounded
/// label cardinality.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogComponent {
    Agent,
    ServingWorker,
    Runtime,
    Adapter,
    PythonPytorchSidecar,
    Observability,
    Cli,
}

/// Severity level. Bounded to the v0.1 set; `Trace` is filtered out of
/// bounded sinks by default.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    /// Lower-case wire name; identical to the `serde` representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// Bounded context value carried under a log event. The serialiser
/// preserves the original primitive shape so downstream JSON readers
/// see numbers as numbers, not stringified numbers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LogContextValue {
    String(String),
    Integer(i64),
    Float(f64),
    Bool(bool),
    Null,
}

impl Eq for LogContextValue {}

impl LogContextValue {
    /// Returns true when the value is a string longer than the bounded
    /// length, or contains characters that suggest a payload (NUL,
    /// control characters); sanitiser drops these.
    #[must_use]
    pub fn is_safe_bounded(&self) -> bool {
        match self {
            Self::String(s) => {
                s.len() <= MAX_LOG_CONTEXT_STRING_BYTES
                    && !s.bytes().any(|b| b == 0 || (b < 0x20 && b != b'\t'))
            }
            Self::Float(f) => f.is_finite(),
            Self::Integer(_) | Self::Bool(_) | Self::Null => true,
        }
    }
}

impl From<&str> for LogContextValue {
    fn from(value: &str) -> Self {
        Self::String(value.to_string())
    }
}

impl From<String> for LogContextValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<i64> for LogContextValue {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

impl From<u64> for LogContextValue {
    fn from(value: u64) -> Self {
        Self::Integer(i64::try_from(value).unwrap_or(i64::MAX))
    }
}

impl From<f64> for LogContextValue {
    fn from(value: f64) -> Self {
        Self::Float(value)
    }
}

impl From<bool> for LogContextValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

/// Wire payload for `protocol/schemas/log_event.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogEvent {
    pub schema_version: String,
    pub component: LogComponent,
    pub event: String,
    pub level: LogLevel,
    pub monotonic_timestamp_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_time_iso8601: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_class: Option<ModelClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<ErrorCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<FailureReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub context: BTreeMap<String, LogContextValue>,
}

impl Eq for LogEvent {}

impl LogEvent {
    /// Construct a minimal log event with no optional fields.
    #[must_use]
    pub fn new(
        component: LogComponent,
        event: impl Into<String>,
        level: LogLevel,
        monotonic_timestamp_ns: u64,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            component,
            event: event.into(),
            level,
            monotonic_timestamp_ns,
            wall_time_iso8601: None,
            correlation_id: None,
            request_id: None,
            transaction_id: None,
            deployment_id: None,
            model_name: None,
            model_class: None,
            backend: None,
            error_code: None,
            failure_reason: None,
            duration_ms: None,
            context: BTreeMap::new(),
        }
    }

    /// Attach a correlation id. Producers should use
    /// [`crate::correlation_id::CorrelationId`] to ensure the id stays
    /// in policy.
    #[must_use]
    pub fn with_correlation_id(mut self, id: impl Into<String>) -> Self {
        self.correlation_id = Some(id.into());
        self
    }

    /// Set the deployment id.
    #[must_use]
    pub fn with_deployment(mut self, id: impl Into<String>) -> Self {
        self.deployment_id = Some(id.into());
        self
    }

    /// Set the failure reason and (implicitly) the protocol error code
    /// derived from the canonical mapping.
    #[must_use]
    pub fn with_failure(mut self, reason: FailureReason) -> Self {
        self.error_code = Some(reason.error_code());
        self.failure_reason = Some(reason);
        self
    }

    /// Insert a bounded context entry. Insertions beyond
    /// [`MAX_LOG_CONTEXT_ENTRIES`] or strings beyond
    /// [`MAX_LOG_CONTEXT_STRING_BYTES`] are dropped; the producer can
    /// rely on the sanitiser to enforce the rules rather than checking
    /// at every call site.
    pub fn insert_context(&mut self, key: impl Into<String>, value: impl Into<LogContextValue>) {
        if self.context.len() >= MAX_LOG_CONTEXT_ENTRIES {
            return;
        }
        let key = key.into();
        if !is_safe_context_key(&key) {
            return;
        }
        let value = value.into();
        let value = match value {
            LogContextValue::String(s) => LogContextValue::String(truncate_to_bound(s)),
            other => other,
        };
        if !value.is_safe_bounded() {
            return;
        }
        self.context.insert(key, value);
    }
}

fn is_safe_context_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 48
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

fn truncate_to_bound(mut s: String) -> String {
    if s.len() <= MAX_LOG_CONTEXT_STRING_BYTES {
        return s;
    }
    let mut cut = MAX_LOG_CONTEXT_STRING_BYTES;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s
}

impl ValidatePayload for LogEvent {
    fn validate_payload(self) -> Result<Self, DecodeError> {
        if self.event.is_empty() || self.event.len() > 64 {
            return Err(DecodeError::InvalidPayload(
                "LogEvent.event must be 1..=64 bytes".into(),
            ));
        }
        if !self
            .event
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
        {
            return Err(DecodeError::InvalidPayload(
                "LogEvent.event must match [a-z0-9_.]+".into(),
            ));
        }
        for id in [
            self.correlation_id.as_deref(),
            self.request_id.as_deref(),
            self.transaction_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            validate_correlation_id(id)?;
        }
        if self.context.len() > MAX_LOG_CONTEXT_ENTRIES {
            return Err(DecodeError::InvalidPayload(format!(
                "LogEvent.context has {} entries (max {MAX_LOG_CONTEXT_ENTRIES})",
                self.context.len()
            )));
        }
        for (k, v) in &self.context {
            if !is_safe_context_key(k) {
                return Err(DecodeError::InvalidPayload(format!(
                    "LogEvent.context key `{k}` violates bounded-key policy"
                )));
            }
            if !v.is_safe_bounded() {
                return Err(DecodeError::InvalidPayload(format!(
                    "LogEvent.context value for key `{k}` violates bounded-value policy"
                )));
            }
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]

    use super::{
        LogComponent, LogContextValue, LogEvent, LogLevel, MAX_LOG_CONTEXT_ENTRIES,
        MAX_LOG_CONTEXT_STRING_BYTES,
    };
    use crate::failure_reason::FailureReason;
    use crate::{decode_with_version_check, DecodeError, SCHEMA_VERSION};

    #[test]
    fn minimal_event_serialises_required_fields_only() {
        let event = LogEvent::new(
            LogComponent::ServingWorker,
            "request.accepted",
            LogLevel::Info,
            1_000,
        );
        let json = serde_json::to_string(&event).expect("serialise");
        assert!(json.contains("\"component\":\"serving_worker\""));
        assert!(json.contains("\"event\":\"request.accepted\""));
        assert!(json.contains("\"level\":\"info\""));
        // Optional fields are skipped when None.
        assert!(!json.contains("correlation_id"));
        assert!(!json.contains("\"context\""));
    }

    #[test]
    fn context_insertion_enforces_count_and_string_bounds() {
        let mut event = LogEvent::new(LogComponent::Agent, "deploy.warmup", LogLevel::Debug, 0);
        for i in 0..(MAX_LOG_CONTEXT_ENTRIES + 4) {
            event.insert_context(format!("k{i}"), "v");
        }
        assert_eq!(event.context.len(), MAX_LOG_CONTEXT_ENTRIES);

        // Oversize string is truncated, not dropped, so the operator
        // still sees the field.
        let mut e2 = LogEvent::new(LogComponent::Agent, "deploy.warmup", LogLevel::Debug, 0);
        e2.insert_context("blob", "x".repeat(MAX_LOG_CONTEXT_STRING_BYTES + 32));
        let v = e2.context.get("blob").expect("present");
        match v {
            LogContextValue::String(s) => assert!(s.len() <= MAX_LOG_CONTEXT_STRING_BYTES),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn unsafe_context_keys_and_values_are_dropped() {
        let mut event = LogEvent::new(LogComponent::Cli, "doctor.check", LogLevel::Info, 0);
        event.insert_context("bad key", "ok");
        event.insert_context("ok", "\u{0000}bad");
        event.insert_context("ok", LogContextValue::Float(f64::NAN));
        assert!(event.context.is_empty());
    }

    #[test]
    fn with_failure_sets_error_code_and_reason() {
        let event = LogEvent::new(LogComponent::Adapter, "infer.timeout", LogLevel::Error, 0)
            .with_failure(FailureReason::Timeout);
        assert_eq!(
            event.error_code,
            Some(tensorplate_protocol_self_check::expected_timeout_code())
        );
        assert_eq!(event.failure_reason, Some(FailureReason::Timeout));
    }

    #[test]
    fn decode_rejects_event_name_outside_policy() {
        let json = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","component":"agent","event":"BAD NAME","level":"info","monotonic_timestamp_ns":1}}"#
        );
        let err = decode_with_version_check::<LogEvent>(&json).expect_err("rejected");
        assert!(matches!(err, DecodeError::InvalidPayload(_)));
    }

    #[test]
    fn decode_rejects_unknown_schema_version() {
        let json = r#"{"schema_version":"99.99","component":"agent","event":"ok","level":"info","monotonic_timestamp_ns":1}"#;
        let err = decode_with_version_check::<LogEvent>(json).expect_err("rejected");
        assert!(matches!(err, DecodeError::UnsupportedSchemaVersion { .. }));
    }

    #[test]
    fn decode_rejects_out_of_policy_correlation_id() {
        let json = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","component":"agent","event":"ok","level":"info","monotonic_timestamp_ns":1,"correlation_id":"bad id"}}"#
        );
        let err = decode_with_version_check::<LogEvent>(&json).expect_err("rejected");
        assert!(matches!(err, DecodeError::InvalidPayload(_)));
    }

    mod tensorplate_protocol_self_check {
        // Tiny helper so the test file does not pin to a specific
        // canonical code; the source of truth is the FailureReason
        // mapping.
        use crate::error::ErrorCode;
        use crate::failure_reason::FailureReason;
        pub fn expected_timeout_code() -> ErrorCode {
            FailureReason::Timeout.error_code()
        }
    }
}
