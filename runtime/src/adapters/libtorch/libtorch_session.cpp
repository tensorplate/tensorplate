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

#include <filesystem>
#include <memory>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include "tensorplate/backend/capability.hpp"
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
#endif

}  // namespace

class LibTorchSession final : public ExecutionSession {
 public:
  explicit LibTorchSession(ExecutionSessionRuntimeHooks hooks) : ExecutionSession(hooks) {}

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
    // V01-E05-F03-T02 wires named-input -> at::Tensor conversion and
    // output materialization. The current stub returns Unsupported so
    // the conformance suite skips this branch until V01-E05-F03-T03
    // exported-graph fixtures land.
    (void)request;
    return unexpected(
        Error::Code::Unsupported,
        "LibTorch do_infer not yet wired to exported-graph fixture (V01-E05-F03-T03)");
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
