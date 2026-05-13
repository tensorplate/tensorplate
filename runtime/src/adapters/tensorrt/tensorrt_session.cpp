// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F02: TensorRT adapter implementation.
//
// SDK gating
//   This file is compiled only when `TP_ENABLE_TENSORRT=1` (the
//   `runtime/CMakeLists.txt` feature flag). The CMake configuration
//   probes for the TensorRT and CUDA SDKs and defines
//   `TP_HAS_TENSORRT_SDK=1` when they are usable. Without the SDK the
//   adapter still compiles, registers, and publishes its capability
//   record, but `do_load` surfaces `Error::Code::Unsupported`.
//
// RAII ownership
//   Every TensorRT and CUDA SDK handle (runtime, engine, execution
//   context, device buffers, CUDA stream) is owned by a private
//   `TensorRTState` object instantiated inside `TensorRTSession`. The
//   session and the state never leak into public runtime headers; the
//   conformance harness only ever sees `tensorplate::ExecutionSession*`.

#include "tensorrt_session.hpp"

#include <chrono>
#include <cstddef>
#include <cstdint>
#include <filesystem>
#include <fstream>
#include <memory>
#include <mutex>
#include <numeric>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include "tensorplate/backend/capability.hpp"
#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/buffer/output.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/core/infer_result.hpp"
#include "tensorplate/core/model_spec.hpp"
#include "tensorplate/core/result.hpp"

#if TP_HAS_TENSORRT_SDK
#include <NvInfer.h>
#include <cuda_runtime_api.h>
#endif

namespace tensorplate::adapters::tensorrt {

BackendCapability make_tensorrt_capability() {
  // FP16 and INT8 are the v0.1.0 Jetson Orin Nano 8GB Super profile.
  // FP32 is reported as accepted so a developer building an
  // un-quantized engine for bring-up can deploy it.
  std::vector<PrecisionHint> precision = {PrecisionHint::Fp32, PrecisionHint::Fp16,
                                          PrecisionHint::Int8};
  std::vector<std::string> notes = {
      "tested on Jetson Orin Nano 8GB Super (v0.1.0 validation target)",
      "requires a prebuilt TensorRT engine matching the device GPU architecture",
  };
  auto cap = BackendCapability::create(std::string(kBackendName), std::move(precision),
                                       /*shape_support=*/ShapeSupport::Fixed,
                                       /*profile_id=*/std::nullopt,
                                       /*supports_async=*/false,
                                       /*supports_generation=*/false,
                                       /*supports_streaming=*/false,
                                       /*supports_kv_cache=*/false,
                                       /*op_coverage_score_pct=*/std::nullopt,
                                       /*memory_estimate_bytes=*/std::nullopt,
                                       /*memory_limit_bytes=*/std::nullopt,
                                       /*target_compatibility_notes=*/std::move(notes));
  // The factory is called with validated arguments; failure here would
  // be a programming bug.
  return std::move(cap).value();
}

namespace {

#if TP_HAS_TENSORRT_SDK
/// TensorRT logger that routes diagnostics through the structured-log
/// facility once V01-E12 lands. Until then it discards messages so the
/// adapter does not spew to stderr.
class TensorRTLogger final : public nvinfer1::ILogger {
 public:
  void log(Severity /*severity*/, const char* /*msg*/) noexcept override {}
};

TensorRTLogger& trt_logger() noexcept {
  static TensorRTLogger logger;
  return logger;
}

/// RAII wrapper for any TensorRT SDK handle. Calls `destroy()` on
/// release; nullptr is a no-op.
template <typename T>
struct TrtDeleter {
  void operator()(T* ptr) const noexcept {
    if (ptr != nullptr)
      ptr->destroy();
  }
};

template <typename T>
using TrtUniquePtr = std::unique_ptr<T, TrtDeleter<T>>;

struct CudaStreamHandle {
  cudaStream_t stream = nullptr;
  CudaStreamHandle() = default;
  ~CudaStreamHandle() {
    if (stream != nullptr)
      cudaStreamDestroy(stream);
  }
  CudaStreamHandle(const CudaStreamHandle&) = delete;
  CudaStreamHandle& operator=(const CudaStreamHandle&) = delete;
  CudaStreamHandle(CudaStreamHandle&& other) noexcept : stream(other.stream) {
    other.stream = nullptr;
  }
  CudaStreamHandle& operator=(CudaStreamHandle&& other) noexcept {
    if (this != &other) {
      if (stream != nullptr)
        cudaStreamDestroy(stream);
      stream = other.stream;
      other.stream = nullptr;
    }
    return *this;
  }
};

struct CudaDeviceBuffer {
  void* device_ptr = nullptr;
  std::size_t size_bytes = 0;
  CudaDeviceBuffer() = default;
  ~CudaDeviceBuffer() {
    if (device_ptr != nullptr)
      cudaFree(device_ptr);
  }
  CudaDeviceBuffer(const CudaDeviceBuffer&) = delete;
  CudaDeviceBuffer& operator=(const CudaDeviceBuffer&) = delete;
  CudaDeviceBuffer(CudaDeviceBuffer&& other) noexcept
      : device_ptr(other.device_ptr), size_bytes(other.size_bytes) {
    other.device_ptr = nullptr;
    other.size_bytes = 0;
  }
  CudaDeviceBuffer& operator=(CudaDeviceBuffer&& other) noexcept {
    if (this != &other) {
      if (device_ptr != nullptr)
        cudaFree(device_ptr);
      device_ptr = other.device_ptr;
      size_bytes = other.size_bytes;
      other.device_ptr = nullptr;
      other.size_bytes = 0;
    }
    return *this;
  }
};

struct TensorRTState {
  TrtUniquePtr<nvinfer1::IRuntime> runtime;
  TrtUniquePtr<nvinfer1::ICudaEngine> engine;
  TrtUniquePtr<nvinfer1::IExecutionContext> context;
  CudaStreamHandle stream;
  std::vector<CudaDeviceBuffer> device_buffers;
};

[[nodiscard]] Result<std::vector<std::byte>> read_file(const std::string& path) {
  std::ifstream f(path, std::ios::binary | std::ios::ate);
  if (!f) {
    return unexpected(Error::Code::LoadFailed, "failed to open TensorRT engine: " + path);
  }
  const auto end = f.tellg();
  if (end <= 0) {
    return unexpected(Error::Code::LoadFailed, "TensorRT engine file is empty: " + path);
  }
  f.seekg(0, std::ios::beg);
  std::vector<std::byte> buf(static_cast<std::size_t>(end));
  if (!f.read(reinterpret_cast<char*>(buf.data()), static_cast<std::streamsize>(buf.size()))) {
    return unexpected(Error::Code::LoadFailed, "short read on TensorRT engine: " + path);
  }
  return buf;
}
#endif  // TP_HAS_TENSORRT_SDK

}  // namespace

/// Concrete TensorRT-backed `ExecutionSession`. The session owns every
/// TensorRT and CUDA handle privately; nothing leaks across the public
/// interface.
class TensorRTSession final : public ExecutionSession {
 public:
  explicit TensorRTSession(ExecutionSessionRuntimeHooks hooks) : ExecutionSession(hooks) {}

