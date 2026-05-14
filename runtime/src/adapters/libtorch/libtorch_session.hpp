// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F03: LibTorch native adapter session - internal header.
//
// LibTorch is the C++ runtime of PyTorch. This adapter executes
// TorchScript modules that have been exported with `torch.jit.script`
// or `torch.jit.trace`. It is intentionally not a fallback for the
// `python_pytorch` sidecar backend: bundles whose author declares
// `backend_hint: python_pytorch` execute in a Python sidecar, never in
// LibTorch (V01-E05-F03 acceptance criteria).
//
// As with the TensorRT adapter, no LibTorch type appears in this
// internal header; the SDK includes are confined to
// `libtorch_session.cpp` behind the `TP_HAS_LIBTORCH_SDK` define.

#pragma once

#include <memory>
#include <string>
#include <string_view>
#include <vector>

#include "tensorplate/backend/capability.hpp"
#include "tensorplate/backend/registry.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate {

namespace adapters::libtorch {

inline constexpr std::string_view kBackendName = "libtorch";

[[nodiscard]] BackendCapability make_libtorch_capability();

class LibTorchSession;

}  // namespace adapters::libtorch

[[nodiscard]] Result<void> register_libtorch_backend(BackendRegistry& registry);

}  // namespace tensorplate
