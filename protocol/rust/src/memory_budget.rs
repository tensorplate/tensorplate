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

/// Upper bound (inclusive) for every byte line: 2^53 - 1, the largest
/// integer that IEEE-754 doubles — and therefore every mainstream JSON
/// parser — represent exactly. Bounding the domain here means a declared
/// byte count can never be silently rounded to a different integer on its
/// way through a JSON pipeline (~9.0 PB, far above any row budget).
pub const MEMORY_BUDGET_LINE_MAX_BYTES: u64 = (1 << 53) - 1;

const MEMORY_BUDGET_LINE_MAX_BYTES_F64: f64 = 9_007_199_254_740_991.0;

/// Scan the raw document and validate every number token's exact decimal
/// lexeme against the byte-line domain. Without the (workspace-global)
/// `arbitrary_precision` feature, `serde_json` parses number tokens through
/// `f64`, which silently rounds high-precision lexemes — `1.0000000000000001`
/// becomes `1.0` — before any `fract()` check can see them. Scanning the
/// lexemes first keeps the fail-closed fractional/range guarantees exact.
/// Every number in a well-formed declaration is a byte line, so the rule
/// applies to all number tokens; malformed tokens are left for serde to
/// report as grammar errors.
fn validate_number_lexemes(raw: &str) -> Result<(), MemoryBudgetError> {
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                // Skip string contents, honoring escapes.
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i += 2,
                        b'"' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
            }
            b'-' | b'0'..=b'9' => {
                let start = i;
                while i < bytes.len()
                    && matches!(bytes[i], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
                {
                    i += 1;
                }
                let token = &raw[start..i];
                if let Some(reason) = lexeme_domain_violation(token) {
                    return Err(MemoryBudgetError::InvalidNumberLexeme {
                        token: token.to_string(),
                        reason,
                    });
                }
            }
            _ => i += 1,
        }
    }
    Ok(())
}

/// Parsed exponent of a number lexeme. Well-formed exponents that overflow
/// `i64` are classified by sign so they fail closed at the lexeme layer:
/// deferring them to f64 parsing would silently underflow tiny nonzero
/// values (e.g. `1e-9223372036854775809`) to an accepted zero.
enum LexemeExponent {
    Value(i64),
    NegativeOverflow,
    PositiveOverflow,
}

