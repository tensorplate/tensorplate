// SPDX-License-Identifier: Apache-2.0
// This target is built only on Linux x86_64 with SDK headers available.
#if TP_HAS_TENSORRT_LOADER
#include "sdk_loader.hpp"

#include <dlfcn.h>

#include <string>
#include <type_traits>

namespace tensorplate::adapters::tensorrt {

std::string cuda_library_name() {
  return "libcudart.so." + std::to_string(CUDART_VERSION / 1000);
}

std::string tensorrt_library_name() {
  return "libnvinfer.so." + std::to_string(NV_TENSORRT_MAJOR);
}

Result<std::shared_ptr<const SdkLibraries>> SdkLibraries::load() {
  return load({::dlopen, ::dlsym, ::dlclose, ::dlerror});
}

Result<std::shared_ptr<const SdkLibraries>> SdkLibraries::load(LibraryOps ops) {
  const auto cuda_name = cuda_library_name();
  const auto infer_name = tensorrt_library_name();
  Handle cuda{ops.open(cuda_name.c_str(), RTLD_NOW | RTLD_LOCAL), ops.close};
  if (!cuda) {
    const char* detail = ops.error();
    return unexpected(
        Error::Code::Unsupported,
        "cannot load " + cuda_name + ": " + (detail != nullptr ? detail : "unknown loader error"));
  }
  Handle infer{ops.open(infer_name.c_str(), RTLD_NOW | RTLD_LOCAL), ops.close};
  if (!infer) {
    const char* detail = ops.error();
    return unexpected(
        Error::Code::Unsupported,
        "cannot load " + infer_name + ": " + (detail != nullptr ? detail : "unknown loader error"));
  }

  SdkFunctions functions;
  std::string missing;
  const auto bind = [&](auto& function, void* handle, const char* name) {
    function =
        reinterpret_cast<std::remove_reference_t<decltype(function)>>(ops.symbol(handle, name));
    if (function == nullptr) {
      missing = name;
    }
    return function != nullptr;
  };
  if (!bind(functions.create_runtime, infer.get(), "createInferRuntime_INTERNAL") ||
      !bind(functions.infer_version, infer.get(), "getInferLibVersion") ||
      !bind(functions.runtime_version, cuda.get(), "cudaRuntimeGetVersion") ||
      !bind(functions.driver_version, cuda.get(), "cudaDriverGetVersion") ||
      !bind(functions.device_count, cuda.get(), "cudaGetDeviceCount") ||
      !bind(functions.error_string, cuda.get(), "cudaGetErrorString") ||
      !bind(functions.malloc, cuda.get(), "cudaMalloc") ||
      !bind(functions.free, cuda.get(), "cudaFree") ||
      !bind(functions.stream_create, cuda.get(), "cudaStreamCreate") ||
      !bind(functions.stream_destroy, cuda.get(), "cudaStreamDestroy") ||
      !bind(functions.memcpy_async, cuda.get(), "cudaMemcpyAsync") ||
      !bind(functions.stream_synchronize, cuda.get(), "cudaStreamSynchronize")) {
    return unexpected(Error::Code::Unsupported, "missing SDK symbol: " + missing);
  }
  const auto infer_version = functions.infer_version();
  if (infer_version != NV_TENSORRT_VERSION) {
    return unexpected(Error::Code::Unsupported,
                      "TensorRT runtime/header version mismatch: " + std::to_string(infer_version));
  }
  int runtime_version = 0;
  if (functions.runtime_version(&runtime_version) != cudaSuccess) {
    return unexpected(Error::Code::LoadFailed, "cudaRuntimeGetVersion failed");
  }
  if (runtime_version != CUDART_VERSION) {
    return unexpected(Error::Code::Unsupported,
                      "CUDA runtime/header version mismatch: " + std::to_string(runtime_version));
  }
  return std::shared_ptr<const SdkLibraries>(
      new SdkLibraries(std::move(cuda), std::move(infer), functions));
}

}  // namespace tensorplate::adapters::tensorrt
#endif
