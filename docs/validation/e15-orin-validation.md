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

## Current E15 Gaps To Track

- The checked-in `test/models/bundles/v01_e13/vision_tensorrt` fixture is
  parser-only and is not functional TensorRT evidence.
- The checked-in `smolvla_python_pytorch` fixture is not a real SmolVLA
  policy. Use it only for bundle/parser compatibility until a real or
  validation-grade VLA sidecar fixture lands.
