// SPDX-License-Identifier: Apache-2.0
//
// V01-E04-F02..F07: Shared mock ExecutionSession used by every T1/T2/T3
// test in V01-E04. The mock implements the adapter contract (the
// protected `do_*` methods) and exposes inspection hooks tests use to
// assert on:
//
//   - the number of `do_load`/`do_prime`/`do_infer`/`do_infer_async`/
//     `do_unload` dispatches reaching the adapter,
//   - the most recent ModelSpec / InferRequest the adapter saw,
//   - the outputs it should publish on success,
//   - and the typed Error to return on the next failure.
//
// The mock deliberately performs no real work; correctness of the NVI
// wrapper is the contract under test. Real adapters in V01-E05 must
// satisfy the same conformance suite through an `ExecutionSession*`
// pointer.

#pragma once

#include <cstddef>
#include <cstdint>
#include <optional>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/core/infer_result.hpp"
#include "tensorplate/core/model_spec.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate::testing {

/// Mock ExecutionSession used across V01-E04 tests. Drop-in adapter
/// that the NVI wrapper dispatches into.
class MockSession final : public ExecutionSession {
 public:
  /// Counts of adapter-level dispatches reaching the mock. Public so
  /// tests can assert that NVI gates (readiness, validation) stop
  /// dispatch where they should.
  struct DispatchCounts {
    std::size_t load = 0;
    std::size_t prime = 0;
    std::size_t infer = 0;
    std::size_t infer_async = 0;
    std::size_t unload = 0;
  };

  explicit MockSession(std::string backend = "mock") : backend_name_(std::move(backend)) {}

  [[nodiscard]] std::string_view backend_name() const noexcept override { return backend_name_; }

  // -- Test programming surface. --------------------------------------------

  /// Force the next `do_load` to fail with the given typed error and
  /// adapter dispatch count. Cleared after a single use.
  void next_load_fails_with(Error err) { next_load_error_ = std::move(err); }
  void next_prime_fails_with(Error err) { next_prime_error_ = std::move(err); }
  void next_infer_fails_with(Error err) { next_infer_error_ = std::move(err); }
  void next_infer_async_fails_with(Error err) { next_infer_async_error_ = std::move(err); }
  void next_unload_fails_with(Error err) { next_unload_error_ = std::move(err); }

  /// Configure the success outputs published by the next `do_infer`. If
  /// empty, `do_infer` returns an empty outputs vector (which the NVI
  /// wrapper treats as an adapter failure on F03+).
  void set_next_infer_outputs(std::vector<NamedOutput> outputs) {
    next_infer_outputs_ = std::move(outputs);
  }

  /// Enable native async support: subsequent `do_infer_async` calls
  /// return an AsyncInferHandle with a fresh session-scoped async_id.
  /// Default (disabled) behavior delegates to the base class, which
  /// returns Error::Code::Unsupported.
  void enable_native_async(bool enable = true) noexcept { native_async_ = enable; }

  [[nodiscard]] const DispatchCounts& dispatch_counts() const noexcept { return dispatch_counts_; }
  [[nodiscard]] const std::optional<ModelSpec>& last_load_spec() const noexcept {
    return last_load_spec_;
  }
  [[nodiscard]] const std::optional<std::string>& last_infer_request_id() const noexcept {
    return last_infer_request_id_;
  }
  [[nodiscard]] const std::optional<std::string>& last_infer_async_request_id() const noexcept {
    return last_infer_async_request_id_;
  }

 protected:
  Result<void> do_load(const ModelSpec& spec) override {
    ++dispatch_counts_.load;
    last_load_spec_ = spec;
    if (next_load_error_.has_value()) {
      auto err = std::move(*next_load_error_);
      next_load_error_.reset();
      return unexpected(std::move(err));
    }
    return Result<void>{};
  }

  Result<void> do_prime() override {
    ++dispatch_counts_.prime;
    if (next_prime_error_.has_value()) {
      auto err = std::move(*next_prime_error_);
      next_prime_error_.reset();
      return unexpected(std::move(err));
    }
    return Result<void>{};
  }

  Result<std::vector<NamedOutput>> do_infer(const InferRequest& request) override {
    ++dispatch_counts_.infer;
    last_infer_request_id_ = request.request_id();
    if (next_infer_error_.has_value()) {
      auto err = std::move(*next_infer_error_);
      next_infer_error_.reset();
      return unexpected(std::move(err));
    }
    return next_infer_outputs_;
  }

  Result<AsyncInferHandle> do_infer_async(const InferRequest& request) override {
    ++dispatch_counts_.infer_async;
    last_infer_async_request_id_ = request.request_id();
    if (next_infer_async_error_.has_value()) {
      auto err = std::move(*next_infer_async_error_);
      next_infer_async_error_.reset();
      return unexpected(std::move(err));
    }
    if (!native_async_) {
      return ExecutionSession::do_infer_async(request);
    }
    AsyncInferHandle h;
    h.request_id = request.request_id();
    h.async_id = next_async_id();
    return h;
  }

  Result<void> do_unload() override {
    ++dispatch_counts_.unload;
    if (next_unload_error_.has_value()) {
      auto err = std::move(*next_unload_error_);
      next_unload_error_.reset();
      return unexpected(std::move(err));
    }
    return Result<void>{};
  }

 private:
  std::string backend_name_;
  DispatchCounts dispatch_counts_{};
  std::optional<Error> next_load_error_;
  std::optional<Error> next_prime_error_;
  std::optional<Error> next_infer_error_;
  std::optional<Error> next_infer_async_error_;
  std::optional<Error> next_unload_error_;
  std::vector<NamedOutput> next_infer_outputs_;
  std::optional<ModelSpec> last_load_spec_;
  std::optional<std::string> last_infer_request_id_;
  std::optional<std::string> last_infer_async_request_id_;
  bool native_async_ = false;
};

}  // namespace tensorplate::testing
