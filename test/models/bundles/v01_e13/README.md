# V01-E13 bundle fixtures

These fixtures exist for V01-E13 parser, verifier, and compatibility
conformance tests, plus the V01-E15 end-to-end validation gate. They are
intentionally **small** so they can ship in the repo and run on host CI
without external download steps. The on-device validation path uses real
artifacts authored outside the repo.

## Valid fixtures

| Path                                 | Class      | Backend          | Notes                                                                 |
| ------------------------------------ | ---------- | ---------------- | --------------------------------------------------------------------- |
| `vision_tensorrt/`                   | `vision`   | `tensorrt`       | Jetson Orin FP16 vision detector; n=1 named input.                    |
| `smolvla_python_pytorch/`            | `vla`      | `python_pytorch` | SmolVLA-style multi-input + named action chunk output + `vla` block.  |
| `language_reserved/`                 | `language` | `libtorch`       | Reserved language block (tokenizer + empty generation_config). Parses cleanly; v0.1.0 never executes generation. |
| `vitis_synthetic/`                   | `vision`   | `vitis_ai`       | `.xmodel` placeholder + Vitis INT8 calibration metadata. Parser-only. |

## Invalid fixtures

| Path                                       | Failure category                |
| ------------------------------------------ | ------------------------------- |
| `invalid_corrupt_artifact/`                | `ArtifactDigestMismatch`        |
| `invalid_unsafe_path/`                     | `UnsafeArtifactPath`            |
| `invalid_missing_artifact/`                | `ArtifactMissing`               |
| `invalid_duplicate_io/`                    | `DuplicateInputName`            |
| `invalid_language_block_class/`            | `MismatchedModelClassBlock`     |

## Regenerating digests

When a fixture artifact body changes, regenerate the digests by running
the helper tool against the bundle root:

```bash
cargo run -p tensorplate-bundle-tool -- test/models/bundles/v01_e13/vision_tensorrt
```

The tool prints the canonical `manifest_digest` and the `sha256:` digest
of every file referenced in `manifest.json`. It does **not** modify the
manifest in place; copy the values into `manifest.json` after reviewing
the diff. The conformance test `protocol/rust/tests/bundle_conformance.rs`
re-verifies all fixture digests deterministically.

## Synthetic Vitis fixture

The Vitis fixture exists only to prove the schema can carry `.xmodel`
artifacts and Vitis-style INT8 metadata without revision. On any v0.1.0
Jetson device, `evaluate_compatibility` returns
`CompatibilityViolation::UnavailableBackend` for this fixture because
`vitis_ai` is never published in `available_backends`. The fixture is
parser/design-review only; v0.1.0 has no Vitis adapter and the runtime
must not try to load it.
