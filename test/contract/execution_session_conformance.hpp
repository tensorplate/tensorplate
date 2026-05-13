// SPDX-License-Identifier: Apache-2.0
//
// V01-E04-F07-T01: ExecutionSession adapter conformance suite.
//
// Real backend adapters (TensorRT in V01-E05-F01, LibTorch in
// V01-E05-F02, Python/PyTorch sidecar in V01-E05-F03, future Vitis AI)
// reuse this header to prove they satisfy the V01-E04 public contract
// through an `ExecutionSession*` pointer. The mock-conformance T1 test
// in `test/unit/execution_session_conformance_test.cpp` runs the same
// suite through the shared `MockSession` so the suite is self-testing
// and downstream adapters can be added without rewriting it.
//
// Usage from an adapter test:
//
//   auto factory = [](){ return std::make_unique<MyAdapter>(...); };
//   tensorplate::testing::run_execution_session_conformance(
//       "my_adapter_backend_name",
//       factory,
//       /*model_artifact_path=*/"...");
//
// The factory must return a fresh, unloaded `ExecutionSession`
// implementation on each call.

#pragma once

#include <gtest/gtest.h>

#include <cstddef>
#include <cstdint>
#include <functional>
#include <memory>
#include <string>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/buffer/output.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/core/infer_result.hpp"
#include "tensorplate/core/model_spec.hpp"

namespace tensorplate::testing {

/// Factory producing a fresh, unloaded ExecutionSession on every call.
/// Adapters supply a closure that constructs their session with the
/// adapter-specific configuration baked in.
using SessionFactory = std::function<std::unique_ptr<ExecutionSession>()>;

/// Conformance-test configuration. The adapter author fills in the
/// fields a real backend needs (model artifact path, expected backend
/// name, sample input fixture shape); the suite derives everything
/// else from the public interface.
struct ConformanceConfig {
  /// Expected return value of `backend_name()` on a freshly constructed
  /// session. Adapters use this to assert their identity hasn't drifted.
  std::string expected_backend_name;

  /// Model artifact path passed to `ModelSpec::create`. The mock uses
  /// "/dev/null"; real adapters point this at the bundle artifact.
  std::string model_artifact_path = "/dev/null";

  /// Backend hint passed to `ModelSpec::create`. Should match the
  /// backend's registered factory key.
  std::string backend_hint = "mock";

