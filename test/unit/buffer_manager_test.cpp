// SPDX-License-Identifier: Apache-2.0
//
// V01-E03-F01-T03 and V01-E03-F02-T01 / T02: BufferManager allocation,
// accounting, release, and copy/move tests.

#include "tensorplate/buffer/buffer_manager.hpp"

#include <cstddef>
#include <cstring>
#include <memory>
#include <unordered_set>
#include <utility>
#include <vector>

#include <gtest/gtest.h>

#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/error.hpp"

namespace {

using tensorplate::BufferManager;
using tensorplate::BufferManagerConfig;
using tensorplate::BufferOwnership;
using tensorplate::BufferRef;
using tensorplate::DType;
using tensorplate::Error;
using tensorplate::MemoryPressure;
using tensorplate::TensorView;

std::unique_ptr<BufferManager> make_manager(std::size_t capacity = 4096,
                                            std::size_t per_buffer = 1024) {
  BufferManagerConfig cfg;
  cfg.pool_name = "test_pool";
  cfg.capacity_bytes = capacity;
  cfg.max_buffer_bytes = per_buffer;
  auto r = BufferManager::create(std::move(cfg));
  EXPECT_TRUE(r.has_value()) << (r.has_value() ? "" : r.error().message);
  return std::move(r).value();
}

// ----- F01-T01: API surface + config validation -----

TEST(BufferManagerConfig, RejectsZeroCapacity) {
  BufferManagerConfig cfg;
  cfg.capacity_bytes = 0;
  auto r = BufferManager::create(std::move(cfg));
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(BufferManagerConfig, RejectsNonPowerOfTwoAlignment) {
  BufferManagerConfig cfg;
  cfg.default_alignment = 3;
  auto r = BufferManager::create(std::move(cfg));
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(BufferManagerConfig, RejectsInvalidThresholds) {
  {
    BufferManagerConfig cfg;
    cfg.warning_threshold = 0.0;
    auto r = BufferManager::create(std::move(cfg));
    ASSERT_FALSE(r.has_value());
    EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
  }
  {
    BufferManagerConfig cfg;
    cfg.warning_threshold = 0.9;
    cfg.critical_threshold = 0.5;
    auto r = BufferManager::create(std::move(cfg));
    ASSERT_FALSE(r.has_value());
    EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
  }
}

TEST(BufferManagerConfig, EmptyPoolNameDefaultsToDefault) {
  BufferManagerConfig cfg;
  cfg.pool_name.clear();
  auto r = BufferManager::create(std::move(cfg));
  ASSERT_TRUE(r.has_value());
  EXPECT_EQ(r.value()->pool_name(), "default");
}

// ----- F01-T03: allocation, accounting, id uniqueness -----

TEST(BufferManagerAllocation, AllocationSucceedsAndUpdatesAccounting) {
  auto mgr = make_manager();
  const auto before = mgr->accounting();
  EXPECT_EQ(before.active_count, 0u);
  EXPECT_EQ(before.in_use_bytes, 0u);

  auto h = mgr->allocate(256);
  ASSERT_TRUE(h.has_value());
  EXPECT_EQ(h.value().ownership(), BufferOwnership::Owned);
  EXPECT_EQ(h.value().size_bytes(), 256u);

  const auto after = mgr->accounting();
  EXPECT_EQ(after.active_count, 1u);
  EXPECT_EQ(after.in_use_bytes, 256u);
  EXPECT_EQ(after.high_water_bytes, 256u);
  EXPECT_EQ(after.allocation_failures, 0u);
  EXPECT_EQ(after.release_failures, 0u);
  EXPECT_EQ(after.pressure, MemoryPressure::Normal);

  ASSERT_TRUE(mgr->release(h.value()).has_value());
  const auto released = mgr->accounting();
  EXPECT_EQ(released.active_count, 0u);
  EXPECT_EQ(released.in_use_bytes, 0u);
  // High-water never decreases.
  EXPECT_EQ(released.high_water_bytes, 256u);
}

TEST(BufferManagerAllocation, RejectsZeroByteRequests) {
  auto mgr = make_manager();
  auto h = mgr->allocate(0);
  ASSERT_FALSE(h.has_value());
  EXPECT_EQ(h.error().code, Error::Code::ConfigInvalid);
  // Validation rejection does not count as an allocation failure
  // pressure signal — caller bug, not pressure.
  EXPECT_EQ(mgr->accounting().allocation_failures, 0u);
}

TEST(BufferManagerAllocation, RejectsRequestsAbovePerBufferCap) {
  auto mgr = make_manager(/*capacity=*/4096, /*per_buffer=*/1024);
  auto h = mgr->allocate(2048);
  ASSERT_FALSE(h.has_value());
  EXPECT_EQ(h.error().code, Error::Code::Unsupported);
}

TEST(BufferManagerAllocation, RejectsRequestsAboveRemainingCapacity) {
  auto mgr = make_manager(/*capacity=*/1024, /*per_buffer=*/1024);
  auto h1 = mgr->allocate(700);
  ASSERT_TRUE(h1.has_value());
  auto h2 = mgr->allocate(700);  // would total 1400 > 1024
  ASSERT_FALSE(h2.has_value());
  EXPECT_EQ(h2.error().code, Error::Code::OOMError);
  EXPECT_EQ(mgr->accounting().allocation_failures, 1u);
}

TEST(BufferManagerAllocation, RejectsInvalidAlignment) {
  auto mgr = make_manager();
  auto h = mgr->allocate(32, /*alignment=*/3);
  ASSERT_FALSE(h.has_value());
  EXPECT_EQ(h.error().code, Error::Code::ConfigInvalid);
}

TEST(BufferManagerAllocation, BufferIdsAreUniqueWhileActive) {
  auto mgr = make_manager();
  std::unordered_set<std::uint64_t> ids;
  std::vector<BufferRef> handles;
  for (int i = 0; i < 16; ++i) {
    auto h = mgr->allocate(8);
    ASSERT_TRUE(h.has_value());
    EXPECT_TRUE(ids.insert(h.value().id()).second) << "duplicate id " << h.value().id();
    handles.push_back(h.value());
  }
  for (auto& h : handles) {
    ASSERT_TRUE(mgr->release(h).has_value());
  }
}

TEST(BufferManagerAllocation, BufferIdsAreNotReusedAfterRelease) {
  auto mgr = make_manager();
  auto h1 = mgr->allocate(32);
  ASSERT_TRUE(h1.has_value());
  const auto id1 = h1.value().id();
  ASSERT_TRUE(mgr->release(h1.value()).has_value());
  auto h2 = mgr->allocate(32);
  ASSERT_TRUE(h2.has_value());
  EXPECT_NE(h2.value().id(), id1);
}

// ----- F02-T01: release semantics -----

TEST(BufferManagerRelease, DoubleReleaseReturnsTypedError) {
  auto mgr = make_manager();
  auto h = mgr->allocate(64);
  ASSERT_TRUE(h.has_value());
  ASSERT_TRUE(mgr->release(h.value()).has_value());
  auto second = mgr->release(h.value());
  ASSERT_FALSE(second.has_value());
  EXPECT_EQ(second.error().code, Error::Code::Internal);
  EXPECT_EQ(mgr->accounting().release_failures, 1u);
  // Accounting stays consistent with the one real release.
  EXPECT_EQ(mgr->accounting().active_count, 0u);
  EXPECT_EQ(mgr->accounting().in_use_bytes, 0u);
}

TEST(BufferManagerRelease, ReleaseOfUnknownHandleReturnsTypedError) {
  auto mgr = make_manager();
  auto fake = BufferRef::create(/*id=*/9999, /*size=*/16, BufferOwnership::Owned);
  ASSERT_TRUE(fake.has_value());
  auto r = mgr->release(fake.value());
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::Internal);
  EXPECT_EQ(mgr->accounting().release_failures, 1u);
}

TEST(BufferManagerRelease, ReleaseOfNullSentinelReturnsTypedError) {
  auto mgr = make_manager();
  auto r = mgr->release(BufferRef{});
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::Internal);
}

TEST(BufferManagerRelease, ReleaseIfOwnedIgnoresReleasedHandles) {
  auto mgr = make_manager();
  auto h = mgr->allocate(64);
  ASSERT_TRUE(h.has_value());

  // First call succeeds and frees storage.
  auto r1 = mgr->release_if_owned(h.value());
  ASSERT_TRUE(r1.has_value());
  EXPECT_TRUE(r1.value());

  // Second call on a copy is now a copy of a Released ref (caller would
  // have updated it); pass an explicitly released copy.
  BufferRef tombstoned = h.value();
  tombstoned.mark_released();
  auto r2 = mgr->release_if_owned(tombstoned);
  ASSERT_TRUE(r2.has_value());
  EXPECT_FALSE(r2.value());

  // The null sentinel is also a no-op.
  auto r3 = mgr->release_if_owned(BufferRef{});
  ASSERT_TRUE(r3.has_value());
  EXPECT_FALSE(r3.value());
}

TEST(BufferManagerRelease, DataAccessAfterReleaseFails) {
  auto mgr = make_manager();
  auto h = mgr->allocate(32);
  ASSERT_TRUE(h.has_value());
  ASSERT_TRUE(mgr->release(h.value()).has_value());
  auto d = mgr->data(h.value());
  ASSERT_FALSE(d.has_value());
  EXPECT_EQ(d.error().code, Error::Code::Internal);
}

// ----- F02-T02: copy/move behavior against release semantics -----

TEST(BufferManagerCopy, BufferRefCopiesShareReleaseFate) {
  auto mgr = make_manager();
  auto h = mgr->allocate(128);
  ASSERT_TRUE(h.has_value());
  BufferRef a = h.value();
  BufferRef b = a;  // value-object copy preserves identity
  EXPECT_EQ(a, b);

  // Release through one copy.
  ASSERT_TRUE(mgr->release(a).has_value());
  // Data access through the *other* copy must fail; storage is gone.
  auto d = mgr->data(b);
  ASSERT_FALSE(d.has_value());
  EXPECT_EQ(d.error().code, Error::Code::Internal);
}

TEST(BufferManagerCopy, MovedBufferRefStillIdentifiesSameStorage) {
  auto mgr = make_manager();
  auto h = mgr->allocate(64);
  ASSERT_TRUE(h.has_value());
  BufferRef src = h.value();
  BufferRef dst = std::move(src);
  EXPECT_EQ(dst.size_bytes(), 64u);
  // Storage is reachable through the destination.
  auto d = mgr->data(dst);
  ASSERT_TRUE(d.has_value());
  EXPECT_EQ(d.value().size(), 64u);
  ASSERT_TRUE(mgr->release(dst).has_value());
}

// ----- Data access + view bounds -----

TEST(BufferManagerData, RoundTripsBytes) {
  auto mgr = make_manager();
  auto h = mgr->allocate(16);
  ASSERT_TRUE(h.has_value());
  auto write = mgr->data(h.value());
  ASSERT_TRUE(write.has_value());
  for (std::size_t i = 0; i < write.value().size(); ++i) {
    write.value()[i] = static_cast<std::byte>(i & 0xFF);
  }
  auto read = mgr->data(h.value());
  ASSERT_TRUE(read.has_value());
  for (std::size_t i = 0; i < read.value().size(); ++i) {
    EXPECT_EQ(read.value()[i], static_cast<std::byte>(i & 0xFF));
  }
  ASSERT_TRUE(mgr->release(h.value()).has_value());
}

TEST(BufferManagerView, RejectsOutOfBoundsTensorWindow) {
  auto mgr = make_manager();
  auto h = mgr->allocate(64);
  ASSERT_TRUE(h.has_value());

  auto good = TensorView::create(DType::UInt8, {32}, tensorplate::Layout::RowMajor,
                                 /*byte_offset=*/0, /*byte_size=*/32);
  ASSERT_TRUE(good.has_value());
  auto gv = mgr->view(h.value(), good.value());
  ASSERT_TRUE(gv.has_value());
  EXPECT_EQ(gv.value().size(), 32u);

  auto bad = TensorView::create(DType::UInt8, {65}, tensorplate::Layout::RowMajor,
                                /*byte_offset=*/0, /*byte_size=*/65);
  ASSERT_TRUE(bad.has_value());
  auto bv = mgr->view(h.value(), bad.value());
  ASSERT_FALSE(bv.has_value());
  EXPECT_EQ(bv.error().code, Error::Code::ShapeMismatch);

  auto offset_overflow =
      TensorView::create(DType::UInt8, {32}, tensorplate::Layout::RowMajor,
                         /*byte_offset=*/40, /*byte_size=*/32);
  ASSERT_TRUE(offset_overflow.has_value());
  auto ov = mgr->view(h.value(), offset_overflow.value());
  ASSERT_FALSE(ov.has_value());
  EXPECT_EQ(ov.error().code, Error::Code::ShapeMismatch);

  ASSERT_TRUE(mgr->release(h.value()).has_value());
}

// ----- Destruction reclaims still-active storage -----

TEST(BufferManagerLifetime, DestructorReclaimsActiveStorage) {
  // ASAN/UBSAN catches missed frees; this test exists so the destructor
  // branch is exercised under sanitizer-enabled builds.
  auto mgr = make_manager();
  auto h1 = mgr->allocate(32);
  auto h2 = mgr->allocate(64);
  ASSERT_TRUE(h1.has_value());
  ASSERT_TRUE(h2.has_value());
  // Drop the manager without explicit releases. ASAN should report no
  // leaks; the destructor frees everything it owns.
}

}  // namespace
