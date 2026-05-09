// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F01-T03 unit coverage for tensorplate::Error.

#include "tensorplate/core/error.hpp"

#include <gtest/gtest.h>

#include <string>

namespace {

using tensorplate::Error;
using tensorplate::error_code_from_string;
using tensorplate::format;
using tensorplate::to_string;

TEST(Error, AllCodesHaveStableSerializedNames) {
  EXPECT_EQ(to_string(Error::Code::ConfigInvalid), "config_invalid");
  EXPECT_EQ(to_string(Error::Code::LoadFailed), "load_failed");
  EXPECT_EQ(to_string(Error::Code::NotReady), "not_ready");
  EXPECT_EQ(to_string(Error::Code::ShapeMismatch), "shape_mismatch");
  EXPECT_EQ(to_string(Error::Code::Unsupported), "unsupported");
  EXPECT_EQ(to_string(Error::Code::OOMError), "oom_error");
  EXPECT_EQ(to_string(Error::Code::Timeout), "timeout");
  EXPECT_EQ(to_string(Error::Code::InferenceFailed), "inference_failed");
  EXPECT_EQ(to_string(Error::Code::Internal), "internal");
}

TEST(Error, FromStringRoundTripsKnownCodes) {
  for (auto code : {Error::Code::ConfigInvalid, Error::Code::LoadFailed, Error::Code::NotReady,
                    Error::Code::ShapeMismatch, Error::Code::Unsupported, Error::Code::OOMError,
                    Error::Code::Timeout, Error::Code::InferenceFailed, Error::Code::Internal}) {
    auto parsed = error_code_from_string(to_string(code));
    ASSERT_TRUE(parsed.has_value()) << "round-trip failed for " << to_string(code);
    EXPECT_EQ(*parsed, code);
  }
}

TEST(Error, FromStringRejectsUnknownNames) {
  EXPECT_FALSE(error_code_from_string("does_not_exist").has_value());
  EXPECT_FALSE(error_code_from_string("").has_value());
  // Wrong case (we're snake_case lowercase only).
  EXPECT_FALSE(error_code_from_string("ConfigInvalid").has_value());
}

TEST(Error, MakeWithoutContext) {
  auto e = Error::make(Error::Code::Timeout, "deadline exceeded");
  EXPECT_EQ(e.code, Error::Code::Timeout);
  EXPECT_EQ(e.message, "deadline exceeded");
  EXPECT_FALSE(e.context.has_value());
}

TEST(Error, MakeWithContextPreservesAllFields) {
  auto e =
      Error::make(Error::Code::ShapeMismatch, "rank mismatch", "input=image_front rank=4");
  EXPECT_EQ(e.code, Error::Code::ShapeMismatch);
  EXPECT_EQ(e.message, "rank mismatch");
  ASSERT_TRUE(e.context.has_value());
  EXPECT_EQ(*e.context, "input=image_front rank=4");
}

TEST(Error, EqualityComparesAllThreeFields) {
  Error a{Error::Code::Timeout, "x", std::nullopt};
  Error b{Error::Code::Timeout, "x", std::nullopt};
  Error c{Error::Code::Internal, "x", std::nullopt};
  Error d{Error::Code::Timeout, "y", std::nullopt};
  Error e{Error::Code::Timeout, "x", std::optional<std::string>{"ctx"}};

  EXPECT_EQ(a, b);
  EXPECT_NE(a, c);
  EXPECT_NE(a, d);
  EXPECT_NE(a, e);
}

TEST(Error, FormatIncludesCodeAndMessage) {
  auto e = Error::make(Error::Code::OOMError, "buffer pool empty");
  std::string s = format(e);
  EXPECT_NE(s.find("oom_error"), std::string::npos);
  EXPECT_NE(s.find("buffer pool empty"), std::string::npos);
}

TEST(Error, FormatIncludesContextWhenPresent) {
  auto e = Error::make(Error::Code::ConfigInvalid, "bad value", "field=precision");
  std::string s = format(e);
  EXPECT_NE(s.find("config_invalid"), std::string::npos);
  EXPECT_NE(s.find("bad value"), std::string::npos);
  EXPECT_NE(s.find("field=precision"), std::string::npos);
}

}  // namespace
