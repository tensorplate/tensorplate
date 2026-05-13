// SPDX-License-Identifier: Apache-2.0
//
// V01-E03-F04-T01 / T02 unit coverage for ingress copy and multi-input
// build helpers.

#include <gtest/gtest.h>

#include <array>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <memory>
#include <unordered_set>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/cleanup.hpp"
#include "tensorplate/buffer/ingress.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/infer_request.hpp"

#include "ingress_fixtures.hpp"

namespace {

using tensorplate::BufferManager;
using tensorplate::BufferManagerConfig;
using tensorplate::build_named_inputs;
using tensorplate::copy_payload_into_buffer;
using tensorplate::DType;
using tensorplate::Error;
using tensorplate::IngressInput;
using tensorplate::IngressLimits;
using tensorplate::NamedInput;
using tensorplate::TensorView;

std::unique_ptr<BufferManager> make_manager(std::size_t capacity = 1024 * 1024,
                                            std::size_t per_buffer = 256 * 1024) {
  BufferManagerConfig cfg;
  cfg.pool_name = "ingress_test";
  cfg.capacity_bytes = capacity;
  cfg.max_buffer_bytes = per_buffer;
  auto r = BufferManager::create(std::move(cfg));
  EXPECT_TRUE(r.has_value());
  return std::move(r).value();
}

// ----- F04-T01: copy helper -----

TEST(IngressCopy, CopiesExactBytesIntoOwnedStorage) {
  auto mgr = make_manager();
  std::array<std::byte, 4> input{std::byte{0xDE}, std::byte{0xAD}, std::byte{0xBE},
                                 std::byte{0xEF}};
  auto h = copy_payload_into_buffer(*mgr, std::span<const std::byte>(input));
  ASSERT_TRUE(h.has_value());
  EXPECT_EQ(h.value().size_bytes(), 4u);

  auto seen = mgr->data(h.value());
  ASSERT_TRUE(seen.has_value());
  ASSERT_EQ(seen.value().size(), 4u);
  EXPECT_EQ(0, std::memcmp(seen.value().data(), input.data(), 4));

  ASSERT_TRUE(mgr->release(h.value()).has_value());
}

TEST(IngressCopy, RejectsEmptyPayload) {
  auto mgr = make_manager();
  std::span<const std::byte> empty;
  auto h = copy_payload_into_buffer(*mgr, empty);
  ASSERT_FALSE(h.has_value());
  EXPECT_EQ(h.error().code, Error::Code::ConfigInvalid);
  EXPECT_EQ(mgr->accounting().active_count, 0u);
}

TEST(IngressCopy, OversizedPayloadIsRejectedByPerBufferCap) {
  auto mgr = make_manager(/*capacity=*/1024 * 1024, /*per_buffer=*/512);
  std::vector<std::byte> bytes(1024, std::byte{0});
  auto h = copy_payload_into_buffer(*mgr, std::span<const std::byte>(bytes));
  ASSERT_FALSE(h.has_value());
  EXPECT_EQ(h.error().code, Error::Code::Unsupported);
  EXPECT_EQ(mgr->accounting().active_count, 0u);
}

// ----- F04-T02: multi-input build (vision + VLA) -----

TEST(IngressBuild, VisionFixtureBuildsOneNamedInput) {
  auto mgr = make_manager();
  const auto fixtures = tensorplate::testing::make_vision_fixture(/*h=*/32, /*w=*/32, /*c=*/3);
  ASSERT_EQ(fixtures.size(), 1u);

  auto inputs = build_named_inputs(*mgr, tensorplate::testing::as_ingress_inputs(fixtures));
  ASSERT_TRUE(inputs.has_value());
  ASSERT_EQ(inputs.value().size(), 1u);
  EXPECT_EQ(inputs.value().front().name, "image");
  EXPECT_EQ(inputs.value().front().buffer.size_bytes(), 32u * 32u * 3u);
  EXPECT_EQ(inputs.value().front().tensor.dtype(), DType::UInt8);
  EXPECT_EQ(mgr->accounting().active_count, 1u);

  // Cleanup so the test does not rely on the manager destructor.
  for (auto& in : inputs.value()) {
    ASSERT_TRUE(mgr->release(in.buffer).has_value());
  }
}

TEST(IngressBuild, SmolVLAFixtureProducesMultipleDistinctBuffers) {
  auto mgr = make_manager();
  const auto fixtures = tensorplate::testing::make_smolvla_fixture();
  ASSERT_EQ(fixtures.size(), 4u);

  auto inputs = build_named_inputs(*mgr, tensorplate::testing::as_ingress_inputs(fixtures));
  ASSERT_TRUE(inputs.has_value());
  ASSERT_EQ(inputs.value().size(), 4u);
  EXPECT_EQ(mgr->accounting().active_count, 4u);

  // All buffer ids must be distinct.
  std::unordered_set<std::uint64_t> ids;
  for (const auto& in : inputs.value()) {
    ids.insert(in.buffer.id());
  }
  EXPECT_EQ(ids.size(), 4u);

  // Payload bytes round-trip through the manager.
  for (std::size_t i = 0; i < inputs.value().size(); ++i) {
    auto seen = mgr->data(inputs.value()[i].buffer);
    ASSERT_TRUE(seen.has_value());
    ASSERT_EQ(seen.value().size(), fixtures[i].bytes.size());
    EXPECT_EQ(0, std::memcmp(seen.value().data(), fixtures[i].bytes.data(), seen.value().size()));
  }

  for (auto& in : inputs.value()) {
    ASSERT_TRUE(mgr->release(in.buffer).has_value());
  }
}

TEST(IngressBuild, RejectsDuplicateInputNamesAndReleasesPartialAllocations) {
  auto mgr = make_manager();
  auto vision = tensorplate::testing::make_vision_fixture(8, 8, 3);
  // Force a duplicate name in the descriptor list.
  std::vector<IngressInput> inputs;
  inputs.push_back(tensorplate::testing::as_ingress_inputs(vision).front());
  inputs.push_back(tensorplate::testing::as_ingress_inputs(vision).front());

  auto r = build_named_inputs(*mgr, inputs);
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
  // The first allocation must have been rolled back.
  EXPECT_EQ(mgr->accounting().active_count, 0u);
  EXPECT_EQ(mgr->accounting().in_use_bytes, 0u);
}

TEST(IngressBuild, MalformedTensorMetadataReleasesAllocatedBuffer) {
  auto mgr = make_manager();
  auto vision = tensorplate::testing::make_vision_fixture(8, 8, 3);
  auto inputs = tensorplate::testing::as_ingress_inputs(vision);
  // Replace the tensor window with one that exceeds the payload size.
  auto bad_view = TensorView::create(DType::UInt8, {8, 8, 3}, tensorplate::Layout::RowMajor,
                                     /*byte_offset=*/0,
                                     /*byte_size=*/8 * 8 * 3 + 1024);
  ASSERT_TRUE(bad_view.has_value());
  inputs.front().tensor = bad_view.value();

  auto r = build_named_inputs(*mgr, inputs);
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ShapeMismatch);
  EXPECT_EQ(mgr->accounting().active_count, 0u);
}

