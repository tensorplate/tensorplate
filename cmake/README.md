# `cmake/`

CMake helpers for the C++ build.

## Layout

- `toolchains/` — toolchain files (e.g., `x86_64-linux-gnu.cmake`,
  `aarch64-jetson.cmake`) used with `-DCMAKE_TOOLCHAIN_FILE=...` or via
  vcpkg's chainload toolchain mechanism.
- `modules/` — `Find*.cmake` and reusable modules added to
  `CMAKE_MODULE_PATH`.
- `features/` — feature-flag plumbing for `TP_ENABLE_<FEATURE>` options
  defined in [`CONTRIBUTING.md`](../CONTRIBUTING.md).

## Rules

- Vendor SDK detection (TensorRT, CUDA, PyTorch/LibTorch) lives under
  `modules/` and is opt-in. The default x86_64 build must succeed without
  Jetson SDKs available.
- Toolchain files do not bake in absolute host paths.

The contents are bootstrapped in V01-E01-F02. Adapter detection lands with
the relevant adapter in V01-E05.
