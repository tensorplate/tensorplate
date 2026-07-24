// SPDX-License-Identifier: Apache-2.0
//
// Rust mirror of `config/schemas/memory_budget_breakdown.json`: the canonical
// memory budget line-item vocabulary shared by every model class.
//
// The schema document lives under `config/schemas/` (not `protocol/schemas/`)
// because the vocabulary is a config-level artifact consumed at deploy and
// admission time, not a cross-process wire payload; the memory pathway's
// platform profile records land beside it. Because it is a config schema, it
// versions on its own track ([`MEMORY_BUDGET_SCHEMA_VERSION`]) independent of
// the cross-process protocol version, and validation failures map to
// `ErrorCode::ConfigInvalid` per `docs/architecture/versioning.md`. A
// conformance test in `tests/memory_budget_fixtures.rs` keeps this mirror and
// the schema document in lockstep.

use serde::{Deserialize, Serialize};

use crate::{ErrorCode, ProtocolError};

/// Version of `config/schemas/memory_budget_breakdown.json`.
///
/// Config schemas evolve independently of the cross-process
/// [`crate::PROTOCOL_VERSION`]; this constant tracks that one schema file
/// and merely starts at the same `0.1` value.
pub const MEMORY_BUDGET_SCHEMA_VERSION: &str = "0.1";

