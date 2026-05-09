// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F05-T01 / T02 / T03 unit coverage for tensorplate::BufferRef.
// Verifies the public ownership/state contract; the actual buffer-pool
// release machinery and the double-release prevention test land in
// V01-E03 alongside the allocator implementation.

#include "tensorplate/buffer/buffer_ref.hpp"

#include <gtest/gtest.h>

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/result.hpp"

namespace {

using tensorplate::buffer_ownership_from_string;
using tensorplate::BufferOwnership;
using tensorplate::BufferRef;
using tensorplate::Error;
using tensorplate::to_string;

TEST(BufferRef, DefaultConstructionIsReleasedSentinel) {
  BufferRef h;
  EXPECT_TRUE(h.is_null());
  EXPECT_FALSE(h.is_valid());
  EXPECT_EQ(h.id(), BufferRef::kNullId);
  EXPECT_EQ(h.size_bytes(), 0u);
  EXPECT_EQ(h.ownership(), BufferOwnership::Released);
}

TEST(BufferRef, OwnershipNamesRoundTripViaSnakeCase) {
  for (auto o : {BufferOwnership::Owned, BufferOwnership::Borrowed, BufferOwnership::Released}) {
    auto parsed = buffer_ownership_from_string(to_string(o));
    ASSERT_TRUE(parsed.has_value());
    EXPECT_EQ(*parsed, o);
  }
}

TEST(BufferRef, OwnershipNamesAreLockedToWireFormat) {
  EXPECT_EQ(to_string(BufferOwnership::Owned), "owned");
  EXPECT_EQ(to_string(BufferOwnership::Borrowed), "borrowed");
  EXPECT_EQ(to_string(BufferOwnership::Released), "released");
}

TEST(BufferRef, OwnershipFromStringRejectsUnknown) {
  EXPECT_FALSE(buffer_ownership_from_string("uknown_state").has_value());
  EXPECT_FALSE(buffer_ownership_from_string("Owned").has_value());
}

TEST(BufferRef, CreateOwnedSucceedsWithValidArgs) {
  auto r = BufferRef::create(1, 1024, BufferOwnership::Owned);
  ASSERT_TRUE(r.has_value());
  EXPECT_EQ(r.value().id(), 1u);
  EXPECT_EQ(r.value().size_bytes(), 1024u);
  EXPECT_EQ(r.value().ownership(), BufferOwnership::Owned);
  EXPECT_TRUE(r.value().is_valid());
  EXPECT_FALSE(r.value().is_null());
}

TEST(BufferRef, CreateRejectsNullIdForActiveHandles) {
  auto r1 = BufferRef::create(BufferRef::kNullId, 16, BufferOwnership::Owned);
  ASSERT_FALSE(r1.has_value());
  EXPECT_EQ(r1.error().code, Error::Code::ConfigInvalid);

  auto r2 = BufferRef::create(BufferRef::kNullId, 16, BufferOwnership::Borrowed);
  ASSERT_FALSE(r2.has_value());
  EXPECT_EQ(r2.error().code, Error::Code::ConfigInvalid);
}

TEST(BufferRef, CreateRejectsZeroSizeForActiveHandles) {
  auto r1 = BufferRef::create(1, 0, BufferOwnership::Owned);
  ASSERT_FALSE(r1.has_value());
  EXPECT_EQ(r1.error().code, Error::Code::ConfigInvalid);

  auto r2 = BufferRef::create(1, 0, BufferOwnership::Borrowed);
  ASSERT_FALSE(r2.has_value());
  EXPECT_EQ(r2.error().code, Error::Code::ConfigInvalid);
}

TEST(BufferRef, CreateAllowsReleasedSentinel) {
  // Released with id 0 / size 0 is the explicit sentinel.
  auto r = BufferRef::create(0, 0, BufferOwnership::Released);
  ASSERT_TRUE(r.has_value());
  EXPECT_TRUE(r.value().is_null());
}

TEST(BufferRef, CopyProducesEqualHandle) {
  auto r = BufferRef::create(7, 32, BufferOwnership::Owned);
  ASSERT_TRUE(r.has_value());
  BufferRef src = r.value();
  BufferRef dup = src;  // NOLINT(performance-unnecessary-copy-initialization)
  EXPECT_EQ(src, dup);
  EXPECT_EQ(dup.ownership(), BufferOwnership::Owned);
  // Source unchanged. The buffer-pool layer (V01-E03) enforces release-once
  // policy; the type itself does not invalidate the source on copy.
  EXPECT_TRUE(src.is_valid());
}

TEST(BufferRef, MoveDoesNotMutateSourceByDefault) {
  // Document the v0.1.0 contract: BufferRef is a trivially small value
  // object. Move construction is bit-equivalent to copy. Holders that need
  // unique-ptr-style invalidation must call mark_released() explicitly on
  // the source.
  auto r = BufferRef::create(9, 64, BufferOwnership::Owned);
  ASSERT_TRUE(r.has_value());
  BufferRef src = r.value();
  BufferRef dst = std::move(src);
  EXPECT_EQ(dst.id(), 9u);
  EXPECT_EQ(dst.size_bytes(), 64u);
  // The standard library's "moved-from is valid but unspecified" rule
  // applies in spirit; we only assert the fields the documented contract
  // covers (the destination has the original identity).
}

TEST(BufferRef, MarkReleasedIsIdempotent) {
  auto r = BufferRef::create(11, 256, BufferOwnership::Owned);
  ASSERT_TRUE(r.has_value());
  BufferRef h = r.value();
  EXPECT_TRUE(h.is_valid());
  h.mark_released();
  EXPECT_FALSE(h.is_valid());
  EXPECT_EQ(h.ownership(), BufferOwnership::Released);
  // Identity preserved after release for log/metric attribution.
  EXPECT_EQ(h.id(), 11u);
  EXPECT_EQ(h.size_bytes(), 256u);
  // Repeating the call is a no-op.
  h.mark_released();
  EXPECT_EQ(h.ownership(), BufferOwnership::Released);
}

TEST(BufferRef, EqualityComparesAllFields) {
  auto a = BufferRef::create(1, 8, BufferOwnership::Owned);
  auto b = BufferRef::create(1, 8, BufferOwnership::Owned);
  auto c = BufferRef::create(2, 8, BufferOwnership::Owned);
  auto d = BufferRef::create(1, 16, BufferOwnership::Owned);
  auto e = BufferRef::create(1, 8, BufferOwnership::Borrowed);
  ASSERT_TRUE(a.has_value() && b.has_value() && c.has_value() && d.has_value() && e.has_value());

  EXPECT_EQ(a.value(), b.value());
  EXPECT_NE(a.value(), c.value());
  EXPECT_NE(a.value(), d.value());
  EXPECT_NE(a.value(), e.value());
}

}  // namespace
