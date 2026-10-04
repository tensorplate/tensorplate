// SPDX-License-Identifier: Apache-2.0
#include <gtest/gtest.h>

#include <cstdint>
#include <string>
#include <vector>

#include "serving/session/session_manager.hpp"
#include "session_lifecycle_fixture.hpp"

namespace tensorplate::serving {
namespace {
using testing::lifecycle_output_item;
using testing::load_session_lifecycle_fixture;

TEST(SessionFinalize, DrainExpiryMatchesFixture) {
  const auto fixture = load_session_lifecycle_fixture();
  const auto& rows = fixture.at("drain_expiry");
  ASSERT_EQ(rows.size(), 13U);
  for (const auto& row : rows) {
    SCOPED_TRACE(row.dump());
    const bool text = row.at("input") == "text";
    testing::FakeSchedulerClock clock;
    auto manager = SessionManager::create(
        SessionLimits::defaults(), fixture.at("generation").get<std::uint64_t>(), clock,
        [](const auto&) {}, {}, false);
    ASSERT_TRUE(manager);
    const auto opened = (*manager)->open(fixture.at("generation").get<std::uint64_t>(),
                                         text ? SessionBudgets::for_text_input(48'000).value()
                                              : SessionBudgets::for_audio_input(32'000).value());
    ASSERT_TRUE(opened);
    ASSERT_TRUE((*manager)->apply(opened->session_key, LogicalSessionEvent::HalfClose));
    const auto elapsed = testing::fixture_ms(row, "elapsed_ms");
    if (row.at("output_pending").get<bool>()) {
      const auto idle = testing::fixture_ms(row, "output_idle_ms");
      clock.advance(elapsed - idle);
      auto item = lifecycle_output_item(text ? OutputKind::Audio : OutputKind::Result, 960);
      ASSERT_EQ(opened->output->offer(item, clock.now()).value(), OutputOffer::Queued);
      clock.advance(idle);
    } else {
      clock.advance(elapsed);
    }
    const auto swept = (*manager)->sweep_due();
    ASSERT_LE(swept.size(), 1U);
    const std::string expired =
        swept.empty() ? "none" : swept[0].cause.value().context.value_or("");
    EXPECT_EQ(expired, row.at("expect").get<std::string>());
  }
}

std::vector<nlohmann::json> lifecycle_cases() {
  return load_session_lifecycle_fixture().at("cases").get<std::vector<nlohmann::json>>();
}

std::string case_test_name(const ::testing::TestParamInfo<nlohmann::json>& info) {
  return info.param.at("id").get<std::string>();
}

class SessionLifecycleCase : public ::testing::TestWithParam<nlohmann::json> {};

TEST_P(SessionLifecycleCase, ReplaysAgainstTheManager) {
  const auto fixture = load_session_lifecycle_fixture();
  const auto& scenario = GetParam();
  SCOPED_TRACE(scenario.at("name").get<std::string>());
  testing::SessionLifecycleReplay replay(fixture, scenario);
  replay.run(scenario.at("steps"));
  EXPECT_EQ(replay.stream().terminals_written, scenario.at("terminals").get<std::uint32_t>());
  if (scenario.contains("terminal_cause")) {
    const auto& cause = scenario.at("terminal_cause");
    EXPECT_EQ(replay.stream().terminal_cause,
              cause.is_null() ? std::nullopt : std::optional{cause.get<std::string>()});
  }
}

INSTANTIATE_TEST_SUITE_P(Fixture, SessionLifecycleCase, ::testing::ValuesIn(lifecycle_cases()),
                         case_test_name);

TEST(SessionLifecycleFixture, HoldsEveryCase) {
  EXPECT_EQ(lifecycle_cases().size(), 27U);
}
}  // namespace
}  // namespace tensorplate::serving
