// SPDX-License-Identifier: Apache-2.0
//
// Rust mirror of `config/schemas/memory_budget_breakdown.json`: the canonical
// memory budget line-item vocabulary shared by every model class.
//
// The schema document lives under `config/schemas/` (not `protocol/schemas/`)
// because the vocabulary is a config-level artifact consumed at deploy and
// admission time, not a cross-process wire payload; the memory pathway's
// platform profile records land beside it. A conformance test in
// `tests/memory_budget_fixtures.rs` keeps this mirror and the schema
// document in lockstep.

use serde::{Deserialize, Serialize};

use crate::{DecodeError, ValidatePayload};

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
    pub model_weights_bytes: u64,
    /// Framework/runtime fixed overhead (CUDA context, torch runtime, CT2
    /// runtime, engine runtime).
    #[serde(default)]
    pub runtime_overhead_bytes: u64,
    /// Execution-session scratch (activation workspace, bindings).
    #[serde(default)]
    pub session_scratch_bytes: u64,
    /// Reusable cache state: KV/token cache, tokenizer/decoder cache, or an
    /// engine-managed KV pool on delegated rows.
    #[serde(default)]
    pub cache_bytes: u64,
    /// Iterative-step scratch: flow/denoising steps, beam state beyond
    /// per-session state where applicable.
    #[serde(default)]
    pub step_scratch_bytes: u64,
    /// Queued outputs awaiting delivery: action queue, undelivered stream
    /// events.
    #[serde(default)]
    pub output_queue_bytes: u64,
    /// Ingress/egress buffers: frames, audio chunks, preallocated
    /// fixed-shape outputs.
    #[serde(default)]
    pub io_buffer_bytes: u64,
    /// Marginal cost of one live streaming session (multiplied by the
    /// session ceiling in ledger admission).
    #[serde(default)]
    pub per_session_state_bytes: u64,
    /// Sidecar process footprint (RSS bound) where the backend path uses
    /// one.
    #[serde(default)]
    pub sidecar_process_bytes: u64,
    /// OS/system reserve for the platform profile.
    #[serde(default)]
    pub os_reserve_bytes: u64,
    /// Backend-specific reserve (allocator slack, engine internals not
    /// covered by other lines).
    #[serde(default)]
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
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryBudgetDeclaration {
    pub schema_version: String,
    pub memory_budget_breakdown_bytes: MemoryBudgetBreakdown,
}

impl ValidatePayload for MemoryBudgetDeclaration {
    fn validate_payload(self) -> Result<Self, DecodeError> {
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{MemoryBudgetBreakdown, MemoryBudgetDeclaration, MEMORY_BUDGET_LINE_NAMES};
    use crate::{decode_with_version_check, DecodeError, SCHEMA_VERSION};

    fn declaration_json(breakdown_body: &str) -> String {
        format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","memory_budget_breakdown_bytes":{breakdown_body}}}"#
        )
    }

    #[test]
    fn undeclared_lines_default_to_zero() {
        let raw = declaration_json(r#"{"model_weights_bytes":1200000000}"#);
        let decl: MemoryBudgetDeclaration =
            decode_with_version_check(&raw).expect("minimal declaration decodes");
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
        let err = decode_with_version_check::<MemoryBudgetDeclaration>(&raw)
            .expect_err("unknown line must reject");
        assert!(
            matches!(err, DecodeError::Malformed(_)),
            "expected malformed decode error, got: {err:?}"
        );
        assert!(
            err.to_string().contains("gpu_weights_bytes"),
            "error should name the unknown line: {err}"
        );
    }

    #[test]
    fn missing_required_weights_line_rejects() {
        let raw = declaration_json(r#"{"cache_bytes":128}"#);
        let err = decode_with_version_check::<MemoryBudgetDeclaration>(&raw)
            .expect_err("missing model_weights_bytes must reject");
        assert!(
            err.to_string().contains("model_weights_bytes"),
            "error should name the missing line: {err}"
        );
    }

    #[test]
    fn non_numeric_line_value_rejects() {
        let raw = declaration_json(r#"{"model_weights_bytes":"lots"}"#);
        decode_with_version_check::<MemoryBudgetDeclaration>(&raw)
            .expect_err("string line value must reject");
    }

    #[test]
    fn negative_line_value_rejects() {
        let raw = declaration_json(r#"{"model_weights_bytes":1,"cache_bytes":-4096}"#);
        decode_with_version_check::<MemoryBudgetDeclaration>(&raw)
            .expect_err("negative line value must reject");
    }

    #[test]
    fn fractional_line_value_rejects() {
        let raw = declaration_json(r#"{"model_weights_bytes":1.5}"#);
        decode_with_version_check::<MemoryBudgetDeclaration>(&raw)
            .expect_err("fractional line value must reject");
    }

    #[test]
    fn unknown_top_level_field_rejects() {
        let raw = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","memory_budget_breakdown_bytes":{{"model_weights_bytes":1}},"memory_budget_total_bytes":1}}"#
        );
        decode_with_version_check::<MemoryBudgetDeclaration>(&raw)
            .expect_err("unknown top-level field must reject");
    }

    #[test]
    fn unsupported_schema_version_rejects() {
        let raw =
            r#"{"schema_version":"9.9","memory_budget_breakdown_bytes":{"model_weights_bytes":1}}"#;
        let err = decode_with_version_check::<MemoryBudgetDeclaration>(raw)
            .expect_err("wrong schema_version must reject");
        assert!(matches!(err, DecodeError::UnsupportedSchemaVersion { .. }));
    }

    #[test]
    fn round_trip_preserves_all_lines() {
        let original = MemoryBudgetDeclaration {
            schema_version: SCHEMA_VERSION.to_string(),
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
        let back: MemoryBudgetDeclaration = decode_with_version_check(&json).expect("re-decode");
        assert_eq!(original, back);
    }

    #[test]
    fn line_names_match_struct_serialization() {
        let decl = MemoryBudgetDeclaration {
            schema_version: SCHEMA_VERSION.to_string(),
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
