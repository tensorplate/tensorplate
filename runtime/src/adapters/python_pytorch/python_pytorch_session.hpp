// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F05: Python/PyTorch sidecar adapter - internal header.
//
// This is the C++ adapter that supervises one Python sidecar process
// per execution session and translates `ExecutionSession` lifecycle
// calls into the schema-defined messages on the Unix-domain socket
// (V01-E05-F04). Adapter and sidecar communicate exclusively over the
// socket; the adapter does not expose Python types in any public
// runtime header.

#pragma once

#include <chrono>
#include <memory>
#include <string>
#include <string_view>

#include "tensorplate/backend/capability.hpp"
#include "tensorplate/backend/registry.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate::adapters::python_pytorch {

inline constexpr std::string_view kBackendName = "python_pytorch";

/// Adapter configuration tunables. Defaults are chosen for the v0.1.0
/// SmolVLA validation flow on Jetson Orin Nano 8GB Super.
struct PythonPytorchConfig {
  std::string python_exe = "python3";
  std::chrono::milliseconds startup_timeout{std::chrono::seconds{15}};
  std::chrono::milliseconds infer_timeout{std::chrono::seconds{30}};
  std::chrono::milliseconds health_timeout{std::chrono::seconds{2}};
};

[[nodiscard]] BackendCapability make_python_pytorch_capability();

}  // namespace tensorplate::adapters::python_pytorch

namespace tensorplate {

[[nodiscard]] Result<void> register_python_pytorch_backend(BackendRegistry& registry);

}  // namespace tensorplate
