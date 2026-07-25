// SPDX-License-Identifier: Apache-2.0
//
// Shared JSON numeric-safety helpers for config schemas that carry byte
// counts.
//
// Two hazards these close, both of which let a document mean one thing and
// decode as another:
//
//   * `serde_json` without the `float_roundtrip` feature parses number
//     tokens through `f64` with two roundings, so a float spelling of an
//     exact integer can land on a NEIGHBORING integer
//     (`9007199254740991.0` decodes as `...990`).
//   * `f64` also erases fractional precision before any `fract()` check
//     can see it (`1.0000000000000001` becomes `1.0`).
//
// Validating each number token's exact decimal lexeme before parsing, and
// decoding from that exact value, keeps declared byte counts intact.

use serde::Deserialize;

/// Upper bound (inclusive) for byte-valued fields: 2^53 - 1, the largest
/// integer IEEE-754 doubles — and therefore every mainstream JSON parser —
/// represent exactly. Bounding the domain here means a declared byte count
/// can never be silently rounded to a different integer on its way through
/// a JSON pipeline (~9.0 PB, far above any real budget).
pub const MAX_SAFE_BYTES: u64 = (1 << 53) - 1;

const MAX_SAFE_BYTES_F64: f64 = 9_007_199_254_740_991.0;

/// Parsed exponent of a number lexeme. Well-formed exponents that overflow
/// `i64` are classified by sign so they fail closed: deferring them to f64
/// parsing would silently underflow tiny nonzero values to an accepted
/// zero.
enum LexemeExponent {
    Value(i64),
    NegativeOverflow,
    PositiveOverflow,
}

/// Scan a document and rewrite every number token to its canonical integer
/// spelling, validating each token's exact decimal lexeme against the
/// byte-value domain first.
///
/// Returns `Err((token, reason))` for a token outside the domain. Callers
/// wrap that in their own typed error. Malformed tokens pass through
/// verbatim for serde to report as grammar errors.
///
/// **Caller invariant:** every number token in the document must be a byte
/// value. Documents that mix byte counts with unrelated numbers need a
/// field-scoped check instead.
pub fn canonicalize_byte_lexemes(raw: &str) -> Result<String, (String, &'static str)> {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut flushed = 0;
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
                match lexeme_exact_value(token) {
                    Ok(Some(value)) => {
                        // Number-token boundaries are ASCII bytes, so both
                        // slices sit on character boundaries.
                        out.push_str(&raw[flushed..start]);
                        out.push_str(&value.to_string());
                        flushed = i;
                    }
                    Ok(None) => {} // grammar error: serde's to report
                    Err(reason) => return Err((token.to_string(), reason)),
                }
            }
            _ => i += 1,
        }
    }
    out.push_str(&raw[flushed..]);
    Ok(out)
}

/// Exact decimal evaluation of one number lexeme against the byte-value
/// domain: non-negative, mathematically integral, at most
/// [`MAX_SAFE_BYTES`]. Returns `Ok(Some(value))` for an in-domain token,
/// `Ok(None)` when the token is not a valid JSON number (serde reports the
/// grammar error instead), and `Err(reason)` for a domain violation.
pub fn lexeme_exact_value(token: &str) -> Result<Option<u64>, &'static str> {
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
                // closed by sign, below) from a grammar error.
                let magnitude = e.strip_prefix(['+', '-']).unwrap_or(e);
                if magnitude.is_empty() || !magnitude.bytes().all(|b| b.is_ascii_digit()) {
                    return Ok(None);
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
        return Ok(None);
    }

    // value = <concatenated digits> x 10^(exponent - frac len). The digit
    // at index `i` has decimal position `int_len - 1 - i + exponent`;
    // position 0 is the units place.
    let digits: Vec<u8> = int_digits.bytes().chain(frac_digits.bytes()).collect();
    let Some(first_nonzero) = digits.iter().position(|&b| b != b'0') else {
        // Exact zero in any spelling (0, -0, 0.0, 0e9) is in-domain.
        return Ok(Some(0));
    };
    let Some(last_nonzero) = digits.iter().rposition(|&b| b != b'0') else {
        return Ok(None);
    };
    if negative {
        return Err("negative values are not in the byte-value domain");
    }
    // The mantissa is nonzero past this point, so an exponent overflowing
    // i64 fails closed by sign instead of deferring to f64 parsing (which
    // underflows tiny values to an accepted zero).
    let exponent = match exponent {
        LexemeExponent::Value(v) => v,
        LexemeExponent::NegativeOverflow => {
            return Err("fractional values are not in the byte-value domain")
        }
        LexemeExponent::PositiveOverflow => {
            return Err("exceeds the largest exactly-representable byte value (2^53 - 1)")
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
        return Err("fractional values are not in the byte-value domain");
    }
    if position(first_nonzero) >= 16 {
        return Err("exceeds the largest exactly-representable byte value (2^53 - 1)");
    }
    // The significant span covers at most 16 decimal positions, so the
    // accumulation below cannot overflow u64.
    let mut value: u64 = 0;
    let Some(span) = digits.get(first_nonzero..=last_nonzero) else {
        return Ok(None);
    };
    for &b in span {
        value = value * 10 + u64::from(b - b'0');
    }
    for _ in 0..position(last_nonzero) {
        value *= 10;
    }
    if value > MAX_SAFE_BYTES {
        return Err("exceeds the largest exactly-representable byte value (2^53 - 1)");
    }
    Ok(Some(value))
}

