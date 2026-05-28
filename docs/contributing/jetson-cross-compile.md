# Jetson cross-compile setup

This document covers how to build TensorPlate's C++ components for the
NVIDIA Jetson Orin family (aarch64) from an x86_64 development host. v0.1.0
deliberately separates this path from the default x86_64 developer loop:
**missing Jetson dependencies must not block CI** for ordinary PRs.

The Jetson hardware-in-loop (T4) tests run only on release branches against
real devices; see [`test/hil/`](../../test/hil/). For the current native
on-device T1/T2/T3 validation pass, see
[`jetson-target-validation.md`](jetson-target-validation.md).

## What we ship and what is system-provided

TensorPlate ships:

- A CMake cross toolchain stub at
  [`cmake/toolchains/aarch64-jetson.cmake`](../../cmake/toolchains/aarch64-jetson.cmake).
- The `tensorrt-adapter` and `libtorch-adapter` feature flags in
  `vcpkg.json`. These do not vendor the SDKs; they only opt the build into
  finding them.

TensorPlate **does not** vendor:

- TensorRT (any version).
- CUDA toolkit, cuBLAS, cuDNN.
- JetPack base libraries or rootfs.
- The aarch64 cross compiler.

These come from NVIDIA JetPack and are licensed by NVIDIA under their own
terms. Contributors must obtain them through
[NVIDIA SDK Manager](https://developer.nvidia.com/nvidia-sdk-manager) or by
mounting a Jetson rootfs.

## Prerequisites

1. JetPack 6.x flashed on the target Jetson Orin device, or a sysroot
   extracted from one.
2. The aarch64 cross compiler. On Ubuntu 22.04:
   ```bash
   sudo apt-get install -y g++-11-aarch64-linux-gnu
   ```
3. The Jetson sysroot copied to a stable host path, for example
   `/opt/jetson/sysroot/`.
4. A working host vcpkg (`$VCPKG_ROOT`) per [`CONTRIBUTING.md`](../../CONTRIBUTING.md).

## Configure command

Set the Jetson toolchain inputs and configure with the vcpkg toolchain
chainloading the project's aarch64 file:

```bash
export TP_JETSON_SYSROOT=/opt/jetson/sysroot
export TP_JETSON_CC=/usr/bin/aarch64-linux-gnu-gcc-11
export TP_JETSON_CXX=/usr/bin/aarch64-linux-gnu-g++-11

cmake -S . -B build-jetson \
  -G Ninja \
  -DCMAKE_BUILD_TYPE=RelWithDebInfo \
  -DCMAKE_TOOLCHAIN_FILE="$VCPKG_ROOT/scripts/buildsystems/vcpkg.cmake" \
  -DVCPKG_CHAINLOAD_TOOLCHAIN_FILE="$PWD/cmake/toolchains/aarch64-jetson.cmake" \
  -DVCPKG_TARGET_TRIPLET=arm64-linux

cmake --build build-jetson --parallel
```

The toolchain file `cmake/toolchains/aarch64-jetson.cmake` aborts with a
clear error if `TP_JETSON_SYSROOT`, `TP_JETSON_CC`, or `TP_JETSON_CXX` are
missing. This is intentional: the absence of a hardcoded host path means
the toolchain file does not bake in any contributor's machine.

## Verifying cross-compiled artifacts on device

T4 hardware-in-loop validation lands in release validation along with the end-to-end
deploy and inference loop. Until then, cross-compiled artifacts can be
smoke-tested manually:

```bash
# On the Jetson device:
file ./build-jetson/serving_worker/tensorplate-serving
./build-jetson/serving_worker/tensorplate-serving --version
```

For a stronger target-device pass that builds natively on the Jetson with
TensorRT enabled and runs T1/T2/T3, use
[`jetson-target-validation.md`](jetson-target-validation.md).

## Why this is not in the default devcontainer

The default `.devcontainer/` image installs the x86_64 toolchain only.
JetPack pulls in tens of GB of NVIDIA-licensed binaries, and including
them in the public dev image would impose those terms on every contributor.
Contributors who are doing Jetson work install JetPack onto their host and
mount the sysroot into the container themselves.

## Troubleshooting

- **`vcpkg` reports it can't find the cross compiler**: confirm
  `aarch64-linux-gnu-g++-11` is on `PATH` for the configure step, or pass
  explicit absolute paths via `TP_JETSON_CC` / `TP_JETSON_CXX`.
- **TensorRT / CUDA headers not found**: the sysroot must include
  `/usr/include/aarch64-linux-gnu/NvInfer.h` and the matching libraries.
  On JetPack this is provided by the `nvidia-tensorrt-dev` package on the
  Jetson side; copy the device rootfs into your host sysroot.
- **Linker complains about absolute host paths**: do not use
  `-DCMAKE_SYSROOT=...` on its own. Always go through the toolchain file
  so `CMAKE_FIND_ROOT_PATH_MODE_*` are set correctly.