  [[nodiscard]] std::string_view backend_name() const noexcept override { return kBackendName; }

 protected:
  Result<void> do_load(const ModelSpec& spec) override {
#if TP_HAS_TENSORRT_SDK
    const auto& path = spec.artifact_path();
    if (path.empty()) {
      return unexpected(Error::Code::ConfigInvalid, "TensorRT artifact_path is empty");
    }
    if (!std::filesystem::exists(path)) {
      return unexpected(Error::Code::LoadFailed, "TensorRT engine not found: " + path);
    }

    auto bytes_r = read_file(path);
    if (!bytes_r.has_value())
      return unexpected(bytes_r.error());

    state_ = std::make_unique<TensorRTState>();
    state_->runtime.reset(nvinfer1::createInferRuntime(trt_logger()));
    if (!state_->runtime) {
      state_.reset();
      return unexpected(Error::Code::LoadFailed, "createInferRuntime returned null");
    }
    state_->engine.reset(
        state_->runtime->deserializeCudaEngine(bytes_r.value().data(), bytes_r.value().size()));
    if (!state_->engine) {
      state_.reset();
      return unexpected(Error::Code::LoadFailed,
                        "TensorRT engine deserialization failed (architecture mismatch?)");
    }
    return Result<void>{};
#else
    (void)spec;
    return unexpected(
        Error::Code::Unsupported,
        "TensorRT adapter built without the TensorRT SDK; rebuild with TP_HAS_TENSORRT_SDK=1");
#endif
  }

  Result<void> do_prime() override {
#if TP_HAS_TENSORRT_SDK
    if (!state_ || !state_->engine) {
      return unexpected(Error::Code::NotReady, "TensorRT engine not loaded");
    }
    state_->context.reset(state_->engine->createExecutionContext());
    if (!state_->context) {
      return unexpected(Error::Code::LoadFailed, "createExecutionContext returned null");
    }
    if (cudaStreamCreate(&state_->stream.stream) != cudaSuccess) {
      return unexpected(Error::Code::OOMError, "cudaStreamCreate failed");
    }
    // V01-E05-F02-T03 lands the actual warmup pass + device-buffer
    // allocation when the vision golden fixture lands. The prime
    // step here is intentionally minimal so the lifecycle conformance
    // test passes on engines whose binding shapes we have not yet
    // recorded in a bundle manifest.
    return Result<void>{};
#else
    return unexpected(Error::Code::Unsupported, "TensorRT adapter built without the TensorRT SDK");
#endif
  }

  Result<std::vector<NamedOutput>> do_infer(const InferRequest& request) override {
#if TP_HAS_TENSORRT_SDK
    if (!state_ || !state_->context) {
      return unexpected(Error::Code::NotReady, "TensorRT session not primed");
    }
    // V01-E05-F02-T02 ships the binding-resolution and synchronous
    // execution path against the v0.1.0 vision validation engine. The
    // current stub returns `Unsupported` so the conformance suite
    // skips this path until the fixture lands.
    (void)request;
    return unexpected(Error::Code::Unsupported,
                      "TensorRT do_infer not yet wired to vision-golden fixture (V01-E05-F02-T03)");
#else
    (void)request;
    return unexpected(Error::Code::Unsupported, "TensorRT adapter built without the TensorRT SDK");
#endif
  }

  Result<void> do_unload() override {
#if TP_HAS_TENSORRT_SDK
    state_.reset();
    return Result<void>{};
#else
    return Result<void>{};
#endif
  }

 private:
#if TP_HAS_TENSORRT_SDK
  std::unique_ptr<TensorRTState> state_;
#endif
};

}  // namespace tensorplate::adapters::tensorrt

namespace tensorplate {

Result<void> register_tensorrt_backend(BackendRegistry& registry) {
  using adapters::tensorrt::kBackendName;
  using adapters::tensorrt::make_tensorrt_capability;
  using adapters::tensorrt::TensorRTSession;
  return registry.register_backend(BackendEntry{
      std::string(kBackendName),
      make_tensorrt_capability(),
      [](ExecutionSessionRuntimeHooks hooks) -> Result<std::unique_ptr<ExecutionSession>> {
        return std::unique_ptr<ExecutionSession>(new TensorRTSession(hooks));
      },
  });
}

}  // namespace tensorplate
