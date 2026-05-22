# V01-E15 Orin Validation

This document records the E15 hardware-validation procedure and evidence
format for TensorPlate v0.1.0 on Jetson Orin Nano 8GB Super.

## Target Matrix

| Target | Required | Notes |
| --- | --- | --- |
| Jetson Orin Nano 8GB Super | yes | v0.1.0 hardware floor. |
| Jetson Orin NX 16GB | no | Optional production-tier rerun. |
| Kria K26/K24 | no | Design review only; Vitis AI execution is post-v0.1.0. |

Record these fields for every run:

- Device model and hostname.
- Ubuntu, kernel, L4T / JetPack release.
- CUDA, TensorRT, cuDNN, PyTorch, Python, and package versions.
- Exact TensorPlate git SHA and Debian package versions.
- `tensorplate doctor --output json` before and after service start.
- `tensorplate status --output json`, service states, local endpoint check,
  relevant journal excerpts, and metrics snapshots.

Do not archive secrets, raw image payloads, full tensor payloads, SSH
credentials, or unbounded journals.

## F01 Clean Install

1. Build packages on the target with package-installed paths:
   `./packaging/scripts/build-deb.sh`.
2. Purge any prior TensorPlate packages, install the generated core
   packages, and verify the units are enabled but inactive before the
   operator starts them.
3. Run `tensorplate doctor --output json`.
4. Start `tensorplate-agent` and `tensorplate-observability`.
5. Confirm only local endpoints are open:
   `/run/tensorplate/agent.sock` and loopback serving ports after deploy.

## F02 TensorRT Vision Path

Use a real TensorRT engine generated on the target:

```bash
tools/validation/create_trt_identity_bundle.sh /tmp/tp-trt-identity
tensorplate deploy /tmp/tp-trt-identity --deployment-id e15-trt-identity
tensorplate infer \
  --input /tmp/tp-trt-identity/sample_infer.json \
  --output-file /tmp/tp-trt-identity-response.json \
  --output json
tools/validation/verify_trt_identity_response.py /tmp/tp-trt-identity-response.json
```

This proves CLI -> agent -> process-backed worker -> scheduler ->
TensorRT adapter -> response serialization with a target-built engine.
It is intentionally a deterministic synthetic vision fixture; a larger
detector can be substituted later without changing the validation path.

Production-size vision validation uses NVIDIA's installed TensorRT
ResNet50 sample ONNX:

```bash
tools/validation/create_trt_resnet50_bundle.sh \
  /var/lib/tensorplate/validation/tp-trt-resnet50
tensorplate deploy \
  /var/lib/tensorplate/validation/tp-trt-resnet50 \
  --deployment-id e15-trt-resnet50 \
  --output json
tensorplate infer \
  --input /var/lib/tensorplate/validation/tp-trt-resnet50/sample_infer.json \
  --output-file /var/lib/tensorplate/validation/tp-trt-resnet50/response.json \
  --timeout-ms 30000 \
  --output json
tools/validation/verify_trt_resnet50_response.py \
  /var/lib/tensorplate/validation/tp-trt-resnet50/response.json
```

Observed E15 evidence on the connected Jetson target:

- TensorRT built a 49.3 MiB FP16 ResNet50 engine from
  `/usr/src/tensorrt/data/resnet50/ResNet50.onnx`.
- Active deployment `e15-trt-resnet50`, backend `tensorrt`, health
  `ready`.
- Six requests completed successfully; zero failed.
- Response verifier passed with output `gpu_0/softmax_1` shape
  `[1, 1000]` and a softmax sum of `0.999937`.

## F03 SmolVLA Python/PyTorch Path

Use the managed Python/PyTorch sidecar for real SmolVLA validation.
TensorRT conversion is not required for this evidence path.

Target setup used for E15:

```bash
python3 -m venv --system-site-packages /var/lib/tensorplate/e15-smolvla-venv
/var/lib/tensorplate/e15-smolvla-venv/bin/python -m pip install \
  -c /var/lib/tensorplate/e15-smolvla-constraints.txt \
  "lerobot[smolvla]==0.4.4"
/var/lib/tensorplate/e15-smolvla-venv/bin/python -m pip install \
  "numpy==1.26.4" "opencv-python-headless==4.10.0.84" \
  "pandas==2.2.3" "Pillow==10.4.0" "scipy==1.11.4"
PYTHONNOUSERSITE=1 /var/lib/tensorplate/e15-smolvla-venv/bin/python -m pip install \
  mypy-extensions termcolor platformdirs typing_extensions pyyaml filelock
```

Service environment:

```bash
TP_PYTHON_PYTORCH_EXECUTABLE=/var/lib/tensorplate/e15-smolvla-venv/bin/python
TP_PYTHON_PYTORCH_DEFAULT_BACKEND=smolvla
TP_PYTHON_PYTORCH_STARTUP_TIMEOUT_MS=90000
PYTHONNOUSERSITE=1
TOKENIZERS_PARALLELISM=false
HF_HOME=/var/lib/tensorplate/hf-cache
```

Validation flow:

```bash
/var/lib/tensorplate/e15-smolvla-venv/bin/python \
  tools/validation/create_smolvla_bundle.py \
  /var/lib/tensorplate/validation/tp-smolvla-real \
  --cache-dir /var/lib/tensorplate/hf-cache \
  --num-steps 2
tensorplate deploy \
  /var/lib/tensorplate/validation/tp-smolvla-real \
  --deployment-id e15-smolvla-real \
  --output json
tensorplate infer \
  --input /var/lib/tensorplate/validation/tp-smolvla-real/sample_infer.json \
  --output-file /var/lib/tensorplate/validation/tp-smolvla-real/response.json \
  --timeout-ms 60000 \
  --output json
/var/lib/tensorplate/e15-smolvla-venv/bin/python \
  tools/validation/verify_smolvla_response.py \
  /var/lib/tensorplate/validation/tp-smolvla-real/response.json
```

Observed E15 evidence on the connected Jetson target:

- Real model: `lerobot/smolvla_base`, loaded by LeRobot through the
  managed `python_pytorch` sidecar.
- Direct model load: 450,046,176 parameters, CUDA device, about
  0.93 GiB allocated after load.
- Active deployment `e15-smolvla-real`, backend `python_pytorch`, model
  class `vla`, health `ready`.
- Request inputs: three `float32` camera tensors `[1, 3, 256, 256]`,
  `observation.state` `[1, 6]`, token IDs `[1, 48]`, and attention mask
  `[1, 48]`.
- Output: `action.chunk` `float32` shape `[1, 50, 6]`.
- Six requests completed successfully; zero failed.
- First TensorPlate inference reported `994,668,886 ns` execution
  latency; the five-request steady-state loop reported about
  `441-454 ms` wall time per request.
- Metrics after the loop: `requests_total=6`, `requests_succeeded=6`,
  `requests_failed=0`, scheduler admitted/completed success `6/6`,
  completed failure `0`, queue depth `0`, in-flight `0`, buffer active
  count `0`.
- `tensorplate doctor --output json` reported `failing: 0` with active
  deployment `e15-smolvla-real`.

## Current E15 Gaps To Track

- The checked-in `test/models/bundles/v01_e13/vision_tensorrt` fixture is
  parser-only and is not functional TensorRT evidence.
- The checked-in `smolvla_python_pytorch` fixture is not a real SmolVLA
  policy. Use it only for bundle/parser compatibility; the real E15
  SmolVLA evidence is the `e15-smolvla-real` sidecar validation above.
