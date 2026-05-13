// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F01: Built-in adapter registration.
//
// Adapter-specific registration functions live next to the adapter's
// implementation file (e.g. `runtime/src/adapters/tensorrt/...`) and
// are forward-declared here to avoid leaking adapter headers into the
// public surface. The build feature flags decide which forward
// declarations are visible and which calls are issued.

#include "tensorplate/backend/builtin.hpp"

#include "tensorplate/backend/registry.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate {

#if TP_ENABLE_TENSORRT
Result<void> register_tensorrt_backend(BackendRegistry& registry);
#endif

#if TP_ENABLE_LIBTORCH
Result<void> register_libtorch_backend(BackendRegistry& registry);
#endif

#if TP_ENABLE_PYTHON_PYTORCH_SIDECAR
Result<void> register_python_pytorch_backend(BackendRegistry& registry);
#endif

Result<void> register_builtin_backends([[maybe_unused]] BackendRegistry& registry) {
#if TP_ENABLE_TENSORRT
  if (auto r = register_tensorrt_backend(registry); !r.has_value()) return r;
#endif
#if TP_ENABLE_LIBTORCH
  if (auto r = register_libtorch_backend(registry); !r.has_value()) return r;
#endif
#if TP_ENABLE_PYTHON_PYTORCH_SIDECAR
  if (auto r = register_python_pytorch_backend(registry); !r.has_value()) return r;
#endif
  return Result<void>{};
}

}  // namespace tensorplate
