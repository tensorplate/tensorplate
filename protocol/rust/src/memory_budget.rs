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

use crate::json_numbers;
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
    /// (unknown line names, missing required lines, non-numeric values).
    /// Out-of-domain numeric values surface as [`Self::InvalidNumberLexeme`]
    /// through [`MemoryBudgetDeclaration::from_json`].
    #[error("malformed memory budget declaration: {0}")]
    Malformed(#[from] serde_json::Error),

    /// Top-level `schema_version` is missing or not a string.
    #[error("memory budget declaration is missing `schema_version`")]
    MissingSchemaVersion,

    /// `schema_version` does not match [`MEMORY_BUDGET_SCHEMA_VERSION`].
    #[error("unsupported memory budget schema_version `{got}` (expected `{expected}`)")]
    UnsupportedSchemaVersion { got: String, expected: &'static str },

    /// A number token in the document is outside the byte-line domain.
    /// Checked on the exact decimal lexeme before any float parsing, so
    /// high-precision tokens cannot slip through IEEE-754 rounding.
    #[error("invalid byte line value `{token}`: {reason}")]
    InvalidNumberLexeme { token: String, reason: &'static str },
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

/// Upper bound (inclusive) for every byte line, re-exported from the
/// shared numeric-safety helpers: 2^53 - 1, the largest integer every
/// mainstream JSON parser represents exactly.
pub const MEMORY_BUDGET_LINE_MAX_BYTES: u64 = json_numbers::MAX_SAFE_BYTES;

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
    #[serde(deserialize_with = "json_numbers::deserialize_safe_bytes")]
    pub model_weights_bytes: u64,
    /// Framework/runtime fixed overhead (CUDA context, torch runtime, CT2
    /// runtime, engine runtime).
    #[serde(default, deserialize_with = "json_numbers::deserialize_safe_bytes")]
    pub runtime_overhead_bytes: u64,
    /// Execution-session scratch (activation workspace, bindings).
    #[serde(default, deserialize_with = "json_numbers::deserialize_safe_bytes")]
    pub session_scratch_bytes: u64,
    /// Reusable cache state: KV/token cache, tokenizer/decoder cache, or an
    /// engine-managed KV pool on delegated rows.
    #[serde(default, deserialize_with = "json_numbers::deserialize_safe_bytes")]
    pub cache_bytes: u64,
    /// Iterative-step scratch: flow/denoising steps, beam state beyond
    /// per-session state where applicable.
    #[serde(default, deserialize_with = "json_numbers::deserialize_safe_bytes")]
    pub step_scratch_bytes: u64,
    /// Queued outputs awaiting delivery: action queue, undelivered stream
    /// events.
    #[serde(default, deserialize_with = "json_numbers::deserialize_safe_bytes")]
    pub output_queue_bytes: u64,
    /// Ingress/egress buffers: frames, audio chunks, preallocated
    /// fixed-shape outputs.
    #[serde(default, deserialize_with = "json_numbers::deserialize_safe_bytes")]
    pub io_buffer_bytes: u64,
    /// Marginal cost of one live streaming session (multiplied by the
    /// session ceiling in ledger admission).
    #[serde(default, deserialize_with = "json_numbers::deserialize_safe_bytes")]
    pub per_session_state_bytes: u64,
    /// Sidecar process footprint (RSS bound) where the backend path uses
    /// one.
    #[serde(default, deserialize_with = "json_numbers::deserialize_safe_bytes")]
    pub sidecar_process_bytes: u64,
    /// OS/system reserve for the platform profile.
    #[serde(default, deserialize_with = "json_numbers::deserialize_safe_bytes")]
    pub os_reserve_bytes: u64,
    /// Backend-specific reserve (allocator slack, engine internals not
    /// covered by other lines).
    #[serde(default, deserialize_with = "json_numbers::deserialize_safe_bytes")]
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
    /// [`MEMORY_BUDGET_SCHEMA_VERSION`] and the exact byte-line numeric
    /// domain (validated on each number token's decimal lexeme, immune to
    /// IEEE-754 rounding). All failures are fail-closed typed errors that
    /// map to [`ErrorCode::ConfigInvalid`]. This is the validated entry
    /// point; deserializing the types directly with serde skips the
    /// lexeme-level checks.
    pub fn from_json(json: &str) -> Result<Self, MemoryBudgetError> {
        let canonical = json_numbers::canonicalize_byte_lexemes(json)
            .map_err(|(token, reason)| MemoryBudgetError::InvalidNumberLexeme { token, reason })?;
        let value: serde_json::Value = serde_json::from_str(&canonical)?;
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
        MEMORY_BUDGET_LINE_MAX_BYTES, MEMORY_BUDGET_LINE_NAMES, MEMORY_BUDGET_SCHEMA_VERSION,
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
    fn max_safe_integer_line_value_accepted() {
        let raw = declaration_json(r#"{"model_weights_bytes":9007199254740991}"#);
        let decl = MemoryBudgetDeclaration::from_json(&raw).expect("2^53 - 1 decodes");
        assert_eq!(
            decl.memory_budget_breakdown_bytes.model_weights_bytes,
            MEMORY_BUDGET_LINE_MAX_BYTES
        );
    }

    #[test]
    fn above_max_safe_integer_line_value_rejects() {
        let raw = declaration_json(r#"{"model_weights_bytes":9007199254740992}"#);
        MemoryBudgetDeclaration::from_json(&raw).expect_err("2^53 must reject");
    }

    #[test]
    fn far_above_range_line_value_rejects() {
        let raw = declaration_json(r#"{"model_weights_bytes":18446744073709551616}"#);
        MemoryBudgetDeclaration::from_json(&raw).expect_err("2^64 must reject");
    }

    #[test]
    fn silently_rounding_float_token_rejects() {
        // 2^53 + 1 written as a float lexeme parses to a *different* f64
        // integer; capping the domain below 2^53 keeps every silently
        // rounded token out instead of decoding a changed byte count.
        let raw = declaration_json(r#"{"model_weights_bytes":9007199254740993.0}"#);
        MemoryBudgetDeclaration::from_json(&raw).expect_err("rounded float token must reject");
    }

    #[test]
    fn precise_fractional_lexeme_rejects() {
        // 1.0000000000000001 rounds to exactly 1.0 during f64 parsing, so
        // only the lexeme-level check can see the fractional part.
        let raw = declaration_json(r#"{"model_weights_bytes":1.0000000000000001}"#);
        let err = MemoryBudgetDeclaration::from_json(&raw)
            .expect_err("high-precision fractional token must reject");
        assert!(
            matches!(err, MemoryBudgetError::InvalidNumberLexeme { .. }),
            "expected a lexeme-level rejection, got: {err:?}"
        );
        assert!(err.to_string().contains("fractional"), "reason: {err}");
    }

    #[test]
    fn precise_below_one_lexeme_rejects() {
        // 0.9999999999999999999 rounds to exactly 1.0 in f64; the lexeme
        // check rejects it as fractional regardless.
        let raw = declaration_json(r#"{"model_weights_bytes":0.9999999999999999999}"#);
        MemoryBudgetDeclaration::from_json(&raw)
            .expect_err("sub-integer high-precision token must reject");
    }

    #[test]
    fn exponent_lexemes_follow_their_mathematical_value() {
        // Exponent notation is judged by exact decimal value, matching the
        // Draft-07 data model: 1e3 and 1.5e1 are integers; 2.5e-1 is not.
        let accepted = declaration_json(r#"{"model_weights_bytes":1e3,"cache_bytes":1.5e1}"#);
        let decl = MemoryBudgetDeclaration::from_json(&accepted).expect("integral exponents");
        assert_eq!(decl.memory_budget_breakdown_bytes.model_weights_bytes, 1000);
        assert_eq!(decl.memory_budget_breakdown_bytes.cache_bytes, 15);

        let fractional = declaration_json(r#"{"model_weights_bytes":2.5e-1}"#);
        MemoryBudgetDeclaration::from_json(&fractional).expect_err("2.5e-1 is fractional");

        let too_large = declaration_json(r#"{"model_weights_bytes":1e16}"#);
        MemoryBudgetDeclaration::from_json(&too_large).expect_err("1e16 exceeds the domain");
    }

    #[test]
    fn zero_spellings_accepted() {
        // Exact zero is in-domain in any lexical form, including -0.
        let raw = declaration_json(
            r#"{"model_weights_bytes":1,"cache_bytes":-0,"io_buffer_bytes":0.0,"os_reserve_bytes":0e9}"#,
        );
        let decl = MemoryBudgetDeclaration::from_json(&raw).expect("zero spellings decode");
        assert_eq!(decl.memory_budget_breakdown_bytes.cache_bytes, 0);
        assert_eq!(decl.memory_budget_breakdown_bytes.io_buffer_bytes, 0);
        assert_eq!(decl.memory_budget_breakdown_bytes.os_reserve_bytes, 0);
    }

    #[test]
    fn overflowing_exponents_reject_without_panic() {
        // i64::MAX exponent: position arithmetic must saturate into the
        // out-of-range rejection, not overflow.
        let raw = declaration_json(r#"{"model_weights_bytes":10e9223372036854775807}"#);
        let err = MemoryBudgetDeclaration::from_json(&raw).expect_err("must fail closed");
        assert!(matches!(err, MemoryBudgetError::InvalidNumberLexeme { .. }));
        assert!(err.to_string().contains("exceeds"), "reason: {err}");

        // i64::MIN exponent with the significand right of the decimal
        // point: negative saturation maps to the fractional rejection.
        let raw = declaration_json(r#"{"model_weights_bytes":0.001e-9223372036854775808}"#);
        let err = MemoryBudgetDeclaration::from_json(&raw).expect_err("must fail closed");
        assert!(err.to_string().contains("fractional"), "reason: {err}");

        // A positive exponent that does not even fit i64 is classified
        // out-of-range at the lexeme layer.
        let raw = declaration_json(r#"{"model_weights_bytes":1e99999999999999999999}"#);
        let err = MemoryBudgetDeclaration::from_json(&raw).expect_err("must fail closed");
        assert!(err.to_string().contains("exceeds"), "reason: {err}");
    }

    #[test]
    fn negative_exponent_overflow_rejects_as_fractional() {
        // The exponent is below i64::MIN, so f64 parsing would underflow
        // the value to 0.0 and fail OPEN with an accepted zero byte count.
        // The lexeme check classifies the nonzero mantissa as fractional.
        let raw = declaration_json(r#"{"model_weights_bytes":1e-9223372036854775809}"#);
        let err = MemoryBudgetDeclaration::from_json(&raw).expect_err("must fail closed");
        assert!(matches!(err, MemoryBudgetError::InvalidNumberLexeme { .. }));
        assert!(err.to_string().contains("fractional"), "reason: {err}");

        // Exact-zero spellings keep their exemption even with an
        // overflowing exponent: the value is zero, not fractional.
        let raw =
            declaration_json(r#"{"model_weights_bytes":1,"cache_bytes":0e-9223372036854775809}"#);
        let decl = MemoryBudgetDeclaration::from_json(&raw).expect("zero mantissa stays zero");
        assert_eq!(decl.memory_budget_breakdown_bytes.cache_bytes, 0);
    }

    #[test]
    fn float_spelling_of_domain_max_decodes_exactly() {
        // Without canonicalization, serde_json's two-step f64 parse lands
        // "9007199254740991.0" on the NEIGHBORING integer ...990 — a
        // silently corrupted byte count. The decoded value must be the
        // exact declared one, for plain-float and exponent spellings.
        let raw = declaration_json(r#"{"model_weights_bytes":9007199254740991.0}"#);
        let decl = MemoryBudgetDeclaration::from_json(&raw).expect("exact decode");
        assert_eq!(
            decl.memory_budget_breakdown_bytes.model_weights_bytes,
            MEMORY_BUDGET_LINE_MAX_BYTES
        );

        let raw = declaration_json(r#"{"model_weights_bytes":618153020126875200e-2}"#);
        let decl = MemoryBudgetDeclaration::from_json(&raw).expect("exact decode");
        assert_eq!(
            decl.memory_budget_breakdown_bytes.model_weights_bytes,
            6_181_530_201_268_752
        );
    }

    #[test]
    fn trailing_zero_float_spellings_decode_exactly() {
        // Trailing fractional zeros push serde's f64 significand past 2^53
        // and used to round to a NON-integral double, false-rejecting a
        // schema-valid ~962 MB declaration with an error naming a value
        // that appears nowhere in the input.
        let raw = declaration_json(r#"{"model_weights_bytes":962147477.0000000000}"#);
        let decl = MemoryBudgetDeclaration::from_json(&raw).expect("exact decode");
        assert_eq!(
            decl.memory_budget_breakdown_bytes.model_weights_bytes,
            962_147_477
        );

        let raw = declaration_json(r#"{"model_weights_bytes":79727479823099.000}"#);
        let decl = MemoryBudgetDeclaration::from_json(&raw).expect("exact decode");
        assert_eq!(
            decl.memory_budget_breakdown_bytes.model_weights_bytes,
            79_727_479_823_099
        );
    }

    #[test]
    fn long_integral_spelling_accepted() {
        // "1." + 63 zeros is 65 characters and exactly 1; length alone is
        // never a rejection reason.
        let token = format!("1.{}", "0".repeat(63));
        let raw = declaration_json(&format!(r#"{{"model_weights_bytes":{token}}}"#));
        let decl = MemoryBudgetDeclaration::from_json(&raw).expect("long integral spelling");
        assert_eq!(decl.memory_budget_breakdown_bytes.model_weights_bytes, 1);
    }

    #[test]
    fn long_fractional_spelling_rejects() {
        // Same length class, but with a significant digit right of the
        // point: fractional no matter how long the spelling.
        let token = format!("1.{}1", "0".repeat(70));
        let raw = declaration_json(&format!(r#"{{"model_weights_bytes":{token}}}"#));
        let err = MemoryBudgetDeclaration::from_json(&raw).expect_err("fractional must reject");
        assert!(err.to_string().contains("fractional"), "reason: {err}");
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