/// Typed failures for memory budget declaration parsing. Static-config
/// validation failures; converts into a [`ProtocolError`] with
/// [`ErrorCode::ConfigInvalid`].
#[derive(Debug, thiserror::Error)]
pub enum MemoryBudgetError {
    /// The document is not valid JSON or violates the line-item schema
    /// (unknown line names, missing required lines, out-of-domain values).
    #[error("malformed memory budget declaration: {0}")]
    Malformed(#[from] serde_json::Error),

    /// Top-level `schema_version` is missing or not a string.
    #[error("memory budget declaration is missing `schema_version`")]
    MissingSchemaVersion,

    /// `schema_version` does not match [`MEMORY_BUDGET_SCHEMA_VERSION`].
    #[error("unsupported memory budget schema_version `{got}` (expected `{expected}`)")]
    UnsupportedSchemaVersion { got: String, expected: &'static str },
}

impl From<MemoryBudgetError> for ProtocolError {
    fn from(value: MemoryBudgetError) -> Self {
        ProtocolError::new(
            ErrorCode::ConfigInvalid,
            "invalid memory budget declaration",
        )
        .with_context(value.to_string())
    }
}

/// Deserialize one byte-count line into the schema's numeric domain:
/// integers in `[0, 2^64)`. Draft-07 `type: integer` treats any number with
/// a zero fractional part as an integer (`1.0` validates), so this accepts
/// integral floats below 2^64 and rejects everything else — keeping the
/// Rust domain identical to the schema's `minimum`/`maximum` bounds.
fn de_bytes_line<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let n = serde_json::Number::deserialize(deserializer)?;
    if let Some(v) = n.as_u64() {
        return Ok(v);
    }
    if let Some(f) = n.as_f64() {
        // Every integral f64 in [0, 2^64) is an exactly representable
        // integer, so the guarded cast below is lossless.
        #[allow(
            clippy::float_cmp,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        if (0.0..18_446_744_073_709_551_616.0).contains(&f) && f.fract() == 0.0 {
            return Ok(f as u64);
        }
    }
    Err(serde::de::Error::custom(format!(
        "byte line values must be integers in [0, 2^64), got {n}"
    )))
}

/// Canonical `memory_budget_breakdown_bytes` line items.
///
/// A deployable declares the lines relevant to its class; lines not declared
/// default to zero, and unknown line names are rejected fail-closed. Whether
/// a specific line must be non-zero is a row/bundle admission rule, not a
/// vocabulary rule, so every optional line accepts zero here.
///
/// `model_weights_bytes` is the one line every class declares explicitly:
/// a budget with no weights line is malformed, not "zero-weight".
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryBudgetBreakdown {
    /// Resident weights for this deployment (per variant where variants are
    /// full checkpoints). Required: presence is mandatory, zero is rejected
    /// at row/bundle admission, not here.
    #[serde(deserialize_with = "de_bytes_line")]
    pub model_weights_bytes: u64,
    /// Framework/runtime fixed overhead (CUDA context, torch runtime, CT2
    /// runtime, engine runtime).
    #[serde(default, deserialize_with = "de_bytes_line")]
    pub runtime_overhead_bytes: u64,
    /// Execution-session scratch (activation workspace, bindings).
    #[serde(default, deserialize_with = "de_bytes_line")]
    pub session_scratch_bytes: u64,
    /// Reusable cache state: KV/token cache, tokenizer/decoder cache, or an
    /// engine-managed KV pool on delegated rows.
    #[serde(default, deserialize_with = "de_bytes_line")]
    pub cache_bytes: u64,
    /// Iterative-step scratch: flow/denoising steps, beam state beyond
    /// per-session state where applicable.
    #[serde(default, deserialize_with = "de_bytes_line")]
    pub step_scratch_bytes: u64,
    /// Queued outputs awaiting delivery: action queue, undelivered stream
    /// events.
    #[serde(default, deserialize_with = "de_bytes_line")]
    pub output_queue_bytes: u64,
    /// Ingress/egress buffers: frames, audio chunks, preallocated
    /// fixed-shape outputs.
    #[serde(default, deserialize_with = "de_bytes_line")]
    pub io_buffer_bytes: u64,
    /// Marginal cost of one live streaming session (multiplied by the
    /// session ceiling in ledger admission).
    #[serde(default, deserialize_with = "de_bytes_line")]
    pub per_session_state_bytes: u64,
    /// Sidecar process footprint (RSS bound) where the backend path uses
    /// one.
    #[serde(default, deserialize_with = "de_bytes_line")]
    pub sidecar_process_bytes: u64,
    /// OS/system reserve for the platform profile.
    #[serde(default, deserialize_with = "de_bytes_line")]
    pub os_reserve_bytes: u64,
    /// Backend-specific reserve (allocator slack, engine internals not
    /// covered by other lines).
    #[serde(default, deserialize_with = "de_bytes_line")]
    pub backend_reserve_bytes: u64,
}

/// The canonical line names, in schema order. The single vocabulary other
/// components (telemetry, release evidence, admission math) key off; adding
/// a name here requires updating the schema document and the memory pathway
/// vocabulary in lockstep.
pub const MEMORY_BUDGET_LINE_NAMES: [&str; 11] = [
    "model_weights_bytes",
    "runtime_overhead_bytes",
    "session_scratch_bytes",
    "cache_bytes",
    "step_scratch_bytes",
    "output_queue_bytes",
    "io_buffer_bytes",
    "per_session_state_bytes",
    "sidecar_process_bytes",
    "os_reserve_bytes",
    "backend_reserve_bytes",
];

/// Standalone memory-budget declaration document, mirroring the top level of
/// `config/schemas/memory_budget_breakdown.json`.
///
/// Parse with [`MemoryBudgetDeclaration::from_json`], which enforces the
/// schema-scoped version track; this document is deliberately **not** wired
/// into [`crate::decode_with_version_check`] because that path enforces the
/// protocol-global version, and config schemas version independently.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryBudgetDeclaration {
    pub schema_version: String,
    pub memory_budget_breakdown_bytes: MemoryBudgetBreakdown,
}

