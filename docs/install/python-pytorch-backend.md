# Installing the Python/PyTorch backend

The Python/PyTorch sidecar backend is required for SmolVLA validation
(V01-E15) and for any bundle whose `manifest.json` declares
`backend_hint: python_pytorch`. It is **not** part of the core
TensorPlate install:

- `tensorplate-agent`, `tensorplate-serving`, `tensorplate-observability`,
  and `tensorplate-cli` do not depend on it.
- PyTorch is not declared as a Debian dependency — Jetson aarch64 hosts
  need NVIDIA's PyTorch wheel, CPU hosts get the upstream wheel, and the
  Debian dependency machinery cannot express that choice.
- `tensorplate doctor` reports `python_pytorch_backend = missing` when
  the package or its runtime are absent. Deploys of a `python_pytorch`
  bundle fail before staging with a typed `BackendUnrunnable` error.

## 1. Install the backend package

```bash
sudo apt install tensorplate-backend-python-pytorch
```

This installs:

| Path | Purpose |
| --- | --- |
| `/usr/lib/tensorplate/backends/python_pytorch/` | Sidecar Python package source. |
| `/usr/lib/python3/dist-packages/tensorplate_pytorch_backend.pth` | Makes the sidecar package importable from the descriptor's `/usr/bin/python3`. |
| `/usr/bin/tensorplate-backend-python-pytorch` | Console entrypoint wrapper for direct diagnostics. |
| `/usr/share/tensorplate/backends/python_pytorch/backend.json` | Backend descriptor read by `tensorplate doctor` and the agent. |
| `/usr/share/doc/tensorplate-backend-python-pytorch/` | README mirror. |

The descriptor is intentionally a separate file so doctor probes do not
have to walk arbitrary Python environments to discover the backend. Its
schema is `protocol/schemas/backend_descriptor.json`.

## 2. Install PyTorch into the descriptor's interpreter

The descriptor pins which Python interpreter the sidecar uses. Open it:

```bash
jq . /usr/share/tensorplate/backends/python_pytorch/backend.json
```

Look for the `python.interpreter` field (default `/usr/bin/python3`).
Install PyTorch into that interpreter. The exact command depends on
the platform:

```bash
# Jetson Orin (aarch64, CUDA): use NVIDIA's PyTorch wheel matrix.
# See https://forums.developer.nvidia.com/t/pytorch-for-jetson/72048.
sudo apt install \
  libcudnn9-cuda-12 \
  libcufile-12-6 \
  cuda-cupti-12-6 \
  cuda-libraries-12-6
sudo /usr/bin/python3 -m pip install --upgrade pip wheel
sudo /usr/bin/python3 -m pip install <jetson torch wheel URL>

# x86_64 CPU host (development):
sudo /usr/bin/python3 -m pip install torch>=2.1
```

For the JetPack 6.2 / CUDA 12.6 E15 validation target, the tested wheel
source was the Jetson AI Lab JP6 CUDA 12.6 index:

```bash
sudo /usr/bin/python3 -m pip install --no-cache-dir \
  --index-url https://pypi.jetson-ai-lab.io/jp6/cu126 \
  torch==2.8.0
```

If `import torch` fails with a missing CUDA shared library
(`libcudnn.so.9`, `libcufile.so.0`, `libcupti.so.12`, `libcusparse.so.12`,
etc.), install the apt packages above and rerun `tensorplate doctor`.

The descriptor's `pytorch.minimum_version` field is what `doctor` and
the agent compare against. Override the descriptor if the wheel you
chose pins a different `torch.__version__`:

```bash
sudo $EDITOR /usr/share/tensorplate/backends/python_pytorch/backend.json
```

The descriptor file is not a dpkg conffile (it ships with the optional
backend package). If you rewrite it, keep a copy alongside your
deploy notes; the next package upgrade will overwrite it.

## 3. Verify with `tensorplate doctor`

```bash
tensorplate doctor
```

The relevant findings are stable strings (the V01-E15 harness asserts
on them):

