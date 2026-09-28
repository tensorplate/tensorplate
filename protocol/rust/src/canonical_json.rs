// SPDX-License-Identifier: Apache-2.0
//
// Canonical JSON, version 1: the byte form the deployment descriptor's
// digests are computed over. Specified in `docs/bundles/integrity.md`;
// `protocol/fixtures/canonical_json.json` holds the vectors every
// implementation replays.

use std::cell::Cell;
use std::fmt;

use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use sha2::{Digest, Sha256};

use crate::json_numbers::MAX_SAFE_BYTES;

/// The canonical form this module writes. A document hashed under another
/// version is refused, not re-hashed.
pub const CANONICAL_JSON_VERSION: u64 = 1;

/// Deepest nesting of arrays and objects the form accepts. Below
/// `serde_json`'s own limit, so text and in-memory values refuse alike.
pub const MAX_CANONICAL_DEPTH: usize = 64;

/// Why a JSON text or value has no canonical form.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CanonicalJsonError {
    /// Not JSON text: a grammar error, trailing text, or a string escape
    /// that is not a Unicode scalar value.
    #[error("not JSON text: {0}")]
    Malformed(String),

    /// An object names the same key twice, compared after unescaping.
    #[error("object key `{0}` appears more than once")]
    DuplicateKey(String),

    /// Arrays and objects nest deeper than [`MAX_CANONICAL_DEPTH`].
    #[error("arrays and objects nest more than {MAX_CANONICAL_DEPTH} deep")]
    Depth,

    /// `null` appears; an absent value is omitted instead.
    #[error("null has no canonical form; an absent value is omitted")]
    Null,

    /// A number outside the integer domain.
    #[error("number {value} {problem}")]
    Number {
        value: String,
        problem: &'static str,
    },
}

impl CanonicalJsonError {
    /// The reason name the cross-language vectors use.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Malformed(_) => "malformed",
            Self::DuplicateKey(_) => "duplicate_key",
            Self::Depth => "depth",
            Self::Null => "null",
            Self::Number { .. } => "number",
        }
    }
}

/// A parsed value that keeps what the canonical form rejects (nulls,
/// non-integers, repeated keys) so the writer can refuse each by name.
enum Node {
    Null,
    Bool(bool),
    Unsigned(u64),
    Signed(i64),
    Float(f64),
    String(String),
    Array(Vec<Node>),
    Object(Vec<(String, Node)>),
}

/// Reads one value at `depth` containers deep, raising `too_deep` when a
/// container would exceed [`MAX_CANONICAL_DEPTH`].
#[derive(Clone, Copy)]
struct NodeSeed<'a> {
    depth: usize,
    too_deep: &'a Cell<bool>,
}

impl NodeSeed<'_> {
    fn enter<E: de::Error>(self) -> Result<Self, E> {
        if self.depth >= MAX_CANONICAL_DEPTH {
            self.too_deep.set(true);
            return Err(E::custom("nesting too deep"));
        }
        Ok(NodeSeed {
            depth: self.depth + 1,
            ..self
        })
    }
}

