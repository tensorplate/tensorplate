# Bundle Integrity

**Supported bundle formats:** 0.1 and 0.2
**Code:** [`protocol/rust/src/bundle.rs`](../../protocol/rust/src/bundle.rs)

The parser checks the format before verifying integrity:

1. **Format version** — `format_version` must be exactly `0.1` or `0.2`.
   Unknown majors, minors and alternate spellings raise
   [`ParseError::UnsupportedFormatVersion`] before payload decoding or
   artifact access.
2. **Artifact digests** — every artifact in `manifest.artifacts[]` must publish
   a `sha256:hex` digest. The parser opens the file and streams it through
   `sha2::Sha256`; a mismatch raises [`ParseError::ArtifactDigestMismatch`].
3. **Manifest self-digest** — when `manifest_digest` is set, the parser
   computes the *canonical* manifest digest (see below) with the field
   stripped, and compares them. Mismatches raise
   [`ParseError::ManifestDigestMismatch`].

The verifier does **not** require the optional `signature` block. When
present, the parser checks shape (non-empty `algorithm` and `value`) but
v0.1.0 does not verify the cryptographic signature itself. Hosted
provenance verification is explicitly out of scope (see
[non-goals](#non-goals)).

---

## Canonical manifest digest

The canonical manifest digest is the sha256 of the *manifest JSON value
with `manifest_digest` stripped*, serialized through `serde_json::to_vec`.
That serialization writes object keys in sorted order and no whitespace,
so two manifest files whose only difference is whitespace and field
ordering produce the same digest as long as the underlying JSON object is
the same.

Only the top-level `manifest_digest` key is removed. A nested key of the
same name, such as `base_model_ref.manifest_digest`, is part of the value
that is hashed, so a tool that strips by key name at every depth computes a
different digest.

Pseudocode:

```text
value = parse_json(manifest_bytes)
value.as_object_mut().remove("manifest_digest")
"sha256:" + hex(sha256(serde_json::to_vec(value)))
```

This form is not [canonical JSON](#canonical-json-version-1): a manifest
may carry `null` and non-integer numbers, which `serde_json` writes in its
own spelling. Every bundle's digest depends on it, so it stays as it is;
the deployment descriptor's digests below use canonical JSON instead.

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

## Deployment descriptor digests

A verified bundle becomes a deployment through a
[deployment descriptor](../../protocol/schemas/deployment_descriptor.json)
(`tensorplate_protocol::DeploymentDescriptor`), derived for one generation
of one deployment from the verified bundle, the installed backend
descriptor's runner profile and the agent's choices. It carries two
identities:

- **`configuration_digest`** identifies the portable `configuration`: the
  bundle's name, version, format and canonical manifest digest; the
  artifacts with their relative paths and digests; the model class,
  `backend_hint` and precision (written out even when it is `auto`); the
  runtime version; the resolved runner profile's id and packages; the
  format 0.2 compute type, warmup, pipeline stages, per-domain budgets,
  session bound, degraded profile and speech contract; the selected
  session count and quota bytes; and an acceptance profile digest when one
  applies. It holds no generation, absolute path, endpoint, set revision,
  approval status or evidence record, so restarting or redeploying the
  same configuration reproduces it, and changing the quota or the runtime
  changes it.
- **`descriptor_digest`** identifies the generation's whole descriptor:
  the configuration and its digest, plus `deployment_id`, `generation`,
  `admission_mode`, the `staged_path` its artifacts resolve under and the
  runner profile's
  install paths (`runner_environment`). Each generation has its own.

Each digest is `"sha256:"` and the lowercase hex SHA-256 of canonical JSON
version 1: `configuration_digest` of the `configuration` object,
`descriptor_digest` of the descriptor without its `descriptor_digest`
member. Neither input contains its own digest. Hashes are computed in one
direction only: artifacts and profiles, then the bundle, then the
configuration, then the descriptor, then any evidence that names them. A
profile digest names a leaf document and never a bundle, configuration,
descriptor or evidence digest, and a descriptor names no evidence.

A reader checks the canonical form first, then the shape, then decodes the
format 0.2 fields with the manifest's own decoder, checks the descriptor's
rules, requires the text to be exactly what the decoded descriptor
serializes to (every budget line and stage `observable` written out, no
default filled in), and only then compares both digests. The
cross-language fixtures are
`protocol/fixtures/deployment_descriptor_{stt,stt_restart,tts}.json`: the
two speech bundles under `test/models/bundles/v0_2/`, with the restart
fixture keeping the first's `configuration_digest` under a new generation
and `descriptor_digest`.

---

## Canonical JSON version 1

The byte form the descriptor digests are computed over. A value has a
canonical form only when it holds objects, arrays, strings, `true`,
`false` and integers; the form is:

- **Objects**: members sorted by key, comparing keys as sequences of
  Unicode code points (equivalently, their UTF-8 bytes; not UTF-16 code
  units), `{`, `:`, `,`, `}` and no whitespace. A key appears once,
  compared after unescaping.
- **Arrays**: elements in their given order, `[`, `,`, `]`.
- **Strings**: UTF-8. `"` and `\` are escaped as `\"` and `\\`; U+0008,
  U+0009, U+000A, U+000C and U+000D as `\b`, `\t`, `\n`, `\f` and `\r`;
  every other code point below U+0020 as `\u00` and two lowercase hex
  digits. Everything else, `/`, U+007F and U+2028 included, is written as
  itself.
- **Integers**: in [-(2^53-1), 2^53-1], in shortest decimal form: an
  optional `-`, no leading zero, no fraction, no exponent; zero is `0`.
- **Nesting**: arrays and objects nest at most 64 deep.
- **Absence**: an optional member that has no value is omitted; `null`
  has no canonical form.

Text has a canonical form only when it is one JSON value (no trailing text
or byte order mark), nested at most 64 deep, whose strings are Unicode
scalar values and whose numbers are integers written without a fraction or
an exponent, not `-0` and within the range above. A reader refuses anything else rather than
normalizing it, so two implementations never hash different readings of
the same bytes. `protocol/fixtures/canonical_json.json` holds the accepted
and refused vectors, with the reason for each refusal, and every
implementation replays all of them.
`protocol/fixtures/canonical_json_reference.py`, a Python standard-library
implementation, writes those vectors and the three descriptor fixtures, so
their expected bytes and digests do not come from the Rust code they test;
the Rust tests run it with `--check`.

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
