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
#include <cstring>
#include <filesystem>
#include <fstream>
#include <limits>
#include <memory>
#include <mutex>
#include <numeric>
#include <optional>
#include <string>
#include <string_view>
#include <unordered_map>
#include <utility>
#include <vector>

#include "tensorplate/backend/capability.hpp"
#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/buffer/cleanup.hpp"
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

/// RAII wrapper for TensorRT SDK handles. TensorRT 10 removed the
/// legacy `destroy()` member and expects normal deletion; older
/// releases still require `destroy()` because public destructors were
/// not part of the supported API.
template <typename T>
struct TrtDeleter {
  void operator()(T* ptr) const noexcept {
    if (ptr != nullptr) {
#if NV_TENSORRT_MAJOR >= 10
      delete ptr;
#else
      ptr->destroy();
#endif
    }
  }
};

template <typename T>
using TrtUniquePtr = std::unique_ptr<T, TrtDeleter<T>>;

struct CudaStreamHandle {
  cudaStream_t stream = nullptr;
  CudaStreamHandle() = default;
  ~CudaStreamHandle() {
    if (stream != nullptr) {
      cudaStreamDestroy(stream);
    }
  }
  CudaStreamHandle(const CudaStreamHandle&) = delete;
  CudaStreamHandle& operator=(const CudaStreamHandle&) = delete;
  CudaStreamHandle(CudaStreamHandle&& other) noexcept : stream(other.stream) {
    other.stream = nullptr;
  }
  CudaStreamHandle& operator=(CudaStreamHandle&& other) noexcept {
    if (this != &other) {
      if (stream != nullptr) {
        cudaStreamDestroy(stream);
      }
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
    if (device_ptr != nullptr) {
      cudaFree(device_ptr);
    }
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
      if (device_ptr != nullptr) {
        cudaFree(device_ptr);
      }
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

[[nodiscard]] std::string cuda_error_message(cudaError_t status, std::string_view op) {
  return std::string(op) + " failed: " + cudaGetErrorString(status);
}

[[nodiscard]] Result<CudaDeviceBuffer> make_device_buffer(std::size_t bytes) {
  if (bytes == 0) {
    return unexpected(Error::Code::ShapeMismatch, "TensorRT tensor byte size is zero");
  }
  CudaDeviceBuffer out;
  out.size_bytes = bytes;
  const cudaError_t status = cudaMalloc(&out.device_ptr, bytes);
  if (status != cudaSuccess) {
    return unexpected(Error::Code::OOMError, cuda_error_message(status, "cudaMalloc"));
  }
  return out;
}

[[nodiscard]] Result<nvinfer1::Dims> dims_from_tensor_view(const TensorView& view) {
  if (view.rank() > static_cast<std::size_t>(nvinfer1::Dims::MAX_DIMS)) {
    return unexpected(Error::Code::ShapeMismatch,
                      "TensorRT input rank exceeds nvinfer1::Dims::MAX_DIMS");
  }
  nvinfer1::Dims dims{};
  dims.nbDims = static_cast<std::int32_t>(view.rank());
  for (std::int32_t i = 0; i < dims.nbDims; ++i) {
    const auto dim = view.shape().at(static_cast<std::size_t>(i));
    if (dim <= 0 || dim > std::numeric_limits<std::int32_t>::max()) {
      return unexpected(Error::Code::ShapeMismatch,
                        "TensorRT input shape contains an unsupported dimension");
    }
    dims.d[i] = static_cast<std::int32_t>(dim);
  }
  return dims;
}

[[nodiscard]] Result<std::vector<std::int64_t>> tensor_shape_from_dims(const nvinfer1::Dims& dims,
                                                                       std::string_view name) {
  if (dims.nbDims <= 0) {
    return unexpected(Error::Code::ShapeMismatch,
                      "TensorRT tensor `" + std::string(name) + "` has no resolved shape");
  }
  std::vector<std::int64_t> shape;
  shape.reserve(static_cast<std::size_t>(dims.nbDims));
  for (std::int32_t i = 0; i < dims.nbDims; ++i) {
    if (dims.d[i] <= 0) {
      return unexpected(Error::Code::ShapeMismatch, "TensorRT tensor `" + std::string(name) +
                                                        "` has unresolved dynamic dimension");
    }
    shape.push_back(static_cast<std::int64_t>(dims.d[i]));
  }
  return shape;
}

[[nodiscard]] std::optional<DType> dtype_from_trt(nvinfer1::DataType dtype) noexcept {
  switch (dtype) {
    case nvinfer1::DataType::kFLOAT:
      return DType::Float32;
    case nvinfer1::DataType::kHALF:
      return DType::Float16;
    case nvinfer1::DataType::kINT32:
      return DType::Int32;
    case nvinfer1::DataType::kINT8:
      return DType::Int8;
    case nvinfer1::DataType::kBOOL:
      return DType::Bool;
    default:
      return std::nullopt;
  }
}

[[nodiscard]] std::optional<nvinfer1::DataType> dtype_to_trt(DType dtype) noexcept {
  switch (dtype) {
    case DType::Float32:
      return nvinfer1::DataType::kFLOAT;
    case DType::Float16:
      return nvinfer1::DataType::kHALF;
    case DType::Int32:
      return nvinfer1::DataType::kINT32;
    case DType::Int8:
      return nvinfer1::DataType::kINT8;
    case DType::Bool:
      return nvinfer1::DataType::kBOOL;
    case DType::BFloat16:
    case DType::Int64:
    case DType::Int16:
    case DType::UInt8:
      return std::nullopt;
  }
  return std::nullopt;
}

[[nodiscard]] Result<void> check_dtype_matches(nvinfer1::DataType expected, DType observed,
                                               std::string_view tensor_name) {
  const auto observed_trt = dtype_to_trt(observed);
  if (!observed_trt.has_value()) {
    return unexpected(Error::Code::Unsupported, "TensorRT input `" + std::string(tensor_name) +
                                                    "` uses unsupported dtype `" +
                                                    std::string(to_string(observed)) + "`");
  }
  if (*observed_trt != expected) {
    return unexpected(Error::Code::ShapeMismatch, "TensorRT input `" + std::string(tensor_name) +
                                                      "` dtype does not match engine binding");
  }
  return Result<void>{};
}
#endif  // TP_HAS_TENSORRT_SDK

}  // namespace

/// Concrete TensorRT-backed `ExecutionSession`. The session owns every
/// TensorRT and CUDA handle privately; nothing leaks across the public
/// interface.
class TensorRTSession final : public ExecutionSession {
 public:
  explicit TensorRTSession(ExecutionSessionRuntimeHooks hooks)
      : ExecutionSession(hooks)
#if TP_HAS_TENSORRT_SDK
        ,
        manager_(hooks.buffer_manager)
#endif
  {
  }

  [[nodiscard]] std::string_view backend_name() const noexcept override {
    return kBackendName;
  }

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
    if (!bytes_r.has_value()) {
      return unexpected(bytes_r.error());
    }

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
    if (manager_ == nullptr) {
      return unexpected(Error::Code::ConfigInvalid,
                        "TensorRT adapter requires a BufferManager hook");
    }
#if NV_TENSORRT_MAJOR < 10
    (void)request;
    return unexpected(Error::Code::Unsupported,
                      "TensorRT inference requires TensorRT 10 explicit-IO APIs");
#else
    std::unordered_map<std::string_view, const NamedInput*> inputs_by_name;
    inputs_by_name.reserve(request.inputs().size());
    for (const auto& input : request.inputs()) {
      inputs_by_name.emplace(std::string_view(input.name), &input);
    }

    std::vector<CudaDeviceBuffer> device_buffers;
    std::vector<NamedOutput> outputs;
    auto fail = [&](Error error) -> Result<std::vector<NamedOutput>> {
      if (!outputs.empty()) {
        (void)release_partial_outputs(*manager_, outputs);
      }
      return unexpected(std::move(error));
    };

    const auto nb_tensors = state_->engine->getNbIOTensors();
    for (std::int32_t i = 0; i < nb_tensors; ++i) {
      const char* tensor_name_c = state_->engine->getIOTensorName(i);
      if (tensor_name_c == nullptr || *tensor_name_c == '\0') {
        return fail(
            Error::make(Error::Code::LoadFailed, "TensorRT engine published an unnamed IO tensor"));
      }
      const std::string_view tensor_name{tensor_name_c};
      if (state_->engine->getTensorIOMode(tensor_name_c) != nvinfer1::TensorIOMode::kINPUT) {
        continue;
      }

      const auto found = inputs_by_name.find(tensor_name);
      if (found == inputs_by_name.end()) {
        return fail(Error::make(Error::Code::ShapeMismatch, "TensorRT request missing input `" +
                                                                std::string(tensor_name) + "`"));
      }
      const NamedInput& input = *found->second;
      if (auto d = check_dtype_matches(state_->engine->getTensorDataType(tensor_name_c),
                                       input.tensor.dtype(), tensor_name);
          !d.has_value()) {
        return fail(d.error());
      }
      if (input.tensor.layout() != Layout::RowMajor) {
        return fail(Error::make(
            Error::Code::Unsupported,
            "TensorRT input `" + std::string(tensor_name) + "` requires row_major layout"));
      }
      auto dims = dims_from_tensor_view(input.tensor);
      if (!dims.has_value()) {
        return fail(dims.error());
      }
      if (!state_->context->setInputShape(tensor_name_c, dims.value())) {
        return fail(Error::make(Error::Code::ShapeMismatch, "TensorRT rejected input shape for `" +
                                                                std::string(tensor_name) + "`"));
      }
      auto host = manager_->view(input.buffer, input.tensor);
      if (!host.has_value()) {
        return fail(host.error());
      }
      auto device = make_device_buffer(host.value().size());
      if (!device.has_value()) {
        return fail(device.error());
      }
      const cudaError_t copy_status =
          cudaMemcpyAsync(device.value().device_ptr, host.value().data(), host.value().size(),
                          cudaMemcpyHostToDevice, state_->stream.stream);
      if (copy_status != cudaSuccess) {
        return fail(Error::make(Error::Code::InferenceFailed,
                                cuda_error_message(copy_status, "cudaMemcpyAsync H2D")));
      }
      if (!state_->context->setTensorAddress(tensor_name_c, device.value().device_ptr)) {
        return fail(Error::make(
            Error::Code::InferenceFailed,
            "TensorRT rejected input tensor address for `" + std::string(tensor_name) + "`"));
      }
      device_buffers.push_back(std::move(device).value());
    }

    struct PendingOutput {
      std::string name;
      TensorView tensor;
      CudaDeviceBuffer device;
    };
    std::vector<PendingOutput> pending_outputs;

    for (std::int32_t i = 0; i < nb_tensors; ++i) {
      const char* tensor_name_c = state_->engine->getIOTensorName(i);
      const std::string_view tensor_name{tensor_name_c == nullptr ? "" : tensor_name_c};
      if (tensor_name.empty() ||
          state_->engine->getTensorIOMode(tensor_name_c) != nvinfer1::TensorIOMode::kOUTPUT) {
        continue;
      }
      auto dtype = dtype_from_trt(state_->engine->getTensorDataType(tensor_name_c));
      if (!dtype.has_value()) {
        return fail(Error::make(
            Error::Code::Unsupported,
            "TensorRT output `" + std::string(tensor_name) + "` uses unsupported dtype"));
      }
      auto shape =
          tensor_shape_from_dims(state_->context->getTensorShape(tensor_name_c), tensor_name);
      if (!shape.has_value()) {
        return fail(shape.error());
      }
      auto tv = TensorView::create(*dtype, std::move(shape).value());
      if (!tv.has_value()) {
        return fail(tv.error());
      }
      auto device = make_device_buffer(tv.value().byte_size());
      if (!device.has_value()) {
        return fail(device.error());
      }
      if (!state_->context->setTensorAddress(tensor_name_c, device.value().device_ptr)) {
        return fail(Error::make(
            Error::Code::InferenceFailed,
            "TensorRT rejected output tensor address for `" + std::string(tensor_name) + "`"));
      }
      pending_outputs.push_back(
          PendingOutput{std::string(tensor_name), tv.value(), std::move(device).value()});
    }
    if (pending_outputs.empty()) {
      return fail(
          Error::make(Error::Code::InferenceFailed, "TensorRT engine has no output tensors"));
    }

    if (!state_->context->enqueueV3(state_->stream.stream)) {
      return fail(Error::make(Error::Code::InferenceFailed, "TensorRT enqueueV3 returned false"));
    }

    for (auto& pending : pending_outputs) {
      auto host_buffer = manager_->allocate(pending.tensor.byte_size());
      if (!host_buffer.has_value()) {
        return fail(host_buffer.error());
      }
      NamedOutput output{pending.name, host_buffer.value(), pending.tensor, std::nullopt};
      outputs.push_back(output);
      auto dst = manager_->data(output.buffer);
      if (!dst.has_value()) {
        return fail(dst.error());
      }
      const cudaError_t copy_status =
          cudaMemcpyAsync(dst.value().data(), pending.device.device_ptr, pending.tensor.byte_size(),
                          cudaMemcpyDeviceToHost, state_->stream.stream);
      if (copy_status != cudaSuccess) {
        return fail(Error::make(Error::Code::InferenceFailed,
                                cuda_error_message(copy_status, "cudaMemcpyAsync D2H")));
      }
    }
    const cudaError_t sync_status = cudaStreamSynchronize(state_->stream.stream);
    if (sync_status != cudaSuccess) {
      return fail(Error::make(Error::Code::InferenceFailed,
                              cuda_error_message(sync_status, "cudaStreamSynchronize")));
    }
    return outputs;
#endif
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
  BufferManager* manager_ = nullptr;
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
