// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F02: TensorRT adapter session - internal header.
//
// This header is **private** to the TensorRT adapter. It must not be
// included by anything outside `runtime/src/adapters/tensorrt/` or
// `test/unit/`. No TensorRT or CUDA header appears here; the actual
// SDK types are isolated inside `tensorrt_session.cpp` behind the
// `TP_HAS_TENSORRT_SDK` preprocessor switch.
//
// The adapter publishes its registration through
// `register_tensorrt_backend(BackendRegistry&)`, which the built-in
// adapter registration entry point dispatches when
// `TP_ENABLE_TENSORRT=1` is set at build time. When the build flag is
// on but the SDK was not found at configure time the adapter still
// registers (so `tensorplate doctor` can enumerate it) and `load`
// surfaces `Error::Code::Unsupported` with an actionable message.

#pragma once

#include <memory>
#include <string>
#include <string_view>
#include <vector>

#include "tensorplate/backend/capability.hpp"
#include "tensorplate/backend/registry.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/core/infer_result.hpp"
#include "tensorplate/core/model_spec.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate {

class BufferManager;

namespace adapters::tensorrt {

/// Stable backend key.
inline constexpr std::string_view kBackendName = "tensorrt";

/// Build the capability record this adapter publishes. Reported to the
/// registry at registration time and to status reporting at runtime.
[[nodiscard]] BackendCapability make_tensorrt_capability();

/// Concrete `ExecutionSession` implementation for TensorRT. Forward
/// declared here; the implementation owns TensorRT and CUDA resources
/// privately via RAII and never leaks them across the public interface.
class TensorRTSession;

}  // namespace adapters::tensorrt

/// Register the TensorRT adapter onto `registry`. Called by
/// `register_builtin_backends` when `TP_ENABLE_TENSORRT=1`.
[[nodiscard]] Result<void> register_tensorrt_backend(BackendRegistry& registry);

}  // namespace tensorplate