/// Exact decimal check of one number lexeme against the byte-line domain:
/// non-negative, mathematically integral, at most 2^53 - 1. Returns the
/// violation, or `None` when the token is in-domain — or is not a valid
/// JSON number at all, which serde reports as a grammar error instead.
fn lexeme_domain_violation(token: &str) -> Option<&'static str> {
    let (negative, rest) = match token.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, token),
    };
    let (mantissa, exponent) = match rest.split_once(['e', 'E']) {
        Some((m, e)) => {
            let exp = if let Ok(v) = e.parse::<i64>() {
                LexemeExponent::Value(v)
            } else {
                // Distinguish a well-formed exponent overflowing i64 (fail
                // closed by sign, below) from a grammar error (serde's to
                // report).
                let magnitude = e.strip_prefix(['+', '-']).unwrap_or(e);
                if magnitude.is_empty() || !magnitude.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                if e.starts_with('-') {
                    LexemeExponent::NegativeOverflow
                } else {
                    LexemeExponent::PositiveOverflow
                }
            };
            (m, exp)
        }
        None => (rest, LexemeExponent::Value(0)),
    };
    let (int_digits, frac_digits) = match mantissa.split_once('.') {
        Some((i, f)) => (i, f),
        None => (mantissa, ""),
    };
    if int_digits.is_empty()
        || frac_digits.is_empty() && mantissa.contains('.')
        || !int_digits.bytes().all(|b| b.is_ascii_digit())
        || !frac_digits.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }

    // value = <concatenated digits> x 10^(exponent - frac len). The digit
    // at index `i` has decimal position `int_len - 1 - i + exponent`;
    // position 0 is the units place.
    let digits: Vec<u8> = int_digits.bytes().chain(frac_digits.bytes()).collect();
    let Some(first_nonzero) = digits.iter().position(|&b| b != b'0') else {
        return None; // exact zero in any spelling (0, -0, 0.0, 0e9) is in-domain
    };
    let last_nonzero = digits.iter().rposition(|&b| b != b'0')?;
    if negative {
        return Some("negative values are not in the byte-line domain");
    }
    // The mantissa is nonzero past this point, so an exponent overflowing
    // i64 fails closed by sign instead of deferring to f64 parsing (which
    // underflows tiny values to an accepted zero).
    let exponent = match exponent {
        LexemeExponent::Value(v) => v,
        LexemeExponent::NegativeOverflow => {
            return Some("fractional values are not in the byte-line domain")
        }
        LexemeExponent::PositiveOverflow => {
            return Some("exceeds the largest exactly-representable byte value (2^53 - 1)")
        }
    };
    // Token length is unbounded — a spelling like "1." + 63 zeros is
    // exactly 1 and must be accepted — so every arithmetic step saturates:
    // digit counts always fit i64 (they are bounded by the input length),
    // and with an attacker-controlled exponent up to +/- i64::MAX,
    // positive saturation lands above the range check (out-of-range) and
    // negative saturation lands below zero (fractional), never a panic.
    let int_len = i64::try_from(int_digits.len()).unwrap_or(i64::MAX);
    let position = |i: usize| {
        int_len
            .saturating_sub(1)
            .saturating_sub(i64::try_from(i).unwrap_or(i64::MAX))
            .saturating_add(exponent)
    };
    if position(last_nonzero) < 0 {
        return Some("fractional values are not in the byte-line domain");
    }
    if position(first_nonzero) >= 16 {
        return Some("exceeds the largest exactly-representable byte value (2^53 - 1)");
    }
    // The significant span covers at most 16 decimal positions, so the
    // accumulation below cannot overflow u64.
    let mut value: u64 = 0;
    for &b in digits.get(first_nonzero..=last_nonzero)? {
        value = value * 10 + u64::from(b - b'0');
    }
    for _ in 0..position(last_nonzero) {
        value *= 10;
    }
    if value > MEMORY_BUDGET_LINE_MAX_BYTES {
        return Some("exceeds the largest exactly-representable byte value (2^53 - 1)");
    }
    None
}

/// Deserialize one byte-count line into the schema's numeric domain:
/// integers in `[0, 2^53)`. Draft-07 `type: integer` treats any number with
/// a zero fractional part as an integer (`4096.0` validates), so integral
/// floats inside the range are accepted; fractional, negative, or
/// out-of-range values are rejected. This is the serde-level backstop;
/// [`MemoryBudgetDeclaration::from_json`] additionally validates each
/// number token's exact decimal lexeme before parsing, which is what
/// catches tokens that only look integral after IEEE-754 rounding.
fn de_bytes_line<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let n = serde_json::Number::deserialize(deserializer)?;
    if let Some(v) = n.as_u64() {
        if v <= MEMORY_BUDGET_LINE_MAX_BYTES {
            return Ok(v);
        }
    } else if let Some(f) = n.as_f64() {
        // Every integral f64 in [0, 2^53) is exactly representable, so the
        // guarded cast below is lossless.
        #[allow(
            clippy::float_cmp,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        if (0.0..=MEMORY_BUDGET_LINE_MAX_BYTES_F64).contains(&f) && f.fract() == 0.0 {
            return Ok(f as u64);
        }
    }
    Err(serde::de::Error::custom(format!(
        "byte line values must be integers in [0, 2^53), got {n}"
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
    /// [`MEMORY_BUDGET_SCHEMA_VERSION`] and the exact byte-line numeric
    /// domain (validated on each number token's decimal lexeme, immune to
    /// IEEE-754 rounding). All failures are fail-closed typed errors that
    /// map to [`ErrorCode::ConfigInvalid`]. This is the validated entry
    /// point; deserializing the types directly with serde skips the
    /// lexeme-level checks.
    pub fn from_json(json: &str) -> Result<Self, MemoryBudgetError> {
        validate_number_lexemes(json)?;
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
    fn float_bound_constant_matches_integer_bound() {
        // 2^53 - 1 is exactly representable, so the u64 round-trip through
        // the f64 constant is lossless and the comparison stays integral.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let round_tripped = super::MEMORY_BUDGET_LINE_MAX_BYTES_F64 as u64;
        assert_eq!(round_tripped, MEMORY_BUDGET_LINE_MAX_BYTES);
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
