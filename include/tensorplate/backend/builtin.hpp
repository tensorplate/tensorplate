// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F01: Built-in backend registration entry point.
//
// `register_builtin_backends` registers every adapter whose build flag
// is enabled at compile time onto the supplied registry. Higher-level
// callers (the serving worker, conformance tests, doctor checks) call
// this on a registry of their choice instead of relying on a static-
// init "side door" registration that would make it impossible for
// tests to start from a known empty state.
//
// Which backends are registered is determined entirely by the
// `TP_ENABLE_TENSORRT`, `TP_ENABLE_LIBTORCH`, and
// `TP_ENABLE_PYTHON_PYTORCH_SIDECAR` build flags. The function returns
// the first registration error it encounters; partial registration is
// preserved so the caller can introspect the registry to see how far
// initialization progressed.

#pragma once

#include "tensorplate/backend/registry.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate {

/// Register every adapter compiled into this build of `tp_runtime` onto
/// the supplied registry. Safe to call multiple times on distinct
/// registries; calling twice on the same registry returns
/// `Error::Code::Internal` from the underlying `register_backend` call.
[[nodiscard]] Result<void> register_builtin_backends(BackendRegistry& registry);

}  // namespace tensorplate