  /// Sample input fixture used for the happy-path infer test. Default
  /// is a 1x4 float32 input; adapters may override for real model
  /// shape requirements.
  DType sample_input_dtype = DType::Float32;
  std::vector<std::int64_t> sample_input_shape = {1, 4};
  std::string sample_input_name = "in0";
};

/// Internal helpers shared by every conformance test. Exposed so
/// adapter authors can build incremental conformance scaffolding
/// without copy-pasting the suite.
namespace conformance {

inline ModelSpec make_spec(const ConformanceConfig& cfg) {
  auto s = ModelSpec::create("conformance-model", ModelClass::Vision, cfg.model_artifact_path,
                             cfg.backend_hint);
  EXPECT_TRUE(s.has_value()) << (s.has_value() ? "" : s.error().message);
  return std::move(s).value();
}

inline TensorView make_input_view(const ConformanceConfig& cfg) {
  auto v = TensorView::create(cfg.sample_input_dtype, cfg.sample_input_shape);
  EXPECT_TRUE(v.has_value());
  return std::move(v).value();
}

inline std::size_t sample_input_bytes(const ConformanceConfig& cfg) {
  return make_input_view(cfg).byte_size();
}

inline InferRequest valid_request(BufferManager& manager, const ConformanceConfig& cfg,
                                  const std::string& request_id = "conformance-req") {
  const std::size_t bytes = sample_input_bytes(cfg);
  auto buf_r = manager.allocate(bytes);
  EXPECT_TRUE(buf_r.has_value());
  std::vector<NamedInput> inputs;
  inputs.push_back(NamedInput{cfg.sample_input_name, buf_r.value(), make_input_view(cfg)});
  auto req = InferRequest::create(request_id, "/infer", std::move(inputs));
  EXPECT_TRUE(req.has_value());
  return std::move(req).value();
}

inline std::unique_ptr<BufferManager> make_manager() {
  BufferManagerConfig cfg;
  cfg.pool_name = "conformance";
  cfg.capacity_bytes = 1 << 20;
  cfg.max_buffer_bytes = 1 << 18;
  auto r = BufferManager::create(std::move(cfg));
  EXPECT_TRUE(r.has_value());
  return std::move(r).value();
}

}  // namespace conformance

/// Run the full V01-E04 ExecutionSession conformance suite against the
/// adapter produced by `factory`. The suite drives the adapter only
/// through the public `ExecutionSession*` interface, so a passing run
/// proves the adapter respects the NVI contract.
inline void run_execution_session_conformance(const ConformanceConfig& cfg,
                                              const SessionFactory& factory) {
  using namespace conformance;

  // -- backend_name is non-empty and matches the configured expectation.
  {
    auto session = factory();
    ASSERT_NE(session, nullptr);
    EXPECT_EQ(std::string(session->backend_name()), cfg.expected_backend_name);
  }

  // -- Initial state is Unloaded.
  {
    auto session = factory();
    EXPECT_FALSE(session->is_ready());
  }

  // -- Lifecycle happy path: load -> prime -> infer -> unload.
  {
    auto session = factory();
    auto manager = make_manager();

    ASSERT_TRUE(session->load(make_spec(cfg)).has_value());
    EXPECT_FALSE(session->is_ready());
    ASSERT_TRUE(session->prime().has_value());
    EXPECT_TRUE(session->is_ready());

    auto req = valid_request(*manager, cfg);
    auto r = session->infer(req);
    ASSERT_TRUE(r.has_value()) << (r.has_value() ? "" : r.error().message);
    ASSERT_TRUE(r.value().is_success()) << r.value().error().message;
    ASSERT_TRUE(r.value().timing().execution_latency.has_value());
    EXPECT_GE(r.value().timing().execution_latency->count(), 0);
    EXPECT_FALSE(r.value().outputs().empty());

    ASSERT_TRUE(session->unload().has_value());
    EXPECT_FALSE(session->is_ready());
  }

  // -- infer before prime returns NotReady (no adapter dispatch).
  {
    auto session = factory();
    auto manager = make_manager();
    ASSERT_TRUE(session->load(make_spec(cfg)).has_value());

    auto req = valid_request(*manager, cfg);
    auto r = session->infer(req);
    ASSERT_FALSE(r.has_value());
    EXPECT_EQ(r.error().code, Error::Code::NotReady);
  }

  // -- prime before load returns NotReady.
  {
    auto session = factory();
    auto r = session->prime();
    ASSERT_FALSE(r.has_value());
    EXPECT_EQ(r.error().code, Error::Code::NotReady);
  }

  // -- Bad model path is rejected by the adapter at load time.
  //    Adapters that accept any path (mock) skip this branch.
  {
    auto session = factory();
    auto bad = ModelSpec::create("conformance-bad", ModelClass::Vision,
                                 "/tensorplate/conformance/does_not_exist", cfg.backend_hint)
                   .value();
    auto r = session->load(bad);
    // Either succeeds (mock) or returns a typed error (real adapter).
    if (!r.has_value()) {
      EXPECT_TRUE(r.error().code == Error::Code::LoadFailed ||
                  r.error().code == Error::Code::ConfigInvalid)
          << "bad-path load must surface LoadFailed or ConfigInvalid; got "
          << static_cast<int>(r.error().code);
      // Unload recovers.
      EXPECT_TRUE(session->unload().has_value());
      EXPECT_FALSE(session->is_ready());
    }
  }

  // -- Shape mismatch is rejected before adapter dispatch (ShapeMismatch).
  {
    auto session = factory();
    auto manager = make_manager();
    ASSERT_TRUE(session->load(make_spec(cfg)).has_value());
    ASSERT_TRUE(session->prime().has_value());

    // Tensor view overflows its buffer.
    auto tv = make_input_view(cfg);
    auto small_buf = manager->allocate(tv.byte_size() / 2 == 0 ? 1 : tv.byte_size() / 2);
    if (small_buf.has_value()) {
      std::vector<NamedInput> inputs;
      inputs.push_back(NamedInput{cfg.sample_input_name, small_buf.value(), tv});
      auto bad_req = InferRequest::create("conformance-bad-shape", "/infer", std::move(inputs));
      ASSERT_TRUE(bad_req.has_value());

      auto r = session->infer(bad_req.value());
      ASSERT_FALSE(r.has_value());
      EXPECT_EQ(r.error().code, Error::Code::ShapeMismatch);

      // Release the under-sized buffer so the manager goes back to zero.
      (void)manager->release(small_buf.value());
    }
  }

  // -- infer_async exposes the method shape and returns either a typed
  //    Unsupported error or a valid AsyncInferHandle. Whichever it is,
  //    the adapter must be consistent with its capability declaration
  //    (which the conformance suite does not yet introspect — V01-E05).
  {
    auto session = factory();
    auto manager = make_manager();
    ASSERT_TRUE(session->load(make_spec(cfg)).has_value());
    ASSERT_TRUE(session->prime().has_value());

    auto req = valid_request(*manager, cfg, "conformance-async");
    auto r = session->infer_async(req);
    if (!r.has_value()) {
      // Adapters without native async must surface typed Unsupported,
      // not an arbitrary error code.
      EXPECT_EQ(r.error().code, Error::Code::Unsupported);
    } else {
      EXPECT_EQ(r.value().request_id, "conformance-async");
      EXPECT_GT(r.value().async_id, 0u);
    }
  }

  // -- unload then infer returns NotReady.
  {
    auto session = factory();
    auto manager = make_manager();
    ASSERT_TRUE(session->load(make_spec(cfg)).has_value());
    ASSERT_TRUE(session->prime().has_value());
    ASSERT_TRUE(session->unload().has_value());

    auto req = valid_request(*manager, cfg);
    auto r = session->infer(req);
    ASSERT_FALSE(r.has_value());
    EXPECT_EQ(r.error().code, Error::Code::NotReady);
  }

  // -- BufferRef lifetime: request buffers are not implicitly released
  //    by the session. The host or scheduler is responsible for the
  //    request-buffer lifetime (V01-E03 cleanup contracts), so a
  //    successful infer leaves the input buffer in the manager's
  //    active accounting until the caller releases it.
  {
    auto session = factory();
    auto manager = make_manager();
    ASSERT_TRUE(session->load(make_spec(cfg)).has_value());
    ASSERT_TRUE(session->prime().has_value());

    const auto baseline = manager->accounting().active_count;
    auto req = valid_request(*manager, cfg);
    EXPECT_EQ(manager->accounting().active_count, baseline + 1);
    auto r = session->infer(req);
    ASSERT_TRUE(r.has_value()) << (r.has_value() ? "" : r.error().message);
    ASSERT_TRUE(r.value().is_success()) << r.value().error().message;
    // Input buffer is still owned by the request value; it is not
    // freed by `infer`. Release it the way the scheduler would.
    EXPECT_GE(manager->accounting().active_count, baseline + 1);
    for (const auto& in : req.inputs()) {
      (void)manager->release_if_owned(in.buffer);
    }
    EXPECT_EQ(manager->accounting().active_count, baseline);
  }
}

}  // namespace tensorplate::testing