impl MemoryBudgetDeclaration {
    /// Parse and validate a declaration document, enforcing
    /// [`MEMORY_BUDGET_SCHEMA_VERSION`]. All failures are fail-closed typed
    /// errors that map to [`ErrorCode::ConfigInvalid`].
    pub fn from_json(json: &str) -> Result<Self, MemoryBudgetError> {
        let value: serde_json::Value = serde_json::from_str(json)?;
        let observed = value
            .get("schema_version")
            .and_then(serde_json::Value::as_str)
            .ok_or(MemoryBudgetError::MissingSchemaVersion)?;
        if observed != MEMORY_BUDGET_SCHEMA_VERSION {
            return Err(MemoryBudgetError::UnsupportedSchemaVersion {
                got: observed.to_string(),
                expected: MEMORY_BUDGET_SCHEMA_VERSION,
            });
        }
        Ok(serde_json::from_value(value)?)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{
        MemoryBudgetBreakdown, MemoryBudgetDeclaration, MemoryBudgetError,
        MEMORY_BUDGET_LINE_NAMES, MEMORY_BUDGET_SCHEMA_VERSION,
    };
    use crate::{ErrorCode, ProtocolError};

    fn declaration_json(breakdown_body: &str) -> String {
        format!(
            r#"{{"schema_version":"{MEMORY_BUDGET_SCHEMA_VERSION}","memory_budget_breakdown_bytes":{breakdown_body}}}"#
        )
    }

    #[test]
    fn undeclared_lines_default_to_zero() {
        let raw = declaration_json(r#"{"model_weights_bytes":1200000000}"#);
        let decl = MemoryBudgetDeclaration::from_json(&raw).expect("minimal declaration decodes");
        let b = decl.memory_budget_breakdown_bytes;
        assert_eq!(b.model_weights_bytes, 1_200_000_000);
        assert_eq!(b.runtime_overhead_bytes, 0);
        assert_eq!(b.session_scratch_bytes, 0);
        assert_eq!(b.cache_bytes, 0);
        assert_eq!(b.step_scratch_bytes, 0);
        assert_eq!(b.output_queue_bytes, 0);
        assert_eq!(b.io_buffer_bytes, 0);
        assert_eq!(b.per_session_state_bytes, 0);
        assert_eq!(b.sidecar_process_bytes, 0);
        assert_eq!(b.os_reserve_bytes, 0);
        assert_eq!(b.backend_reserve_bytes, 0);
    }

    #[test]
    fn unknown_line_name_rejects_fail_closed() {
        let raw = declaration_json(r#"{"model_weights_bytes":1,"gpu_weights_bytes":2}"#);
        let err = MemoryBudgetDeclaration::from_json(&raw).expect_err("unknown line must reject");
        assert!(
            matches!(err, MemoryBudgetError::Malformed(_)),
            "expected malformed error, got: {err:?}"
        );
        assert!(
            err.to_string().contains("gpu_weights_bytes"),
            "error should name the unknown line: {err}"
        );
    }

    #[test]
    fn missing_required_weights_line_rejects() {
        let raw = declaration_json(r#"{"cache_bytes":128}"#);
        let err = MemoryBudgetDeclaration::from_json(&raw)
            .expect_err("missing model_weights_bytes must reject");
        assert!(
            err.to_string().contains("model_weights_bytes"),
            "error should name the missing line: {err}"
        );
    }

    #[test]
    fn non_numeric_line_value_rejects() {
        let raw = declaration_json(r#"{"model_weights_bytes":"lots"}"#);
        MemoryBudgetDeclaration::from_json(&raw).expect_err("string line value must reject");
    }

    #[test]
    fn negative_line_value_rejects() {
        let raw = declaration_json(r#"{"model_weights_bytes":1,"cache_bytes":-4096}"#);
        MemoryBudgetDeclaration::from_json(&raw).expect_err("negative line value must reject");
    }

    #[test]
    fn fractional_line_value_rejects() {
        let raw = declaration_json(r#"{"model_weights_bytes":1.5}"#);
        MemoryBudgetDeclaration::from_json(&raw).expect_err("fractional line value must reject");
    }

    #[test]
    fn integral_float_line_value_accepted() {
        // Draft-07 `type: integer` validates numbers with a zero fractional
        // part, so the mirror accepts them identically.
        let raw = declaration_json(r#"{"model_weights_bytes":4096.0}"#);
        let decl = MemoryBudgetDeclaration::from_json(&raw).expect("integral float decodes");
        assert_eq!(decl.memory_budget_breakdown_bytes.model_weights_bytes, 4096);
    }

    #[test]
    fn u64_max_line_value_accepted() {
        let raw = declaration_json(r#"{"model_weights_bytes":18446744073709551615}"#);
        let decl = MemoryBudgetDeclaration::from_json(&raw).expect("u64::MAX decodes");
        assert_eq!(
            decl.memory_budget_breakdown_bytes.model_weights_bytes,
            u64::MAX
        );
    }

    #[test]
    fn above_u64_max_line_value_rejects() {
        let raw = declaration_json(r#"{"model_weights_bytes":18446744073709551616}"#);
        MemoryBudgetDeclaration::from_json(&raw).expect_err("2^64 must reject");
    }

    #[test]
    fn unknown_top_level_field_rejects() {
        let raw = format!(
            r#"{{"schema_version":"{MEMORY_BUDGET_SCHEMA_VERSION}","memory_budget_breakdown_bytes":{{"model_weights_bytes":1}},"memory_budget_total_bytes":1}}"#
        );
        MemoryBudgetDeclaration::from_json(&raw).expect_err("unknown top-level field must reject");
    }

    #[test]
    fn missing_schema_version_rejects() {
        let raw = r#"{"memory_budget_breakdown_bytes":{"model_weights_bytes":1}}"#;
        let err = MemoryBudgetDeclaration::from_json(raw).expect_err("missing version must reject");
        assert!(matches!(err, MemoryBudgetError::MissingSchemaVersion));
    }

    #[test]
    fn unsupported_schema_version_rejects() {
        let raw =
            r#"{"schema_version":"9.9","memory_budget_breakdown_bytes":{"model_weights_bytes":1}}"#;
        let err = MemoryBudgetDeclaration::from_json(raw).expect_err("wrong version must reject");
        assert!(matches!(
            err,
            MemoryBudgetError::UnsupportedSchemaVersion { .. }
        ));
    }

    #[test]
    fn errors_map_to_config_invalid() {
        let raw =
            r#"{"schema_version":"9.9","memory_budget_breakdown_bytes":{"model_weights_bytes":1}}"#;
        let err = MemoryBudgetDeclaration::from_json(raw).expect_err("wrong version must reject");
        let protocol_error = ProtocolError::from(err);
        assert_eq!(protocol_error.code, ErrorCode::ConfigInvalid);
        assert!(
            protocol_error
                .context
                .as_deref()
                .is_some_and(|c| c.contains("9.9")),
            "context should carry the offending version"
        );
    }

    #[test]
    fn round_trip_preserves_all_lines() {
        let original = MemoryBudgetDeclaration {
            schema_version: MEMORY_BUDGET_SCHEMA_VERSION.to_string(),
            memory_budget_breakdown_bytes: MemoryBudgetBreakdown {
                model_weights_bytes: 1,
                runtime_overhead_bytes: 2,
                session_scratch_bytes: 3,
                cache_bytes: 4,
                step_scratch_bytes: 5,
                output_queue_bytes: 6,
                io_buffer_bytes: 7,
                per_session_state_bytes: 8,
                sidecar_process_bytes: 9,
                os_reserve_bytes: 10,
                backend_reserve_bytes: 11,
            },
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let back = MemoryBudgetDeclaration::from_json(&json).expect("re-decode");
        assert_eq!(original, back);
    }

    #[test]
    fn line_names_match_struct_serialization() {
        let decl = MemoryBudgetDeclaration {
            schema_version: MEMORY_BUDGET_SCHEMA_VERSION.to_string(),
            memory_budget_breakdown_bytes: MemoryBudgetBreakdown::default(),
        };
        let value = serde_json::to_value(decl).expect("serialize");
        let lines = value["memory_budget_breakdown_bytes"]
            .as_object()
            .expect("breakdown object");
        // serde_json maps are alphabetically ordered, so compare as sets.
        let mut serialized: Vec<&str> = lines.keys().map(String::as_str).collect();
        serialized.sort_unstable();
        let mut expected = MEMORY_BUDGET_LINE_NAMES.to_vec();
        expected.sort_unstable();
        assert_eq!(serialized, expected);
    }
}
