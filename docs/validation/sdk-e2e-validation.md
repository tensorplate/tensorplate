# SDK End-To-End Release Validation

This document records the release-signoff procedure and evidence format for
the `tensorplate-python` SDK. It covers a clean install from the release
candidate, fixture integration, compatibility with an unchanged v0.1.2
serving worker, the failure-mode envelopes, and the deferred
hardware-detector signoff.

The SDK is a client-side library; protocol / transport / health
conformance needs **no GPU**. Real-model detector evidence (a deployed
Jetson YOLO detector) is hardware-only and is recorded with the
[Orin validation procedure](./orin-release-validation.md), not here.

## Decision

Record one of: **pass** (all required rows pass), **conditional pass**
(non-blocking gaps noted, owner sign-off), or **block** (a required row
fails). Capture the SDK version under test, the release tag / candidate,
the `tensorplate-serving` build used for the real-worker lane (if run), and
the host OS / Python version.

Do not archive secrets, raw image payloads, full tensor payloads, or SSH
credentials.

## 1. Clean install from the release candidate

In a fresh virtual environment, install the SDK from the release wheel and
confirm the import surface — without a source checkout.

```bash
python3 -m venv /tmp/tp-sdk-signoff && . /tmp/tp-sdk-signoff/bin/activate
# Download + cosign-verify the wheel + SHA256SUMS as in docs/sdk/python.md, then:
pip install "./tensorplate_python-${TP_VERSION}-py3-none-any.whl[vision]"
python -c "import tensorplate; print(tensorplate.__version__)"
python -c "from tensorplate import ServingClient, VisionClient, Detection, TensorPlateError"
```

- [ ] Wheel installs into a clean environment with no source checkout.
- [ ] `tensorplate.__version__` matches the release version.
- [ ] Core import and the public symbols resolve.
- [ ] PyPI install (`pip install tensorplate-python`): **deferred for
      v0.1.3** — PyPI publication is not enabled. Record N/A; this row
      becomes required once the project is published.

## 2. Fixture integration tests

The SDK test suite runs `ServingClient.infer`, `VisionClient.detect`, the
failure / transport / schema-version cases, and the v0.1.2-compatibility
check against an in-process fixture serving worker. It needs no GPU and no
external process.

```bash
cd sdk/python
pip install -e ".[dev,vision]"
pytest -q
```

- [ ] `ServingClient.infer` round-trips against the fixture worker
      (`tests/test_serving.py`).
- [ ] `VisionClient.detect` decodes detections against the fixture worker
      (`tests/test_vision.py`, `tests/test_examples.py`).
- [ ] Failure envelope maps to `ServingError` with the typed `code`.
- [ ] Transport failure on a dead endpoint raises `TransportError`.
- [ ] An unsupported `schema_version` raises
      `UnsupportedSchemaVersionError`.

## 3. Unchanged v0.1.2 worker compatibility

The `/infer` and `/health` envelope is `schema_version` `0.1`, pinned
identically since v0.1.2, so the v0.1.3 SDK works against an unchanged
v0.1.2 worker. This is asserted by
`tests/test_serving.py::test_round_trips_against_unchanged_v0_1_2_worker`
(canned `0.1` envelope) and can be confirmed against a real worker.

Optional real-worker lane — run the actual `tensorplate-serving` mock
worker (CPU, no GPU) and round-trip the SDK:

```bash
export TENSORPLATE_SERVING_WORKER_BIN=/path/to/tensorplate-serving
cd sdk/python && pytest -q tests/test_e2e_worker.py
```

- [ ] Canned-envelope compatibility test passes (always-on).
- [ ] Real `--mock` worker round-trip passes (`health()` ready + `infer()`),
      or recorded N/A when the worker binary is unavailable in this
      environment.

## 4. Hardware detector signoff (deferred to Orin procedure)

A real deployed Jetson YOLO detector with a golden-output tolerance is
hardware-only and is recorded with the
[Orin validation procedure](./orin-release-validation.md). The mock worker
is not a detector, so `VisionClient.detect` end-to-end against a real model
is not exercised here.

- [ ] Jetson YOLO detector golden-output validation: recorded in the Orin
      validation evidence, or explicitly deferred for this candidate.

## 5. Release artifact, docs, and changelog checks

- [ ] The SDK wheel and sdist are attached to the GitHub Release.
- [ ] The wheel and sdist appear in `SHA256SUMS` and the artifact manifest,
      and the cosign signature over `SHA256SUMS` verifies.
- [ ] `docs/sdk/python.md` install + quickstart commands match the released
      asset names and this candidate's flow.
- [ ] `docs/release/notes/v0.1.3.md` lists the SDK assets and links the SDK
      install / quickstart docs.
- [ ] `CHANGELOG.md` is promoted from `[Unreleased]` to the dated `[0.1.3]`
      section before the final tag.