TEST(IngressBuild, EnforcesMaxTotalBytesLimit) {
  auto mgr = make_manager();
  IngressLimits limits;
  limits.max_total_bytes = 16;

  auto vision = tensorplate::testing::make_vision_fixture(8, 8, 3);  // 192 bytes
  auto inputs = tensorplate::testing::as_ingress_inputs(vision);
  auto r = build_named_inputs(*mgr, inputs, limits);
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::Unsupported);
  EXPECT_EQ(mgr->accounting().active_count, 0u);
}

TEST(IngressBuild, IntegratesIntoInferRequest) {
  // End-to-end pass: raw bytes -> NamedInput vector -> InferRequest.
  auto mgr = make_manager();
  const auto fixtures = tensorplate::testing::make_smolvla_fixture();
  auto inputs = build_named_inputs(*mgr, tensorplate::testing::as_ingress_inputs(fixtures));
  ASSERT_TRUE(inputs.has_value());

  auto req = tensorplate::InferRequest::create("req-vla-1", "/policy", std::move(inputs.value()));
  ASSERT_TRUE(req.has_value());
  EXPECT_EQ(req.value().inputs().size(), 4u);

  // Cleanup via the F03 helper exercises both paths together.
  auto report = tensorplate::release_request_buffers(*mgr, req.value());
  EXPECT_TRUE(report.clean());
  EXPECT_EQ(mgr->accounting().active_count, 0u);
}

}  // namespace
