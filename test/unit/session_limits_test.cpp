// SPDX-License-Identifier: Apache-2.0
#include <gtest/gtest.h>

#include <chrono>
#include <fstream>
#include <nlohmann/json.hpp>
#include <string>

#include "tensorplate/serving/session.hpp"

namespace tensorplate::serving {
namespace {
using namespace std::chrono_literals;

Result<SessionLimits> from_fixture(const nlohmann::json& row) {
  return SessionLimits::create(
      std::chrono::milliseconds{row.at("idle_timeout_ms").get<std::int64_t>()},
      std::chrono::milliseconds{row.at("heartbeat_interval_ms").get<std::int64_t>()},
      std::chrono::milliseconds{row.at("liveness_timeout_ms").get<std::int64_t>()},
      std::chrono::milliseconds{row.at("max_duration_ms").get<std::int64_t>()},
      row.at("max_sessions").get<std::uint32_t>());
}

TEST(SessionLimits, DefaultsAndInvalidSettingsMatchFixture) {
  const std::string path = std::string(TP_SOURCE_DIR) + "/test/unit/fixtures/session_limits.json";
  std::ifstream input(path);
  ASSERT_TRUE(input.good()) << path;
  const auto fixture = nlohmann::json::parse(input);
  const auto expected = from_fixture(fixture.at("defaults"));
  ASSERT_TRUE(expected);
  EXPECT_EQ(SessionLimits::defaults(), *expected);
  EXPECT_EQ(expected->idle_timeout(), 60s);
  EXPECT_EQ(expected->heartbeat_interval(), 10s);
  EXPECT_EQ(expected->liveness_timeout(), 30s);
  EXPECT_EQ(expected->max_duration(), 60min);
  EXPECT_EQ(expected->max_sessions(), 2048U);

  const auto& invalid = fixture.at("invalid");
  ASSERT_EQ(invalid.size(), 7U);
  for (std::size_t i = 0; i < invalid.size(); ++i) {
    SCOPED_TRACE(i);
    const auto result = from_fixture(invalid[i]);
    ASSERT_FALSE(result);
    EXPECT_EQ(result.error().code, Error::Code::ConfigInvalid);
    EXPECT_EQ(result.error().context, "invalid_session_limits");
  }
}

TEST(SessionLimits, AllowsShortValidDurationsForDeterministicTimers) {
  const auto limits = SessionLimits::create(2s, 1s, 3s, 5s, 2);
  ASSERT_TRUE(limits);
  EXPECT_EQ(limits->max_sessions(), 2U);
}
}  // namespace
}  // namespace tensorplate::serving
