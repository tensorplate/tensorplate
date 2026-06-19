# SDK End-To-End Release Validation

This document records the release-signoff procedure and evidence format for
the `tensorplate-python` SDK. It covers a clean install from the release
candidate, fixture integration, compatibility with an unchanged v0.1.2
serving worker, the failure-mode envelopes, and a hardware detector signoff
on a Jetson Orin Nano.

The SDK is a client-side library; protocol / transport / health conformance
needs **no GPU**. Real-model detector evidence (a deployed Jetson YOLO
detector exercised through the SDK and the camera sample) is recorded under
[Observed evidence](#observed-evidence-yolov8n-on-jetson-orin-nano-2026-06-18);
the broader runtime procedure stays in the
[Orin validation procedure](./orin-release-validation.md).

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

## 4. Hardware detector signoff (Jetson Orin Nano)

Deploy a real YOLOv8n TensorRT detector and exercise it through
`VisionClient.detect` and the `camera_infer.py` loop. Build the engine on the
target with `trtexec` from an exported `yolov8n.onnx`
(`images[1,3,640,640]` -> `output0[1,84,8400]`, the `yolo_v8_single_output`
contract) and stage the bundle under `/var/lib/tensorplate`: the agent runs
with `ProtectHome=yes` and `PrivateTmp=yes`, so bundles under `/home` or
`/tmp` are not visible to it and deploy fails with a permission error.

- [ ] YOLOv8n golden-output detection on known images within tolerance.
- [ ] Live USB-camera `camera_infer.py` loop produces per-frame detections.

A recorded run is in
[Observed evidence](#observed-evidence-yolov8n-on-jetson-orin-nano-2026-06-18).

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

## Observed evidence: YOLOv8n on Jetson Orin Nano (2026-06-18)

A full hardware-in-the-loop run of the v0.1.3 SDK against a real deployed
detector. **Decision: pass** (the PyPI-publish row is N/A — deferred).

Target:

- Jetson Orin Nano 8GB Super; L4T R36 REV 5.0 (JetPack 6.x), kernel
  `5.15.185-tegra`, `aarch64`; TensorRT 10.3, CUDA present.
- Runtime `tensorplate 0.1.2 (protocol 0.1)`, core packages `0.1.2-1`,
  `tensorplate doctor` failing: 0. SDK installed with the `[vision]` extra.
  The v0.1.3 SDK running against the unchanged 0.1.2 runtime is itself the
  v0.1.2-worker compatibility check — pass.

Detector:

- YOLOv8n (ultralytics 8.4.71) exported to ONNX
  (`images[1,3,640,640]` -> `output0[1,84,8400]`) and built to a TensorRT
  FP16 engine with `trtexec` (engine 9,467,732 bytes; GPU compute ~3.9 ms
  mean, ~255 qps). Deployed as `validation-yolov8n` (`backend tensorrt`,
  health `ready`).

Results:

- `ServingClient.health()`: `state ready`, `backend tensorrt`,
  `active_model validation-yolov8n`.
- `VisionClient.detect` golden output (boxes in source pixels):
  - `bus.jpg` -> 4x `person` (0.90, 0.88, 0.87, 0.43) + 1x `bus` (0.83), the
    canonical YOLOv8n result for this image.
  - `zidane.jpg` -> 2x `person` (0.82, 0.82) + 1x `tie` (0.29).
- `camera_infer.py` loop:
  - 30-frame video source: per-frame detections throughout.
  - Live USB webcam (`/dev/video0`, 640x480): 20-frame loop, 1-2
    detections/frame; a grabbed frame detected `tv` (0.58).

Findings:

- The agent runs with `ProtectHome=yes` and `PrivateTmp=yes`; bundles must be
  staged under `/var/lib/tensorplate` (not `/home` or `/tmp`), or deploy
  fails with `Permission denied (os error 13)`.
- A pip-installed `opencv-python` has no GStreamer support; a USB / V4L2
  camera works via `--source 0`, but a CSI / MIPI camera needs a
  GStreamer-enabled OpenCV (for example, JetPack's system build).
