# Bundle Integrity

**Status:** v0.1.0 (bundle format)
**Code:** [`protocol/rust/src/bundle.rs`](../../protocol/rust/src/bundle.rs)

The parser verifies integrity in three layers:

1. **Artifact digests** — every artifact in `manifest.artifacts[]` must publish
   a `sha256:hex` digest. The parser opens the file and streams it through
   `sha2::Sha256`; a mismatch raises [`ParseError::ArtifactDigestMismatch`].
2. **Manifest self-digest** — when `manifest_digest` is set, the parser
   computes the *canonical* manifest digest (see below) with the field
   stripped, and compares them. Mismatches raise
   [`ParseError::ManifestDigestMismatch`].
3. **Format version** — `format_version` must use the runtime's supported
   major. Unknown future majors raise
   [`ParseError::UnsupportedFormatVersion`].

The verifier does **not** require the optional `signature` block. When
present, the parser checks shape (non-empty `algorithm` and `value`) but
v0.1.0 does not verify the cryptographic signature itself. Hosted
provenance verification is explicitly out of scope (see
[non-goals](#non-goals)).

---

## Canonical manifest digest

The canonical manifest digest is the sha256 of the *manifest JSON value
with `manifest_digest` stripped*, serialized through `serde_json::to_vec`.
That serialization preserves key order and omits whitespace, so two
manifest files whose only difference is whitespace and field ordering
produce the same digest as long as the underlying JSON object is the
same.

Pseudocode:

```text
value = parse_json(manifest_bytes)
value.as_object_mut().remove("manifest_digest")
sha256("sha256:" + hex(sha256(serde_json::to_vec(value))))
```

Bundle authoring tools must use the same canonicalization when computing
`manifest_digest` for the manifest body. The
[`tools/bundle/`](../../tools/bundle/) helper exposes
`compute_canonical_manifest_digest` so out-of-tree tools can match the
parser exactly.

---

## Digest algorithm

v0.1.0 accepts only `sha256`. Other algorithms (e.g., `sha512`, `blake3`)
parse cleanly into the manifest but the parser rejects them at integrity
verification time with [`ParseError::UnsupportedDigestAlgorithm`]. The
extension space remains so that a later format-major can add an
algorithm without changing the `algo:hex` shape.

---

## Optional signature

```json
"signature": {
  "algorithm": "ed25519",
  "key_id":    "tensorplate-release-2026",
  "value":     "base64..."
}
```

The parser:

- requires `algorithm` and `value` to be non-empty when the field is set,
- treats `key_id` as a bounded diagnostic field (≤ 128 bytes),
- does **not** verify the signature in v0.1.0 — verification is reserved
  for the hosted provenance layer.

Bundle consumers may also store the signature in
`provenance/signature.json` inside the bundle. The path constant
`SIGNATURE_FILENAME` documents the reserved location.

---

## Optional provenance

```json
"provenance": {
  "builder":         "tensorplate-bundle-tool 0.1.0",
  "build_url":       "https://...",
  "source_commit":   "...",
  "build_timestamp": "2026-05-20T12:00:00Z",
  "sbom": { "format": "spdx", "path": "provenance/sbom.json", "digest": "sha256:..." }
}
```

When an SBOM reference is present, its `digest` must be in `algo:hex`
form. The parser does not enforce that the file exists at `path`; the
optional asset digest verifier checks file presence and digest when the
artifact is listed in `manifest.artifacts[]`.

---

## Non-goals

- Cryptographic signature verification (deferred to hosted provenance).
- Public-key distribution and rotation.
- TUF / Sigstore integration.
- Reproducible bundle layout (deterministic archive packing is reserved
  by the layout but not enforced).
