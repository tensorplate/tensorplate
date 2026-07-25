// SPDX-License-Identifier: Apache-2.0
//
// Shared serde shape helpers for config-schema mirrors.
//
// Serde's derive is shape-permissive in ways JSON Schema is not: a derived
// `Deserialize` for a struct accepts the sequence form (`["a", "b"]`) in
// addition to the map form, and `deny_unknown_fields` does not cover that
// path. A schema that pins `type: "object"` is therefore stricter than its
// own Rust mirror unless the mirror pins the shape too.

use serde::Deserialize;

/// Deserialize `T` from a JSON object only, rejecting the sequence form
/// that serde's derive would otherwise accept.
///
/// Apply it to a private wire type and convert in a custom `Deserialize`
/// impl, so no decoding path — including generic serde loaders — can skip
/// the shape check.
///
/// Object-encoding formats only. Formats that encode structs as sequences
/// (bincode, postcard, MessagePack in compact mode) cannot decode types
/// routed through this helper, which is correct for a JSON config schema
/// but would need revisiting before use with such a format.
pub fn deserialize_map_only<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct MapOnly<T>(std::marker::PhantomData<T>);

    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for MapOnly<T> {
        type Value = T;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a JSON object")
        }

        fn visit_map<A>(self, map: A) -> Result<T, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            T::deserialize(serde::de::value::MapAccessDeserializer::new(map))
        }
    }

    deserializer.deserialize_map(MapOnly(std::marker::PhantomData))
}

/// [`deserialize_map_only`] for an optional field: absent stays `None`,
/// present must be an object.
pub fn deserialize_optional_map_only<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Wrapper<T>(T);

    impl<'de, T: Deserialize<'de>> Deserialize<'de> for Wrapper<T> {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserialize_map_only(deserializer).map(Wrapper)
        }
    }

    Option::<Wrapper<T>>::deserialize(deserializer).map(|opt| opt.map(|w| w.0))
}

/// [`deserialize_map_only`] for every element of a sequence field: the
/// field itself is an array, but each element must be an object.
pub fn deserialize_vec_map_only<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Wrapper<T>(T);

    impl<'de, T: Deserialize<'de>> Deserialize<'de> for Wrapper<T> {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserialize_map_only(deserializer).map(Wrapper)
        }
    }

    Vec::<Wrapper<T>>::deserialize(deserializer)
        .map(|items| items.into_iter().map(|w| w.0).collect())
}

/// Reject identifiers that differ only by case or stray whitespace, so
/// identifier resolution across a registry is unambiguous. Mirrors the
/// schema pattern `^[a-z0-9]+(-[a-z0-9]+)*$`: lowercase alphanumeric
/// segments joined by single hyphens.
#[must_use]
pub fn is_canonical_identifier(id: &str) -> bool {
    !id.is_empty()
        && id.split('-').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// Same rule as [`is_canonical_identifier`] with `_` as the separator, for
/// lower_snake_case component and path identifiers.
#[must_use]
pub fn is_canonical_snake_identifier(id: &str) -> bool {
    !id.is_empty()
        && id.split('_').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

#[cfg(test)]
mod tests {
    use super::{is_canonical_identifier, is_canonical_snake_identifier};

    #[test]
    fn canonical_identifiers_accept_lowercase_hyphenated() {
        for id in ["jetson-orin", "gcp-nvidia-a100-40gb", "a", "a1"] {
            assert!(is_canonical_identifier(id), "`{id}` should be canonical");
        }
    }

    #[test]
    fn canonical_identifiers_reject_near_duplicates() {
        for id in [
            "",
            "Jetson-Orin",
            "jetson-orin ",
            "jetson--orin",
            "-jetson",
            "jetson_orin",
        ] {
            assert!(!is_canonical_identifier(id), "`{id}` should reject");
        }
    }

    #[test]
    fn snake_identifiers_follow_the_same_rule() {
        assert!(is_canonical_snake_identifier("python_pytorch"));
        assert!(!is_canonical_snake_identifier("python-pytorch"));
        assert!(!is_canonical_snake_identifier("Python_PyTorch"));
        assert!(!is_canonical_snake_identifier("python__pytorch"));
    }
}