impl<'de> DeserializeSeed<'de> for NodeSeed<'_> {
    type Value = Node;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Node, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for NodeSeed<'_> {
    type Value = Node;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_unit<E>(self) -> Result<Node, E> {
        Ok(Node::Null)
    }

    fn visit_none<E>(self) -> Result<Node, E> {
        Ok(Node::Null)
    }

    fn visit_bool<E>(self, v: bool) -> Result<Node, E> {
        Ok(Node::Bool(v))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Node, E> {
        Ok(Node::Unsigned(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Node, E> {
        Ok(Node::Signed(v))
    }

    // serde_json hands a number here when its text has a fraction or an
    // exponent, is `-0`, or does not fit 64 bits.
    fn visit_f64<E>(self, v: f64) -> Result<Node, E> {
        Ok(Node::Float(v))
    }

    fn visit_str<E>(self, v: &str) -> Result<Node, E> {
        Ok(Node::String(v.to_owned()))
    }

    fn visit_string<E>(self, v: String) -> Result<Node, E> {
        Ok(Node::String(v))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Node, A::Error> {
        let inner = self.enter()?;
        let mut items = Vec::new();
        while let Some(item) = seq.next_element_seed(inner)? {
            items.push(item);
        }
        Ok(Node::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Node, A::Error> {
        let inner = self.enter()?;
        let mut members = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            members.push((key, map.next_value_seed(inner)?));
        }
        Ok(Node::Object(members))
    }
}

fn read_node<'de, D: Deserializer<'de, Error = serde_json::Error>>(
    deserializer: D,
    too_deep: &Cell<bool>,
) -> Result<Node, CanonicalJsonError> {
    NodeSeed { depth: 0, too_deep }
        .deserialize(deserializer)
        .map_err(|e| refusal(&e, too_deep))
}

// serde_json refuses a number beyond a double's range while parsing, before
// the value reaches `visit_f64`; the form refuses it as a number.
fn refusal(e: &serde_json::Error, too_deep: &Cell<bool>) -> CanonicalJsonError {
    if too_deep.get() {
        CanonicalJsonError::Depth
    } else if e.to_string().starts_with("number out of range") {
        CanonicalJsonError::Number {
            value: format!("at line {} column {}", e.line(), e.column()),
            problem: "is outside the range a double holds",
        }
    } else {
        CanonicalJsonError::Malformed(e.to_string())
    }
}

/// The canonical bytes of the JSON text `text`.
///
/// # Errors
///
/// [`CanonicalJsonError`] naming the first construct with no canonical
/// form.
pub fn canonicalize(text: &str) -> Result<Vec<u8>, CanonicalJsonError> {
    let too_deep = Cell::new(false);
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let node = read_node(&mut deserializer, &too_deep)?;
    deserializer
        .end()
        .map_err(|e| CanonicalJsonError::Malformed(e.to_string()))?;
    let mut out = Vec::new();
    write_node(&node, &mut out)?;
    Ok(out)
}

/// The canonical bytes of an in-memory value, such as a serialized
/// protocol type.
///
/// # Errors
///
/// [`CanonicalJsonError`] for a null, a number outside the integer domain
/// or nesting deeper than [`MAX_CANONICAL_DEPTH`].
pub fn canonicalize_value(value: &serde_json::Value) -> Result<Vec<u8>, CanonicalJsonError> {
    let node = read_node(value, &Cell::new(false))?;
    let mut out = Vec::new();
    write_node(&node, &mut out)?;
    Ok(out)
}

/// `sha256:` and the lowercase hex SHA-256 of `canonical`.
#[must_use]
pub fn sha256_digest(canonical: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(canonical)))
}

fn write_node(node: &Node, out: &mut Vec<u8>) -> Result<(), CanonicalJsonError> {
    match node {
        Node::Null => return Err(CanonicalJsonError::Null),
        Node::Bool(v) => out.extend_from_slice(if *v { b"true" } else { b"false" }),
        Node::Unsigned(v) => {
            if *v > MAX_SAFE_BYTES {
                return Err(out_of_range(v));
            }
            out.extend_from_slice(v.to_string().as_bytes());
        }
        Node::Signed(v) => {
            if v.unsigned_abs() > MAX_SAFE_BYTES {
                return Err(out_of_range(v));
            }
            out.extend_from_slice(v.to_string().as_bytes());
        }
        Node::Float(v) => {
            let problem = if v.to_bits() == (-0.0_f64).to_bits() {
                "is -0, which is written 0"
            } else if v.abs() > 9_007_199_254_740_991.0 {
                "is outside [-(2^53-1), 2^53-1]"
            } else {
                "is written with a fraction or an exponent"
            };
            return Err(CanonicalJsonError::Number {
                value: v.to_string(),
                problem,
            });
        }
        Node::String(v) => write_string(v, out),
        Node::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_node(item, out)?;
            }
            out.push(b']');
        }
        Node::Object(members) => {
            // `str` orders by UTF-8 bytes, which is code point order.
            let mut sorted: Vec<&(String, Node)> = members.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            if let Some(pair) = sorted.windows(2).find(|pair| pair[0].0 == pair[1].0) {
                return Err(CanonicalJsonError::DuplicateKey(pair[0].0.clone()));
            }
            out.push(b'{');
            for (i, (key, value)) in sorted.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_string(key, out);
                out.push(b':');
                write_node(value, out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

fn out_of_range(value: &impl fmt::Display) -> CanonicalJsonError {
    CanonicalJsonError::Number {
        value: value.to_string(),
        problem: "is outside [-(2^53-1), 2^53-1]",
    }
}

fn write_string(value: &str, out: &mut Vec<u8>) {
    out.push(b'"');
    for c in value.chars() {
        match c {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\u{8}' => out.extend_from_slice(b"\\b"),
            '\t' => out.extend_from_slice(b"\\t"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\u{c}' => out.extend_from_slice(b"\\f"),
            '\r' => out.extend_from_slice(b"\\r"),
            c if u32::from(c) < 0x20 => {
                out.extend_from_slice(format!("\\u{:04x}", u32::from(c)).as_bytes());
            }
            c => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
        }
    }
    out.push(b'"');
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use serde_json::json;

    use super::{canonicalize, canonicalize_value, sha256_digest, CanonicalJsonError};

    #[test]
    fn values_and_texts_share_one_form() {
        let text = r#"{"b":[1,-2,"é"],"a":{"d":true,"c":false}}"#;
        let from_text = canonicalize(text).expect("text");
        let value: serde_json::Value = serde_json::from_str(text).expect("parse");
        assert_eq!(canonicalize_value(&value).expect("value"), from_text);
        assert_eq!(
            from_text,
            r#"{"a":{"c":false,"d":true},"b":[1,-2,"é"]}"#.as_bytes()
        );
    }

    #[test]
    fn value_input_refuses_null_and_floats() {
        assert_eq!(
            canonicalize_value(&json!({"a": null})),
            Err(CanonicalJsonError::Null)
        );
        let err = canonicalize_value(&json!([2.0])).expect_err("float");
        assert_eq!(err.reason(), "number");
    }

    #[test]
    fn values_nest_no_deeper_than_text() {
        let mut value = json!(1);
        for _ in 0..super::MAX_CANONICAL_DEPTH {
            value = json!([value]);
        }
        assert!(canonicalize_value(&value).is_ok());
        assert_eq!(
            canonicalize_value(&json!([value])),
            Err(CanonicalJsonError::Depth)
        );
    }

    #[test]
    fn digest_is_lowercase_sha256_of_the_bytes() {
        assert_eq!(
            sha256_digest(b"{}"),
            "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        );
    }
}
