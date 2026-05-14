# Jetson target validation

This runbook documents the manual native-device validation used for the
E5 adapter/runtime readiness pass. It runs TensorPlate on a Jetson target
with TensorRT enabled, the Python/PyTorch sidecar installed, and the C++
T1/T2/T3 test tiers executed on device.

This is not the future automated T4 hardware-in-loop suite and it is not
a model-hosting benchmark. It is the current target-side proof that the
v0.1.0 runtime substrate builds and runs on Jetson.

## When to run

Run this after host CI is green when a change touches:

- Adapter registration, capability publication, or SDK feature gates.
- TensorRT or CUDA build/link behavior.
- The Python/PyTorch sidecar adapter or Unix-socket IPC path.
- Buffer ownership, `ExecutionSession`, or adapter conformance behavior.
- Any release-readiness branch that needs target-device evidence.

## What this proves

- The C++ runtime, tests, and serving worker compile natively on Jetson
  aarch64 with warnings as errors.
- CMake detects the JetPack-provided TensorRT/CUDA SDK and compiles the
  real TensorRT adapter execution path.
- The TensorRT adapter links against the target SDK and its SDK-present
  unit-test path behaves differently from the host no-SDK shell.
- The Python/PyTorch sidecar package is importable on target.
- The C++ sidecar adapter can launch the configured Python interpreter
  and communicate over the Unix-socket IPC path.
- The shared `ExecutionSession` conformance harness passes against the
  real Python/PyTorch adapter on target.

## What this does not prove

- Real model serving through the final public serving API.
- A TensorRT golden-engine inference pass.
- SmolVLA or other production model loading.
- Long-running supervision, restart, crash-loop, or observability behavior.
- Latency, throughput, memory, or power targets.

Those remain T4/T5 scope and are tracked with the model-hosting and
benchmark work.

## Prerequisites

- A Jetson device with JetPack, CUDA, and TensorRT runtime/development
  packages installed.
- SSH access to the device.
- Access to the private TensorPlate repository, usually through an SSH
  deploy key or a GitHub user key already authorized for the repo.
- Python 3.10 or newer on the device. The sidecar package requires
  Python 3.10+.
- CMake 3.25 or newer. Ubuntu-provided CMake on older JetPack images may
  be too old, so this runbook installs CMake in a virtual environment.

## 1. Install target packages

```bash
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  build-essential \
  ca-certificates \
  git \
  ninja-build \
  nlohmann-json3-dev \
  python3-pip \
  python3-venv
```

JetPack supplies CUDA, TensorRT, and NVDLA libraries. Do not vendor those
libraries into the repo.

## 2. Clone the branch under test

```bash
mkdir -p ~/workspace
cd ~/workspace
git clone <private-repo-url> tensorplate
cd tensorplate
git checkout <branch-or-sha-under-test>
```

If the repo is already present, fetch and check out the exact commit that
CI validated.

## 3. Create the Python environment

Use a Python 3.10+ interpreter. If the device has multiple Python
versions, replace `python3` below with the intended executable, for
example `python3.10`.

```bash
python3 -m venv .venv
. .venv/bin/activate
python -m pip install --upgrade pip
python -m pip install "cmake>=3.25" ninja
python -m pip install -e backends/python_pytorch
hash -r
cmake --version
```

`cmake --version` must report 3.25 or newer.

## 4. Configure and build

```bash
cmake -S . -B build-jetson -G Ninja \
  -DTP_WARNINGS_AS_ERRORS=ON \
  -DTP_ENABLE_SANITIZERS=OFF \
  -DTP_ENABLE_TENSORRT=ON \
  -DTP_ENABLE_PYTHON_PYTORCH_SIDECAR=ON

cmake --build build-jetson --parallel
```

The configure output must include:

```text
-- TensorRT SDK detected; building real TensorRT adapter execution path
```

If it does not, stop and fix the JetPack/TensorRT installation before
treating the run as target validation.

## 5. Run T1/T2/T3 on target

Some JetPack images keep NVDLA runtime libraries in the `tegra` library
directory. Exporting this path makes GoogleTest discovery and CTest runs
load the TensorRT-linked test binaries consistently.

```bash
export TP_TEST_PYTHON="$PWD/.venv/bin/python"
export LD_LIBRARY_PATH="/usr/lib/aarch64-linux-gnu/tegra:/usr/lib/aarch64-linux-gnu:${LD_LIBRARY_PATH:-}"

ctest --test-dir build-jetson --output-on-failure -L T1
ctest --test-dir build-jetson --output-on-failure -L T2
ctest --test-dir build-jetson --output-on-failure -L T3
```

Expected result:

- T1 reports `100% tests passed`. A skipped
  `LibTorchAdapter.FeatureFlagDisabled` is acceptable when LibTorch is not
  enabled.
- T2 reports `100% tests passed` for the Python/PyTorch sidecar
  integration cases.
- T3 reports `100% tests passed`, including
  `RealAdapterConformance.PythonPytorchSatisfiesV01E04Contract`.

## Evidence to record

For PR or release evidence, capture:

```bash
git rev-parse HEAD
cmake --version
uname -a
dpkg-query -W 'nvidia-l4t-core' 'nvidia-tensorrt*' 'libnvinfer*'
```

Also record the configure line showing TensorRT SDK detection and the
T1/T2/T3 CTest summaries.

## Troubleshooting

### CMake 3.25 or newer is required

If system CMake is too old, keep using the virtual-environment CMake:

```bash
. .venv/bin/activate
python -m pip install --upgrade "cmake>=3.25"
hash -r
which cmake
cmake --version
```

### `Could not find CMAKE_ROOT`

This usually means the shell is invoking a mixed or stale CMake install,
or `CMAKE_ROOT` points at an old path. Unset the variable and reinstall
the pip package:

```bash
unset CMAKE_ROOT
python -m pip install --force-reinstall "cmake>=3.25"
hash -r
cmake --version
```

### `libnvdla_compiler.so` is missing

TensorRT-linked tests may fail to load if the JetPack library path is not
visible during GoogleTest discovery. Export the `tegra` directory before
running CTest:

```bash
export LD_LIBRARY_PATH="/usr/lib/aarch64-linux-gnu/tegra:/usr/lib/aarch64-linux-gnu:${LD_LIBRARY_PATH:-}"
ctest --test-dir build-jetson --output-on-failure -L T1
```

If the library still cannot be found, confirm the JetPack TensorRT runtime
packages are installed on the device.

### TensorRT SDK is not detected

Configure must find `NvInfer.h`, `libnvinfer`, and CUDA. Confirm those are
installed by JetPack, then configure from a fresh build directory. A run
that does not print the TensorRT SDK detection message is only exercising
the no-SDK adapter shell.

### `TensorRTAdapter.LoadWithoutSdkReportsUnsupported` fails

With the SDK present, this test should not expect `Unsupported`; it should
exercise the SDK-present branch. If it reports the no-SDK expectation,
make sure the branch includes the public `TP_HAS_TENSORRT_SDK` test-target
compile definition fix, then reconfigure from a fresh build directory.

### Python sidecar tests skip or fail

Confirm the sidecar package is installed into the same interpreter that
CTest uses:

```bash
export TP_TEST_PYTHON="$PWD/.venv/bin/python"
"$TP_TEST_PYTHON" -m pip show tensorplate-pytorch-backend
"$TP_TEST_PYTHON" -c "import tensorplate_pytorch_backend"
```