| Finding ID | Status meanings |
| --- | --- |
| `python_pytorch_backend` | `ok` (descriptor + PyTorch present), `missing` (descriptor missing), `fail` (descriptor malformed), `warning` (Python/PyTorch version below minimum). |
| `python_pytorch_runtime` | `ok` / `missing` / `warning` for the PyTorch import and its declared minimum version. |

The default agent config already lists `python_pytorch` as an available
backend. When the descriptor is absent the startup probe records a
typed missing-backend report; once the backend package and PyTorch are
installed a new agent start probes the same backend as runnable.

When the doctor reports green for both, restart the agent so it refreshes
its startup probe cache, then run a `python_pytorch` deploy:

```bash
sudo systemctl restart tensorplate-agent
tensorplate deploy /var/lib/tensorplate/bundles/staging/smolvla.tpmodel
```

If the descriptor is missing or the interpreter cannot import `torch`,
the deploy fails with a typed `BackendUnrunnable` error **before** any
files are staged. It will not silently fall through to first inference.

## What the probe does and does not do

- **Does**: `stat` the descriptor file; run `python3 -c 'import sys; ...'`
  and `python3 -c 'import torch; print(torch.__version__)'` against
  the interpreter pinned in the descriptor; compare versions.
- **Does not**: install anything, run model code, mutate the descriptor,
  scan the filesystem, or reach the network. The probe is read-only
  and bounded (it returns within seconds even when Python is missing).

## Tuning environment variables

The adapter and the SmolVLA backend both read environment variables for
host-specific tuning. Set them in the agent's environment (e.g.
`Environment=` in `tensorplate-agent.service` or the doctor's `env-file`)
so they are applied at process start.

### Adapter (consumed by the C++ runtime and Python runner)

| Variable | Default | Purpose |
| --- | --- | --- |
| `TP_PYTHON_PYTORCH_EXECUTABLE` | `/usr/bin/python3` (from descriptor) | Interpreter the sidecar is launched with. Override when running from a virtualenv (e.g. the E15 validation venv). Falls back to `TP_TEST_PYTHON_EXE` then `TP_TEST_PYTHON` for the C++ test fixtures. |
| `TP_PYTHON_PYTORCH_DEFAULT_BACKEND` | `fixture` | Selects the in-process backend factory. Set to `smolvla` to enable the LeRobot SmolVLA path. |
| `TP_PYTHON_PYTORCH_STARTUP_TIMEOUT_MS` | `15000` | Deadline for sidecar `start`/`load`/`prime`/`unload` exchanges. Increase on cold-cache or HuggingFace-download-heavy startups (90000 has been tested for Orin SmolVLA). |
| `TP_PYTHON_PYTORCH_INFER_TIMEOUT_MS` | `30000` | Per-request inference deadline (clamped by the caller's deadline). |
| `TP_PYTHON_PYTORCH_HEALTH_TIMEOUT_MS` | `2000` | Health-probe deadline. Sidecar runners that handle health off the inference thread can keep this tight; otherwise raise it to avoid spurious flaps. |

### SmolVLA backend (consumed when `default_backend = smolvla`)

| Variable | Default | Purpose |
| --- | --- | --- |
| `TP_SMOLVLA_MODEL_ID` | `lerobot/smolvla_base` | HuggingFace model id passed to `PreTrainedConfig.from_pretrained`. |
| `TP_SMOLVLA_CACHE_DIR` | `/var/lib/tensorplate/hf-cache` | HF cache directory shared by the policy, tokenizer, and config. |
| `TP_SMOLVLA_DEVICE` | `cuda` | Torch device string. Set to `cpu` for non-CUDA hosts. |
| `TP_SMOLVLA_NUM_STEPS` | _(model default)_ | Optional positive integer that overrides `PreTrainedConfig.num_steps` (inference rollout length). |
| `TP_SMOLVLA_TASK` | `pick up the cube\n` | Default language task when the inference frame omits explicit token inputs. |

Values from a per-bundle JSON config (`artifact_path`) take precedence
over the environment, which takes precedence over the built-in default.

## Removing the backend

```bash
sudo apt remove tensorplate-backend-python-pytorch
```

Removal does **not** touch the PyTorch install in the Python
environment. Operators who want to free that space should follow up
with `pip uninstall torch` in the descriptor's interpreter.
