// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F03: LibTorch native adapter implementation.
//
// Built when `TP_ENABLE_LIBTORCH=1`. When CMake also locates the
// LibTorch C++ distribution (`Torch_DIR` -> `find_package(Torch)`),
// `TP_HAS_LIBTORCH_SDK=1` is defined and the adapter loads a
// TorchScript module from `ModelSpec::artifact_path()` and executes it
// synchronously. Without the SDK the adapter still registers and
// `do_load` surfaces typed `Unsupported` so doctor can enumerate the
// adapter and bundle validation can produce an actionable error before
// model execution.

#include "libtorch_session.hpp"

#include <cstddef>
#include <cstring>
#include <filesystem>
#include <memory>
#include <optional>
#include <string>
#include <string_view>
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

#if TP_HAS_LIBTORCH_SDK
#include <torch/script.h>
#include <torch/torch.h>
#endif

namespace tensorplate::adapters::libtorch {

BackendCapability make_libtorch_capability() {
  // FP32 is the always-supported reference path. FP16 and BFloat16 are
  // accepted; INT8 is left out of the baseline (per-op quant support
  // varies and v0.1.0 ships only the reference path).
  std::vector<PrecisionHint> precision = {PrecisionHint::Fp32, PrecisionHint::Fp16,
                                          PrecisionHint::BFloat16};
  std::vector<std::string> notes = {
      "TorchScript / scripted-module artifacts only",
      "not a fallback for python_pytorch bundles",
  };
  return BackendCapability::create(std::string(kBackendName), std::move(precision),
                                   /*shape_support=*/ShapeSupport::Dynamic,
                                   /*profile_id=*/std::nullopt,
                                   /*supports_async=*/false,
                                   /*supports_generation=*/false,
                                   /*supports_streaming=*/false,
                                   /*supports_kv_cache=*/false,
                                   /*op_coverage_score_pct=*/std::nullopt,
                                   /*memory_estimate_bytes=*/std::nullopt,
                                   /*memory_limit_bytes=*/std::nullopt,
                                   /*target_compatibility_notes=*/std::move(notes))
      .value();
}

namespace {

#if TP_HAS_LIBTORCH_SDK
struct LibTorchState {
  torch::jit::Module module;
};

[[nodiscard]] std::optional<c10::ScalarType> torch_dtype(DType dtype) noexcept {
  switch (dtype) {
    case DType::Float32:
      return c10::kFloat;
    case DType::Float16:
      return c10::kHalf;
    case DType::BFloat16:
      return c10::kBFloat16;
    case DType::Int64:
      return c10::kLong;
    case DType::Int32:
      return c10::kInt;
    case DType::Int16:
      return c10::kShort;
    case DType::Int8:
      return c10::kChar;
    case DType::UInt8:
      return c10::kByte;
    case DType::Bool:
      return c10::kBool;
  }
  return std::nullopt;
}

[[nodiscard]] std::optional<DType> tensorplate_dtype(c10::ScalarType dtype) noexcept {
  switch (dtype) {
    case c10::kFloat:
      return DType::Float32;
    case c10::kHalf:
      return DType::Float16;
    case c10::kBFloat16:
      return DType::BFloat16;
    case c10::kLong:
      return DType::Int64;
    case c10::kInt:
      return DType::Int32;
    case c10::kShort:
      return DType::Int16;
    case c10::kChar:
      return DType::Int8;
    case c10::kByte:
      return DType::UInt8;
    case c10::kBool:
      return DType::Bool;
    default:
      return std::nullopt;
  }
}

[[nodiscard]] std::string output_name(std::size_t index) {
  return index == 0 ? std::string("output") : "output_" + std::to_string(index);
}
#endif

}  // namespace

class LibTorchSession final : public ExecutionSession {
 public:
  explicit LibTorchSession(ExecutionSessionRuntimeHooks hooks)
      : ExecutionSession(hooks), manager_(hooks.buffer_manager) {}

  [[nodiscard]] std::string_view backend_name() const noexcept override { return kBackendName; }

 protected:
  Result<void> do_load(const ModelSpec& spec) override {
#if TP_HAS_LIBTORCH_SDK
    const auto& path = spec.artifact_path();
    if (path.empty()) {
      return unexpected(Error::Code::ConfigInvalid, "LibTorch artifact_path is empty");
    }
    if (!std::filesystem::exists(path)) {
      return unexpected(Error::Code::LoadFailed, "TorchScript artifact not found: " + path);
    }
    try {
      state_ = std::make_unique<LibTorchState>();
      state_->module = torch::jit::load(path);
      // Eager-mode evaluation; v0.1.0 does not enable adapter-side
      // graph fusion or autograd.
      state_->module.eval();
    } catch (const c10::Error& e) {
      state_.reset();
      return unexpected(Error::Code::LoadFailed,
                        std::string("torch::jit::load failed: ") + e.what());
    } catch (const std::exception& e) {
      state_.reset();
      return unexpected(Error::Code::LoadFailed,
                        std::string("unexpected exception loading TorchScript: ") + e.what());
    }
    return Result<void>{};
#else
    (void)spec;
    return unexpected(Error::Code::Unsupported,
                      "LibTorch adapter built without the LibTorch SDK; rebuild with "
                      "TP_HAS_LIBTORCH_SDK=1 (set Torch_DIR)");
#endif
  }

  Result<void> do_prime() override {
#if TP_HAS_LIBTORCH_SDK
    if (!state_) {
      return unexpected(Error::Code::NotReady, "LibTorch module not loaded");
    }
    // v0.1.0 prime is a no-op; module.eval() in do_load is sufficient.
    return Result<void>{};
#else
    return unexpected(Error::Code::Unsupported, "LibTorch adapter built without the LibTorch SDK");
#endif
  }