/// Deserialize one byte-valued field into `[0, 2^53)`.
///
/// Draft-07 `type: integer` treats any number with a zero fractional part
/// as an integer (`4096.0` validates), so integral floats inside the range
/// are accepted; fractional, negative, or out-of-range values are
/// rejected. This is the serde-level backstop: documents decoded after
/// [`canonicalize_byte_lexemes`] only ever reach the exact `u64` path.
pub fn deserialize_safe_bytes<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let n = serde_json::Number::deserialize(deserializer)?;
    if let Some(v) = n.as_u64() {
        if v <= MAX_SAFE_BYTES {
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
        if (0.0..=MAX_SAFE_BYTES_F64).contains(&f) && f.fract() == 0.0 {
            return Ok(f as u64);
        }
    }
    Err(serde::de::Error::custom(format!(
        "byte values must be integers in [0, 2^53), got {n}"
    )))
}

/// Deserialize an optional byte-valued field. Absent stays `None`; present
/// goes through [`deserialize_safe_bytes`].
pub fn deserialize_optional_safe_bytes<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct Wrapper(#[serde(deserialize_with = "deserialize_safe_bytes")] u64);

    Option::<Wrapper>::deserialize(deserializer).map(|opt| opt.map(|w| w.0))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{canonicalize_byte_lexemes, lexeme_exact_value, MAX_SAFE_BYTES};

    #[test]
    fn exact_values_survive_float_spellings() {
        assert_eq!(
            lexeme_exact_value("9007199254740991.0"),
            Ok(Some(MAX_SAFE_BYTES))
        );
        assert_eq!(
            lexeme_exact_value("962147477.0000000000"),
            Ok(Some(962_147_477))
        );
        assert_eq!(lexeme_exact_value("1e3"), Ok(Some(1000)));
        assert_eq!(lexeme_exact_value("0e-9223372036854775809"), Ok(Some(0)));
    }

    #[test]
    fn out_of_domain_lexemes_reject() {
        for token in [
            "-1",
            "1.5",
            "1.0000000000000001",
            "9007199254740992",
            "1e16",
            "10e9223372036854775807",
            "1e-9223372036854775809",
        ] {
            assert!(
                lexeme_exact_value(token).is_err(),
                "`{token}` must be out of domain"
            );
        }
    }

    #[test]
    fn canonicalization_rewrites_only_number_tokens() {
        let raw = r#"{"note":"1.5 is text","size":4096.0}"#;
        let canonical = canonicalize_byte_lexemes(raw).expect("in domain");
        assert_eq!(canonical, r#"{"note":"1.5 is text","size":4096}"#);
    }

    #[test]
    fn canonicalization_reports_the_offending_token() {
        let (token, reason) =
            canonicalize_byte_lexemes(r#"{"size":1.5}"#).expect_err("fractional rejects");
        assert_eq!(token, "1.5");
        assert!(reason.contains("fractional"), "reason: {reason}");
    }
}
