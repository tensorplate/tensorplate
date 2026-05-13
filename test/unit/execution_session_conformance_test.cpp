// SPDX-License-Identifier: Apache-2.0
//
// V01-E04-F07-T01: Run the shared ExecutionSession conformance suite
// against the mock implementation. Real backend adapters (TensorRT,
// LibTorch, Python/PyTorch sidecar, future Vitis AI) reuse the same
// suite in `test/contract/` so the V01-E04 contract is one source of
// truth across every adapter family.

#include <gtest/gtest.h>

#include <cstddef>
#include <memory>
#include <utility>
#include <vector>

#include "execution_session_conformance.hpp"
#include "mock_execution_session.hpp"
#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/execution_session.hpp"

namespace {

using tensorplate::BufferOwnership;
using tensorplate::BufferRef;
using tensorplate::DType;
using tensorplate::ExecutionSession;
using tensorplate::NamedOutput;
using tensorplate::TensorView;
using tensorplate::testing::ConformanceConfig;
using tensorplate::testing::MockSession;
using tensorplate::testing::run_execution_session_conformance;
using tensorplate::testing::SessionFactory;

/// Build a mock session pre-configured to publish a single-output
/// success response so the conformance suite exercises the success
/// branch. The factory returns a fresh, unloaded session per call so
/// each conformance scenario runs against an independent lifecycle.
SessionFactory make_mock_factory() {
  return []() -> std::unique_ptr<ExecutionSession> {
    auto session = std::make_unique<MockSession>("mock");
    // Conformance success branch wants a non-empty outputs vector with
    // a tensor window that fits its buffer. A 1x4 float32 fits in
    // 16 bytes regardless of the configured input shape.
    auto tv = TensorView::create(DType::Float32, {1, 4}).value();
    auto buf = BufferRef::create(/*id=*/0xC0FFEE, /*size_bytes=*/16,
                                 BufferOwnership::Owned)
                   .value();
    session->set_next_infer_outputs({NamedOutput{"out0", buf, tv, std::nullopt}});
    return session;
  };
}

TEST(ExecutionSessionConformance, MockSatisfiesV01E04Contract) {
  ConformanceConfig cfg;
  cfg.expected_backend_name = "mock";
  cfg.backend_hint = "mock";
  // 1x4 float32 input fits in a 16-byte buffer; the mock does not
  // require any particular model artifact shape.
  cfg.sample_input_dtype = DType::Float32;
  cfg.sample_input_shape = {1, 4};

  run_execution_session_conformance(cfg, make_mock_factory());
}

}  // namespace
