// SPDX-License-Identifier: Apache-2.0
//
// V01-E07-F01-T02: Built-in mock serving session.
//
// Used by the default ServingConfig (use_mock_session = true) so the
// serving worker can be exercised end-to-end on host CI without any
// real backend (TensorRT / LibTorch / Python sidecar). The mock
// returns a single output named "actions" whose payload is a small,
// deterministic tensor derived from the request id.

#pragma once

#include <cstddef>
#include <cstdint>
#include <cstring>
#include <mutex>
#include <string>
#include <string_view>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/output.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate::serving {

/// Behavioral knobs for the mock used by tests. Defaults are tuned
/// for the V01-E07-F08 happy-path tests.
struct MockSessionConfig {
  /// Output name published in `InferResult`.
  std::string output_name = "actions";
  /// Output dtype.
  DType output_dtype = DType::Float32;
  /// Output shape.
  std::vector<std::int64_t> output_shape = {1, 4};
  /// Semantic tag attached to the output.
  std::string output_semantic_tag = "action_chunk";
  /// If non-empty, the next `do_infer` call returns this typed error
  /// and clears the field. Used by failure-injection tests.
  std::optional<Error> next_failure;
  /// If true, every `do_infer` call returns this typed error without
  /// being cleared. Used by sustained-failure tests.
  std::optional<Error> sticky_failure;
};

class MockServingSession final : public ExecutionSession {
 public:
  MockServingSession(BufferManager& buffer_manager, MockSessionConfig config,
                     ExecutionSessionRuntimeHooks hooks = {})
      : ExecutionSession(hooks), buffer_manager_(buffer_manager), config_(std::move(config)) {}

  [[nodiscard]] std::string_view backend_name() const noexcept override { return "mock"; }

  /// Test surface: install a next-call failure.
  void set_next_failure(Error err) {
    std::lock_guard<std::mutex> g(mutex_);
    config_.next_failure = std::move(err);
  }
  void set_sticky_failure(std::optional<Error> err) {
    std::lock_guard<std::mutex> g(mutex_);
    config_.sticky_failure = std::move(err);
  }

 protected:
  Result<void> do_load(const ModelSpec& /*spec*/) override { return Result<void>{}; }
  Result<void> do_prime() override { return Result<void>{}; }

  Result<std::vector<NamedOutput>> do_infer(const InferRequest& request) override {
    {
      std::lock_guard<std::mutex> g(mutex_);
      if (config_.sticky_failure.has_value()) {
        return unexpected(*config_.sticky_failure);
      }
      if (config_.next_failure.has_value()) {
        auto e = std::move(*config_.next_failure);
        config_.next_failure.reset();
        return unexpected(std::move(e));
      }
    }
    // Allocate one output through the buffer plane.
    auto view = TensorView::create(config_.output_dtype, config_.output_shape, Layout::RowMajor);
    if (!view) {
      return unexpected(view.error());
    }
    OutputDescriptor desc{config_.output_name, view.value(), config_.output_semantic_tag,
                          view.value().byte_offset() + view.value().byte_size()};
    auto out_r = build_named_output(buffer_manager_, desc);
    if (!out_r) {
      return unexpected(out_r.error());
    }
    auto out = std::move(out_r).value();
    // Fill with a deterministic ramp tied to request id length so
    // tests can distinguish requests.
    auto span_r = buffer_manager_.data(out.buffer);
    if (span_r) {
      auto span = span_r.value();
      auto numel = static_cast<std::size_t>(view.value().num_elements());
      const std::size_t elem_bytes = view.value().byte_size() / std::max<std::size_t>(numel, 1);
      if (config_.output_dtype == DType::Float32) {
        for (std::size_t i = 0; i < numel; ++i) {
          float v = static_cast<float>(i + request.request_id().size());
          if (elem_bytes == sizeof(float)) {
            std::memcpy(span.data() + i * elem_bytes, &v, sizeof(float));
          }
        }
      } else {
        // Generic deterministic ramp.
        for (std::size_t i = 0; i < span.size(); ++i) {
          span[i] = static_cast<std::byte>(i & 0xFF);
        }
      }
    }
    std::vector<NamedOutput> outputs;
    outputs.push_back(std::move(out));
    return outputs;
  }

  Result<void> do_unload() override { return Result<void>{}; }

 private:
  BufferManager& buffer_manager_;
  std::mutex mutex_;
  MockSessionConfig config_;
};

}  // namespace tensorplate::serving
