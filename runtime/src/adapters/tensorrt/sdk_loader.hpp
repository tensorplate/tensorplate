// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <NvInferRuntime.h>
#include <cuda_runtime_api.h>

#include <memory>
#include <utility>

#include "tensorplate/core/result.hpp"

namespace tensorplate::adapters::tensorrt {

// Private adapter boundary. Function types follow the installed SDK declarations.
struct SdkFunctions {
  decltype(&createInferRuntime_INTERNAL) create_runtime = nullptr;
  decltype(&getInferLibVersion) infer_version = nullptr;
  decltype(&cudaRuntimeGetVersion) runtime_version = nullptr;
  decltype(&cudaDriverGetVersion) driver_version = nullptr;
  decltype(&cudaGetDeviceCount) device_count = nullptr;
  decltype(&cudaGetErrorString) error_string = nullptr;
  decltype(&cudaMalloc) malloc = nullptr;
  decltype(&cudaFree) free = nullptr;
  decltype(&cudaStreamCreate) stream_create = nullptr;
  decltype(&cudaStreamDestroy) stream_destroy = nullptr;
  decltype(&cudaMemcpyAsync) memcpy_async = nullptr;
  decltype(&cudaStreamSynchronize) stream_synchronize = nullptr;
};

// Injection is private to loader tests; production uses the POSIX operations.
struct LibraryOps {
  void* (*open)(const char*, int);
  void* (*symbol)(void*, const char*);
  int (*close)(void*);
};

class SdkLibraries final {
 public:
  [[nodiscard]] static Result<std::shared_ptr<const SdkLibraries>> load();
  [[nodiscard]] static Result<std::shared_ptr<const SdkLibraries>> load(LibraryOps ops);
  [[nodiscard]] const SdkFunctions& functions() const noexcept { return functions_; }

 private:
  using Handle = std::unique_ptr<void, decltype(LibraryOps::close)>;
  SdkLibraries(Handle cuda, Handle infer, SdkFunctions functions)
      : cuda_(std::move(cuda)), infer_(std::move(infer)), functions_(functions) {}

  // Reverse destruction closes TensorRT before CUDA. Consumers retain this owner
  // until every vendor object, stream and buffer using its functions is gone.
  Handle cuda_;
  Handle infer_;
  SdkFunctions functions_;
};

}  // namespace tensorplate::adapters::tensorrt
