# bundle fixtures

These fixtures exist for parser, verifier, and compatibility
conformance tests, plus the end-to-end validation gate. They are
intentionally **small** so they can ship in the repo and run on host CI
without external download steps. The on-device validation path uses real
artifacts authored outside the repo.

## Valid fixtures

| Path | Class | Backend | Notes |
| ------------------------------------ | ---------- | ---------------- | --------------------------------------------------------------------- |
| `vision_tensorrt/` | `vision` | `tensorrt` | Jetson Orin FP16 vision detector; n=1 named input. |
| `smolvla_python_pytorch/` | `vla` | `python_pytorch` | SmolVLA-style multi-input + named action chunk output + `vla` block. |
| `mps_python_pytorch_smoke/` | `custom` | `python_pytorch` | Package-validation fixture whose load performs and synchronizes an MPS tensor operation. |
| `x86_fixture_smoke/` | `custom` | `python_pytorch` | Package-validation fixture for the Ubuntu x86_64 cloud rows. Selects the device-neutral `fixture` profile, so it exercises admission and the worker path without executing an accelerator kernel. |
| `x86_cuda_smoke/` | `custom` | `python_pytorch` | Package-validation fixture for the Ubuntu x86_64 cloud rows. Selects the `cuda_fixture` profile, whose load performs a CUDA matmul and checks its result before the fixture serves. |
| `stt_whisper_candidate/` | `custom` | `python_pytorch` | Candidate-only; not a public bundle format. Selects the `faster_whisper` runner profile; its entry lists a converted Whisper model directory as an artifact set. See [candidate speech fixtures](#candidate-speech-fixtures). |
| `tts_kokoro_candidate/` | `custom` | `python_pytorch` | Candidate-only; not a public bundle format. Selects the `kokoro` runner profile; its entry lists a Kokoro config, checkpoint and one voice as an artifact set. See [candidate speech fixtures](#candidate-speech-fixtures). |
| `language_reserved/` | `language` | `libtorch` | Reserved language block (tokenizer + empty generation_config). Parses cleanly; v0.1.0 never executes generation. |
| `vitis_synthetic/` | `vision` | `vitis_ai` | `.xmodel` placeholder + Vitis INT8 calibration metadata. Parser-only. |

## Invalid fixtures

| Path | Failure category |
| ------------------------------------------ | ------------------------------- |
| `invalid_corrupt_artifact/` | `ArtifactDigestMismatch` |
| `invalid_unsafe_path/` | `UnsafeArtifactPath` |
| `invalid_missing_artifact/` | `ArtifactMissing` |
| `invalid_duplicate_io/` | `DuplicateInputName` |
| `invalid_language_block_class/` | `MismatchedModelClassBlock` |

## Candidate speech fixtures

`stt_whisper_candidate/` and `tts_kokoro_candidate/` exercise the sidecar's
speech runner profiles on the tensor-only `/infer` path before a speech
bundle format exists. Two things in them are a candidate-only convention
that goes away when that format lands, so do not copy them into a bundle
you publish:

- the entry's `backend_profile` field, which selects the runner profile
  inside the `python_pytorch` sidecar;
- the tensors those runners exchange: UTF-8 text in as a one-dimensional
  `uint8` tensor named `text_utf8`, and the structured result out as one
  named `result_json`, each at most 1 MiB.

Each entry's `artifact_set` lists the files its runner profile opens, by a
path relative to the entry's directory and a `sha256:` digest. A runner
profile opens them only through
`backends/python_pytorch/src/tensorplate_pytorch_backend/artifact_set.py`,
which refuses an absolute path, a path with `.`, `..` or empty segments
or a backslash, one that resolves outside the entry's directory and a
directory holding a file the entry does not list, and verifies every
digest before it hands out a file. The manifest lists the same files with
the same digests, so the agent's bundle check and the runner's load check
cover the same set; `protocol/rust/tests/bundle_conformance.rs` fails when
the two lists differ.

The files under `model/` are synthetic placeholders, not models, named
after the files the real ones ship: the converted model directory
faster-whisper 1.2.1 reads (`config.json`, `model.bin`,
`preprocessor_config.json`, `tokenizer.json` and a `vocabulary.*` file,
the set `faster_whisper/utils.py` `download_model` fetches) and the
`hexgrad/Kokoro-82M` repository layout (`config.json`, `kokoro-v1_0.pth`
and `voices/<voice>.pt`).

## Regenerating digests

When a fixture artifact body changes, regenerate the digests by running
the helper tool against the bundle root:

```bash
cargo run -p tensorplate-bundle-tool -- test/models/bundles/v0_1/vision_tensorrt
```

The tool prints the canonical `manifest_digest` and the `sha256:` digest
of every file referenced in `manifest.json`. It does **not** modify the
manifest in place; copy the values into `manifest.json` after reviewing
the diff. The conformance test `protocol/rust/tests/bundle_conformance.rs`
re-verifies all fixture digests deterministically.

## Provenance and license

All files in these fixture directories are TensorPlate-authored synthetic
placeholders and are covered by the repository's Apache-2.0 license. The
`.engine`, `.safetensors`, `.xmodel`, tokenizer, and calibration files are
small text or byte fixtures with stable digests; they are not vendor SDK
outputs, trained model weights, NVIDIA sample binaries, Hugging Face model
files, or LeRobot artifacts.

The fixtures are produced by writing minimal placeholder content and then
recording its digest in the adjacent `manifest.json`. When content changes,
regenerate the digest with `tensorplate-bundle-tool` as described above.

## Synthetic Vitis fixture

The Vitis fixture exists only to prove the schema can carry `.xmodel`
artifacts and Vitis-style INT8 metadata without revision. On any v0.1.0
Jetson device, `evaluate_compatibility` returns
`CompatibilityViolation::UnavailableBackend` for this fixture because
`vitis_ai` is never published in `available_backends`. The fixture is
parser/design-review only; v0.1.0 has no Vitis adapter and the runtime
must not try to load it.
