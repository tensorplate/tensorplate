// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F01-T03 unit coverage for tensorplate::Error.

#include "tensorplate/core/error.hpp"

#include <gtest/gtest.h>

#include <cstddef>
#include <cstdint>
#include <fstream>
#include <nlohmann/json.hpp>
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
  EXPECT_EQ(to_string(Error::Code::Cancelled), "cancelled");
  EXPECT_EQ(to_string(Error::Code::Unavailable), "unavailable");
  EXPECT_EQ(to_string(Error::Code::ResourceExhausted), "resource_exhausted");
}

TEST(Error, NumericValuesAreAppendOnly) {
  // The numeric values are C++ ABI: new codes append, existing ones never move.
  EXPECT_EQ(static_cast<std::uint32_t>(Error::Code::ConfigInvalid), 0U);
  EXPECT_EQ(static_cast<std::uint32_t>(Error::Code::LoadFailed), 1U);
  EXPECT_EQ(static_cast<std::uint32_t>(Error::Code::NotReady), 2U);
  EXPECT_EQ(static_cast<std::uint32_t>(Error::Code::ShapeMismatch), 3U);
  EXPECT_EQ(static_cast<std::uint32_t>(Error::Code::Unsupported), 4U);
  EXPECT_EQ(static_cast<std::uint32_t>(Error::Code::OOMError), 5U);
  EXPECT_EQ(static_cast<std::uint32_t>(Error::Code::Timeout), 6U);
  EXPECT_EQ(static_cast<std::uint32_t>(Error::Code::InferenceFailed), 7U);
  EXPECT_EQ(static_cast<std::uint32_t>(Error::Code::Internal), 8U);
  EXPECT_EQ(static_cast<std::uint32_t>(Error::Code::Cancelled), 9U);
  EXPECT_EQ(static_cast<std::uint32_t>(Error::Code::Unavailable), 10U);
  EXPECT_EQ(static_cast<std::uint32_t>(Error::Code::ResourceExhausted), 11U);
}

TEST(Error, FromStringRoundTripsKnownCodes) {
  for (auto code :
       {Error::Code::ConfigInvalid, Error::Code::LoadFailed, Error::Code::NotReady,
        Error::Code::ShapeMismatch, Error::Code::Unsupported, Error::Code::OOMError,
        Error::Code::Timeout, Error::Code::InferenceFailed, Error::Code::Internal,
        Error::Code::Cancelled, Error::Code::Unavailable, Error::Code::ResourceExhausted}) {
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
  // Near misses of the appended names: US spelling, case, separator.
  EXPECT_FALSE(error_code_from_string("canceled").has_value());
  EXPECT_FALSE(error_code_from_string("Cancelled").has_value());
  EXPECT_FALSE(error_code_from_string("UNAVAILABLE").has_value());
  EXPECT_FALSE(error_code_from_string("resource-exhausted").has_value());
}

TEST(Error, NamesMatchTheErrorSchemaInOrder) {
  // protocol/schemas/error.json is the wire source of truth and lists the
  // names in numeric order. A name only the schema has meets the "internal"
  // fallback at its index; a code only C++ has is named past the schema's end.
  std::ifstream in(std::string{TP_SOURCE_DIR} + "/protocol/schemas/error.json");
  ASSERT_TRUE(in.is_open());
  const auto schema = nlohmann::json::parse(in);
  const auto& names = schema.at("properties").at("code").at("enum");
  ASSERT_TRUE(names.is_array());
  ASSERT_FALSE(names.empty());
  for (std::size_t i = 0; i < names.size(); ++i) {
    const auto code = static_cast<Error::Code>(static_cast<std::uint32_t>(i));
    const auto expected = names[i].get<std::string>();
    EXPECT_EQ(std::string{to_string(code)}, expected) << "numeric value " << i;
    const auto parsed = error_code_from_string(expected);
    ASSERT_TRUE(parsed.has_value()) << expected;
    EXPECT_EQ(*parsed, code) << expected;
  }
  const auto past_end = static_cast<Error::Code>(static_cast<std::uint32_t>(names.size()));
  EXPECT_EQ(to_string(past_end), "internal") << "C++ names a code error.json does not list";
}

TEST(Error, MakeWithoutContext) {
  auto e = Error::make(Error::Code::Timeout, "deadline exceeded");
  EXPECT_EQ(e.code, Error::Code::Timeout);
  EXPECT_EQ(e.message, "deadline exceeded");
  EXPECT_FALSE(e.context.has_value());
}

TEST(Error, MakeWithContextPreservesAllFields) {
  auto e = Error::make(Error::Code::ShapeMismatch, "rank mismatch", "input=image_front rank=4");
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
