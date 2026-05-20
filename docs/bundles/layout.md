# Bundle Layout

**Status:** v0.1.0 (V01-E13-F01)
**Bundle format version:** `0.1`
**Schema:** [`protocol/schemas/bundle_manifest.json`](../../protocol/schemas/bundle_manifest.json)

A TensorPlate model bundle is the deployable artifact the agent consumes from
`tensorplate deploy <bundle>`. A bundle holds the model artifact, the
declared backend hint, the integrity metadata, and the compatibility envelope
the agent uses to refuse impossible deployments before they ever touch the
serving worker.

This document defines the physical layout of a bundle and the path-safety
rules the parser enforces. The manifest schema itself is documented in
[manifest.md](manifest.md).

---

## Bundle root

A bundle is identified by a directory (the *bundle root*). Tooling that ships
or stores bundles may pack the root into an archive (the `.tpmodel` packaged
form), but the parser always works against the same logical layout — the
archive form is just a deterministic packing of the directory form.

```text
<bundle_root>/
├── manifest.json                # Required. The canonical manifest envelope.
├── <artifact files>             # Required. At least one artifact with role=model.
├── assets/                      # Optional. Backend-owned auxiliary files
│   └── ...                      # (e.g., tokenizer files, calibration data).
├── provenance/                  # Optional. Signature / SBOM / build metadata.
│   ├── signature.json           # Reserved location for v0.1.0 signature stub.
│   └── sbom.json                # Reserved location.
└── ...
```

### Required files

| Path                  | Purpose                                                   |
| --------------------- | --------------------------------------------------------- |
| `manifest.json`       | Canonical manifest. UTF-8 JSON. Discovered at this exact path; alternate manifest paths are rejected. |
| `<model artifact>`    | One artifact entry in the manifest with `role: model`. Path is declared in the manifest. |

### Optional files

| Path                       | Purpose                                                              |
| -------------------------- | -------------------------------------------------------------------- |
| Additional artifact files  | Tokenizer, calibration, precompiled assets, or auxiliary files referenced by additional manifest artifact entries. |
| `assets/`                  | Reserved directory for backend-side helper files declared as artifacts. |
| `provenance/signature.json`| Reserved for the optional `signature` block in the manifest.         |
| `provenance/sbom.json`     | Reserved for the optional `provenance.sbom` block in the manifest.   |

The parser does not inspect files that are not referenced by the manifest. Such
files are tolerated for forward compatibility but ignored for digest, capacity,
and compatibility decisions.

---

## Packaged form

The packaged distribution form (`.tpmodel`) is a deterministic archive of the
bundle root with the same layout. v0.1.0 only commits to the directory form
as the parser input; the packaged form is reserved by the layout but is
emitted by tooling in a later milestone. Test fixtures use the directory
form so the parser can run on a development host without an archive
dependency.

The parser's archive-format error class is reserved so that a future
packaged-form reader can return the same typed error shape as the directory
reader.

---

## Path safety rules

The parser rejects manifests whose artifact paths could escape the bundle
root. A path is **safe** when *all* of the following hold:

1. The string is non-empty.
2. The string does not start with `/` (no absolute paths).
3. The string does not contain a `\` byte (Windows-style separators are
   rejected so the layout stays platform-neutral).
4. No segment (after splitting on `/`) equals `..`, `.`, or the empty
   string.

The parser normalizes path segments to a stable form (drops trailing `/`,
collapses adjacent `/`), but never resolves a path against the host
filesystem before canonicalization. The bundle root itself is canonicalized
through the OS so symlinks above the root cannot leak in.

Files referenced by the manifest must reside under the canonicalized bundle
root after path resolution. Symlinks inside the bundle that escape the root
are treated like absolute paths and rejected.

The parser does **not** extract files outside the configured staging area.
Bundles staged into `<staging_dir>/<bundle_id>/` inherit the staging area's
permission boundary; the parser's path-safety rules are the second line of
defense, not the only one.

---

## Bundle descriptor

The parser returns a `BundleDescriptor` value object containing:

- the absolute, canonicalized bundle root
- the parsed manifest (after schema and semantic validation)
- the canonical manifest digest (sha256 of the manifest JSON with
  `manifest_digest` stripped — see [integrity.md](integrity.md))
- the list of artifact descriptors with their absolute paths, declared
  digests, and declared sizes

The descriptor is the *only* contract callers downstream of the parser see.
Backend SDK types never appear in this value object.

---

## Examples

Both example fixtures live under [`test/models/bundles/v01_e13/`](../../test/models/bundles/v01_e13/).

### TensorRT vision bundle (single-input vision, n=1)

```text
yolov8n-vision/
├── manifest.json
└── model.engine
```

### SmolVLA Python/PyTorch bundle (named multi-input + named action output)

```text
smolvla-pi-450m/
├── manifest.json
├── policy.py
├── policy_weights.safetensors
└── assets/
    └── tokenizer.json
```

### Synthetic Vitis-shaped bundle (parser-only fixture)

```text
synthetic-kria-vitis/
├── manifest.json
├── model.xmodel
└── calibration.json
```

This fixture exists to prove the bundle envelope can carry an `.xmodel`
artifact without schema revision. v0.1.0 has no Vitis AI adapter; the agent
deploy verifier still rejects a bundle whose declared backend is unavailable
on the device — Vitis-shaped bundles parse cleanly but fail backend
availability validation on Jetson devices, exactly as a TensorRT bundle
would fail on Kria silicon.

---

## Non-goals (v0.1.0)

- Implementing the packaged `.tpmodel` archive reader. Reserved as an
  extension point.
- Bundle directory layout under `/var` (owned by V01-E14 packaging).
- CLI authoring tools beyond the deterministic digest helper used by
  fixtures (see [`tools/bundle/`](../../tools/bundle/)).
- Hosted provenance verification. The `provenance/` and `signature` fields
  are reserved locations only.