  Result<std::vector<NamedOutput>> do_infer(const InferRequest& request) override {
#if TP_HAS_LIBTORCH_SDK
    if (!state_) {
      return unexpected(Error::Code::NotReady, "LibTorch session not loaded");
    }
    if (manager_ == nullptr) {
      return unexpected(Error::Code::ConfigInvalid,
                        "LibTorch adapter requires a BufferManager hook");
    }

    std::vector<c10::IValue> inputs;
    inputs.reserve(request.inputs().size());
    for (const auto& input : request.inputs()) {
      if (input.tensor.layout() != Layout::RowMajor) {
        return unexpected(Error::Code::Unsupported,
                          "LibTorch adapter supports row_major tensors only");
      }
      auto dtype = torch_dtype(input.tensor.dtype());
      if (!dtype.has_value()) {
        return unexpected(Error::Code::Unsupported,
                          "LibTorch adapter does not support tensor dtype " +
                              std::string(to_string(input.tensor.dtype())));
      }
      auto bytes = manager_->view(input.buffer, input.tensor);
      if (!bytes.has_value()) {
        return unexpected(bytes.error());
      }
      auto options = torch::TensorOptions().dtype(*dtype).device(torch::kCPU);
      auto tensor = torch::from_blob(const_cast<std::byte*>(bytes.value().data()),
                                     input.tensor.shape(), options)
                        .clone();
      inputs.emplace_back(std::move(tensor));
    }

    c10::IValue raw_output;
    try {
      torch::NoGradGuard no_grad;
      raw_output = state_->module.forward(std::move(inputs));
    } catch (const c10::Error& e) {
      return unexpected(Error::Code::InferenceFailed,
                        std::string("LibTorch forward failed: ") + e.what());
    } catch (const std::exception& e) {
      return unexpected(Error::Code::InferenceFailed,
                        std::string("unexpected exception in LibTorch forward: ") + e.what());
    }

    std::vector<at::Tensor> tensors;
    if (raw_output.isTensor()) {
      tensors.push_back(raw_output.toTensor());
    } else if (raw_output.isTuple()) {
      for (const auto& item : raw_output.toTuple()->elements()) {
        if (!item.isTensor()) {
          return unexpected(Error::Code::Unsupported,
                            "LibTorch tuple outputs must contain tensors only");
        }
        tensors.push_back(item.toTensor());
      }
    } else {
      return unexpected(Error::Code::Unsupported,
                        "LibTorch adapter supports Tensor or Tuple[Tensor, ...] outputs only");
    }
    if (tensors.empty()) {
      return unexpected(Error::Code::InferenceFailed, "LibTorch forward returned no tensors");
    }

    std::vector<NamedOutput> outputs;
    auto fail = [&](Error error) -> Result<std::vector<NamedOutput>> {
      (void)release_partial_outputs(*manager_, outputs);
      return unexpected(std::move(error));
    };
    for (std::size_t i = 0; i < tensors.size(); ++i) {
      at::Tensor cpu = tensors[i].detach().to(torch::kCPU).contiguous();
      auto dtype = tensorplate_dtype(cpu.scalar_type());
      if (!dtype.has_value()) {
        return fail(Error::make(Error::Code::Unsupported,
                                "LibTorch output dtype is not representable in TensorView"));
      }
      std::vector<std::int64_t> shape(cpu.sizes().begin(), cpu.sizes().end());
      auto view = TensorView::create(*dtype, std::move(shape));
      if (!view.has_value()) {
        return fail(view.error());
      }
      const auto bytes = static_cast<std::size_t>(cpu.numel()) * cpu.element_size();
      auto buffer = manager_->allocate(bytes);
      if (!buffer.has_value()) {
        return fail(buffer.error());
      }
      auto dst = manager_->data(buffer.value());
      if (!dst.has_value()) {
        outputs.push_back(
            NamedOutput{output_name(i), buffer.value(), std::move(view).value(), std::nullopt});
        return fail(dst.error());
      }
      std::memcpy(dst.value().data(), cpu.data_ptr(), bytes);
      outputs.push_back(
          NamedOutput{output_name(i), buffer.value(), std::move(view).value(), std::nullopt});
    }
    return outputs;
#else
    (void)request;
    return unexpected(Error::Code::Unsupported, "LibTorch adapter built without the LibTorch SDK");
#endif
  }

  Result<void> do_unload() override {
#if TP_HAS_LIBTORCH_SDK
    state_.reset();
#endif
    return Result<void>{};
  }

 private:
#if TP_HAS_LIBTORCH_SDK
  std::unique_ptr<LibTorchState> state_;
#endif
  [[maybe_unused]] BufferManager* manager_ = nullptr;
};

}  // namespace tensorplate::adapters::libtorch

namespace tensorplate {

Result<void> register_libtorch_backend(BackendRegistry& registry) {
  using adapters::libtorch::kBackendName;
  using adapters::libtorch::LibTorchSession;
  using adapters::libtorch::make_libtorch_capability;
  return registry.register_backend(BackendEntry{
      std::string(kBackendName),
      make_libtorch_capability(),
      [](ExecutionSessionRuntimeHooks hooks) -> Result<std::unique_ptr<ExecutionSession>> {
        return std::unique_ptr<ExecutionSession>(new LibTorchSession(hooks));
      },
  });
}

}  // namespace tensorplate
