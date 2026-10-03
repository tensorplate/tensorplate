// SPDX-License-Identifier: Apache-2.0
#include <gtest/gtest.h>

#include <chrono>
#include <cstdint>
#include <string>

#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/job_result.hpp"

#include "serving/session/credits.hpp"
#include "serving/session/tombstones.hpp"
#include "session_budget_fixture.hpp"

namespace tensorplate::serving {
namespace {
using testing::load_session_budget_fixture;
using testing::outcome_of;
using testing::session_budgets_from;

TEST(SessionBudgets, ConstantsMatchFixture) {
  const auto limits = load_session_budget_fixture().at("limits");
  const auto ms = [&limits](const char* name) {
    return std::chrono::milliseconds{limits.at(name).get<std::int64_t>()};
  };
  EXPECT_EQ(SessionBudgets::kInputAudioWindow, ms("input_audio_ms"));
  EXPECT_EQ(SessionBudgets::kWaitingTextSegments, limits.at("input_text_segments"));
  EXPECT_EQ(SessionBudgets::kWaitingTextBytes, limits.at("input_text_bytes"));
  EXPECT_EQ(SessionBudgets::kActiveTextSegments, limits.at("active_text_segments"));
  EXPECT_EQ(SessionBudgets::kOutputPcmWindow, ms("output_pcm_ms"));
  EXPECT_EQ(SessionBudgets::kOutputMetadataBytes, limits.at("output_metadata_bytes"));
  EXPECT_EQ(SessionBudgets::kOutputControlReserveBytes, limits.at("output_control_reserve_bytes"));
  EXPECT_EQ(SessionBudgets::kOutputNoProgressTimeout, ms("output_no_progress_ms"));
  EXPECT_EQ(SessionTombstones::kCapacity, limits.at("tombstone_capacity"));
  EXPECT_EQ(SessionTombstones::kLifetime, ms("tombstone_lifetime_ms"));
  EXPECT_EQ(SessionTombstones::kMaxReasonBytes, limits.at("tombstone_reason_bytes"));
}

TEST(SessionBudgets, DerivationsAndRejectionsMatchFixture) {
  const auto fixture = load_session_budget_fixture();
  const auto& rows = fixture.at("budgets");
  ASSERT_EQ(rows.size(), 5U);
  for (const auto& row : rows) {
    SCOPED_TRACE(row.dump());
    const auto budgets = session_budgets_from(row);
    ASSERT_TRUE(budgets);
    EXPECT_EQ(budgets->input_kind() == SessionInputKind::Audio, row.at("input") == "audio");
    EXPECT_EQ(budgets->input_credit_bytes(), row.at("input_credit_bytes"));
    EXPECT_EQ(budgets->waiting_items(), row.at("waiting_items"));
    EXPECT_EQ(budgets->active_items(), row.at("active_items"));
    EXPECT_EQ(budgets->output_pcm_bytes(), row.at("output_pcm_bytes"));
    EXPECT_EQ(budgets->output_metadata_bytes(), row.at("output_metadata_bytes"));
    EXPECT_EQ(budgets->output_control_reserve_bytes(),
              fixture.at("limits").at("output_control_reserve_bytes"));
  }
  const auto& invalid = fixture.at("invalid_budgets");
  ASSERT_EQ(invalid.size(), 4U);
  for (const auto& row : invalid) {
    SCOPED_TRACE(row.dump());
    const auto budgets = session_budgets_from(row);
    ASSERT_FALSE(budgets);
    EXPECT_EQ(budgets.error().code, Error::Code::ConfigInvalid);
    EXPECT_EQ(budgets.error().context, "invalid_session_budgets");
  }
}

TEST(SessionBudgets, LargestRatesFillTheJobPayloadCeilings) {
  EXPECT_EQ(SessionBudgets::for_audio_input(kMaxAudioFramesBytes)->input_credit_bytes(),
            kMaxAudioFramesBytes);
  EXPECT_EQ(SessionBudgets::for_text_input(kMaxAudioChunkBytes / 2)->output_pcm_bytes(),
            kMaxAudioChunkBytes);
  EXPECT_EQ(SessionBudgets::for_text_input(2 * kJobOutputSampleRateHz)->output_pcm_bytes(),
            2U * 2U * kJobOutputSampleRateHz);
}

Result<void> run_credit_step(InputCredit& credit, const nlohmann::json& step) {
  const auto op = step.at("op").get<std::string>();
  if (op == "charge") {
    return credit.charge(step.at("bytes").get<std::uint64_t>());
  }
  if (op == "release_audio") {
    return credit.release_audio(step.at("chunks").get<std::uint32_t>(),
                                step.at("bytes").get<std::uint64_t>());
  }
  if (op == "start_segment") {
    return credit.start_segment();
  }
  if (op == "finish_segment") {
    return credit.finish_segment();
  }
  return unexpected(Error::make(Error::Code::Internal, "unknown fixture op", "unknown_op"));
}

Error::Code expected_code(const std::string& outcome) {
  if (outcome == "input_credit_exceeded") {
    return Error::Code::ResourceExhausted;
  }
  return outcome == "empty_input" ? Error::Code::ConfigInvalid : Error::Code::Internal;
}

TEST(InputCredit, CasesMatchFixture) {
  const auto fixture = load_session_budget_fixture();
  const auto& cases = fixture.at("credit_cases");
  ASSERT_EQ(cases.size(), 9U);
  std::size_t steps_run = 0;
  for (const auto& test_case : cases) {
    SCOPED_TRACE(test_case.at("name").get<std::string>());
    const auto budgets = session_budgets_from(test_case.at("budget"));
    ASSERT_TRUE(budgets);
    InputCredit credit{*budgets};
    EXPECT_EQ(credit.depth(), 0U);
    EXPECT_EQ(credit.usage().limit(), budgets->input_credit_bytes());
    for (const auto& step : test_case.at("steps")) {
      SCOPED_TRACE(step.dump());
      const auto result = run_credit_step(credit, step);
      const auto expected = step.at("expect").get<std::string>();
      EXPECT_EQ(outcome_of(result), expected);
      if (!result) {
        EXPECT_EQ(result.error().code, expected_code(expected));
      }
      EXPECT_EQ(credit.depth(), step.at("depth"));
      EXPECT_EQ(credit.usage().used(), step.at("used"));
      EXPECT_EQ(credit.usage().limit(), budgets->input_credit_bytes());
      ++steps_run;
    }
  }
  EXPECT_EQ(steps_run, 50U);
}
}  // namespace
}  // namespace tensorplate::serving
