// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F01-T01 / T03 unit coverage for tensorplate::Result<T>.

#include "tensorplate/core/result.hpp"

#include <gtest/gtest.h>

#include <string>
#include <utility>

namespace {

using tensorplate::BadResultAccess;
using tensorplate::Error;
using tensorplate::Result;
using tensorplate::unexpected;

TEST(Result, ValueConstructionHoldsValue) {
  Result<int> r{42};
  EXPECT_TRUE(r.has_value());
  EXPECT_TRUE(static_cast<bool>(r));
  EXPECT_EQ(r.value(), 42);
}

TEST(Result, UnexpectedConstructionHoldsError) {
  Result<int> r = unexpected(Error::Code::Internal, "boom");
  EXPECT_FALSE(r.has_value());
  EXPECT_FALSE(static_cast<bool>(r));
  EXPECT_EQ(r.error().code, Error::Code::Internal);
  EXPECT_EQ(r.error().message, "boom");
}

TEST(Result, ValueAccessOnErrorThrowsBadResultAccess) {
  Result<int> r = unexpected(Error::Code::Timeout, "slow");
  EXPECT_THROW(static_cast<void>(r.value()), BadResultAccess);
}

TEST(Result, ErrorAccessOnValueThrowsBadResultAccess) {
  Result<int> r{1};
  EXPECT_THROW(static_cast<void>(r.error()), BadResultAccess);
}

TEST(Result, ValueOrReturnsDefaultOnError) {
  Result<int> err = unexpected(Error::Code::Timeout, "slow");
  Result<int> ok{7};
  EXPECT_EQ(err.value_or(99), 99);
  EXPECT_EQ(ok.value_or(99), 7);
}

TEST(Result, MoveOnlyTypesAreSupported) {
  Result<std::unique_ptr<int>> r{std::make_unique<int>(123)};
  ASSERT_TRUE(r.has_value());
  EXPECT_EQ(*r.value(), 123);

  // Move out of the held value via rvalue access.
  std::unique_ptr<int> taken = std::move(r).value();
  ASSERT_NE(taken, nullptr);
  EXPECT_EQ(*taken, 123);
}

TEST(Result, EqualityComparesPayload) {
  Result<int> a{1};
  Result<int> b{1};
  Result<int> c{2};
  Result<int> d = unexpected(Error::Code::Internal, "x");
  Result<int> e = unexpected(Error::Code::Internal, "x");
  Result<int> f = unexpected(Error::Code::Timeout, "x");

  EXPECT_EQ(a, b);
  EXPECT_NE(a, c);
  EXPECT_NE(a, d);
  EXPECT_EQ(d, e);
  EXPECT_NE(d, f);
}

TEST(Result, ArrowAndDereferenceForwardsToValue) {
  Result<std::string> r{std::string{"hello"}};
  EXPECT_EQ(r->size(), 5u);
  EXPECT_EQ(*r, "hello");
}

TEST(ResultVoid, DefaultConstructionIsSuccess) {
  Result<void> r;
  EXPECT_TRUE(r.has_value());
  EXPECT_TRUE(static_cast<bool>(r));
  EXPECT_NO_THROW(r.value());
}

TEST(ResultVoid, ErrorConstructionIsFailure) {
  Result<void> r = unexpected(Error::Code::ConfigInvalid, "bad");
  EXPECT_FALSE(r.has_value());
  EXPECT_THROW(r.value(), BadResultAccess);
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(ResultVoid, EqualityComparesErrorState) {
  Result<void> a;
  Result<void> b;
  Result<void> c = unexpected(Error::Code::Internal, "x");
  Result<void> d = unexpected(Error::Code::Internal, "x");
  Result<void> e = unexpected(Error::Code::Timeout, "x");

  EXPECT_EQ(a, b);
  EXPECT_NE(a, c);
  EXPECT_EQ(c, d);
  EXPECT_NE(c, e);
}

// Compile-time documentation that Result<T> carries [[nodiscard]] so that
// ignoring a hardware-boundary status is a warning, not silent UB.
[[maybe_unused]] Result<int> nodiscard_check_helper() { return 0; }

TEST(Result, ResultsArePolymorphicAcrossNamespaceAlias) {
  // The planning doc references types as `tp::Result<T>`; the alias must work.
  static_assert(std::is_same_v<tp::Result<int>, tensorplate::Result<int>>);
  static_assert(std::is_same_v<tp::Error::Code, tensorplate::Error::Code>);
  SUCCEED();
}

}  // namespace
