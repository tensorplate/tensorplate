// SPDX-License-Identifier: Apache-2.0
//
// V01-E03-F03-T01 / T02 unit coverage for the buffer cleanup helpers.

#include "tensorplate/buffer/cleanup.hpp"

#include <cstddef>
#include <memory>
#include <string>
#include <utility>
#include <vector>

#include <gtest/gtest.h>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/core/infer_result.hpp"

namespace {

using tensorplate::BufferManager;
using tensorplate::BufferManagerConfig;
using tensorplate::BufferRef;
using tensorplate::CleanupReport;
using tensorplate::DType;
using tensorplate::Error;
using tensorplate::InferRequest;
using tensorplate::NamedInput;
using tensorplate::NamedOutput;
using tensorplate::RequestBufferGuard;
using tensorplate::TensorView;

std::unique_ptr<BufferManager> make_manager() {
  BufferManagerConfig cfg;
  cfg.pool_name = "cleanup_test";
  cfg.capacity_bytes = 64 * 1024;
  cfg.max_buffer_bytes = 16 * 1024;
  auto r = BufferManager::create(std::move(cfg));
  EXPECT_TRUE(r.has_value());
  return std::move(r).value();
}

// Allocate a small uint8 buffer + matching view; convenient fixture builder.
NamedInput make_input(BufferManager& mgr, std::string name, std::size_t size_bytes) {
  auto h = mgr.allocate(size_bytes);
  EXPECT_TRUE(h.has_value());
  auto tv = TensorView::create(DType::UInt8, {static_cast<std::int64_t>(size_bytes)});
  EXPECT_TRUE(tv.has_value());
  return NamedInput{std::move(name), h.value(), tv.value()};
}

NamedOutput make_output(BufferManager& mgr, std::string name, std::size_t size_bytes) {
  auto h = mgr.allocate(size_bytes);
  EXPECT_TRUE(h.has_value());
  auto tv = TensorView::create(DType::UInt8, {static_cast<std::int64_t>(size_bytes)});
  EXPECT_TRUE(tv.has_value());
  return NamedOutput{std::move(name), h.value(), tv.value(), std::nullopt};
}

TEST(BufferCleanup, ReleaseRequestBuffersFreesEveryInputOnce) {
  auto mgr = make_manager();
  std::vector<NamedInput> inputs;
  inputs.push_back(make_input(*mgr, "image_front", 256));
  inputs.push_back(make_input(*mgr, "image_wrist", 256));
  inputs.push_back(make_input(*mgr, "state", 64));

  auto req = InferRequest::create("req-1", "/policy", std::move(inputs));
  ASSERT_TRUE(req.has_value());
  EXPECT_EQ(mgr->accounting().active_count, 3u);

  auto report = release_request_buffers(*mgr, req.value());
  EXPECT_EQ(report.buffers_released, 3u);
  EXPECT_TRUE(report.clean());
  EXPECT_EQ(mgr->accounting().active_count, 0u);
  EXPECT_EQ(mgr->accounting().in_use_bytes, 0u);
}

TEST(BufferCleanup, DuplicateBufferIdsDoNotDoubleFree) {
  // Hand-craft a malformed-fixture-style request that mentions the
  // same BufferRef under two names. The helper must release the
  // underlying storage exactly once.
  auto mgr = make_manager();
  auto h = mgr->allocate(128);
  ASSERT_TRUE(h.has_value());
  auto tv = TensorView::create(DType::UInt8, {128});
  ASSERT_TRUE(tv.has_value());

  std::vector<NamedInput> inputs;
  inputs.push_back(NamedInput{"alias_a", h.value(), tv.value()});
  inputs.push_back(NamedInput{"alias_b", h.value(), tv.value()});
  auto req = InferRequest::create("req-dup", "/policy", std::move(inputs));
  ASSERT_TRUE(req.has_value());

  auto report = release_request_buffers(*mgr, req.value());
  EXPECT_EQ(report.buffers_released, 1u);
  EXPECT_TRUE(report.clean());
  EXPECT_EQ(mgr->accounting().release_failures, 0u);
}

TEST(BufferCleanup, ReleasePartialOutputsFreesEveryNamedOutput) {
  auto mgr = make_manager();
  std::vector<NamedOutput> outputs;
  outputs.push_back(make_output(*mgr, "action_chunk", 512));
  outputs.push_back(make_output(*mgr, "logits", 128));
  EXPECT_EQ(mgr->accounting().active_count, 2u);

  auto report = release_partial_outputs(*mgr, outputs);
  EXPECT_EQ(report.buffers_released, 2u);
  EXPECT_TRUE(report.clean());
  EXPECT_EQ(mgr->accounting().active_count, 0u);
}

TEST(BufferCleanup, ReleasePartialOutputsDoesNotTouchInputs) {
  auto mgr = make_manager();
  auto in = make_input(*mgr, "image", 256);
  auto out = make_output(*mgr, "logits", 128);
  std::vector<NamedOutput> outputs{out};

  auto report = release_partial_outputs(*mgr, outputs);
  EXPECT_EQ(report.buffers_released, 1u);
  // Input buffer is still active.
  EXPECT_EQ(mgr->accounting().active_count, 1u);

  ASSERT_TRUE(mgr->release(in.buffer).has_value());
}

TEST(BufferCleanup, ReportsReleaseFailureWithoutMaskingOriginalError) {
  auto mgr = make_manager();
  auto in = make_input(*mgr, "image", 256);
  // Simulate a previous (incorrect) release path having already freed
  // the storage: we release the buffer here, then the helper tries
  // again and should record an error.
  ASSERT_TRUE(mgr->release(in.buffer).has_value());

  // Pretend we still hold the un-tombstoned handle (e.g., a copy
  // captured before the early release).
  std::vector<NamedInput> inputs{in};
  auto req = InferRequest::create("req-stale", "/policy", std::move(inputs));
  ASSERT_TRUE(req.has_value());

  auto report = release_request_buffers(*mgr, req.value());
  EXPECT_EQ(report.buffers_released, 0u);
  EXPECT_FALSE(report.clean());
  ASSERT_EQ(report.errors.size(), 1u);
  EXPECT_EQ(report.errors.front().code, Error::Code::Internal);
}

TEST(RequestBufferGuard, ReleasesOnScopeExitUnlessDismissed) {
  auto mgr = make_manager();
  {
    std::vector<NamedInput> inputs;
    inputs.push_back(make_input(*mgr, "x", 64));
    inputs.push_back(make_input(*mgr, "y", 64));
    auto req = InferRequest::create("req-2", "/policy", std::move(inputs));
    ASSERT_TRUE(req.has_value());

    RequestBufferGuard guard(*mgr, req.value());
    EXPECT_EQ(mgr->accounting().active_count, 2u);
    // Leaving scope without dismissing must release everything.
  }
  EXPECT_EQ(mgr->accounting().active_count, 0u);
}

TEST(RequestBufferGuard, DismissalKeepsBuffersAlive) {
  auto mgr = make_manager();
  std::vector<BufferRef> alive;
  {
    std::vector<NamedInput> inputs;
    inputs.push_back(make_input(*mgr, "x", 64));
    auto req = InferRequest::create("req-3", "/policy", std::move(inputs));
    ASSERT_TRUE(req.has_value());

    RequestBufferGuard guard(*mgr, req.value());
    guard.dismiss();
    EXPECT_TRUE(guard.dismissed());
    alive.push_back(req.value().inputs().front().buffer);
  }
  EXPECT_EQ(mgr->accounting().active_count, 1u);
  // Explicit release on the success path.
  ASSERT_TRUE(mgr->release(alive.front()).has_value());
}

}  // namespace
