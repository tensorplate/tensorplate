// SPDX-License-Identifier: Apache-2.0
#include "serving/session/window_scheduler.hpp"

#include <gtest/gtest.h>

#include <chrono>
#include <cstdint>
#include <optional>
#include <ostream>

namespace tensorplate::serving {

void PrintTo(const SampleWindow& window, std::ostream* out) {
  *out << '[' << window.first << ", " << window.end << ')';
}

namespace {
using namespace std::chrono_literals;

// The default profile's 320 ms and 160 ms at 16 kHz.
constexpr std::uint64_t kAccumulation = 5'120;
constexpr std::uint64_t kStride = 2'560;

TEST(WindowScheduler, NothingIsDueBeforeAnUtteranceOpens) {
  const WindowScheduler windows{VadProfile{}};
  EXPECT_FALSE(windows.due(0));
  EXPECT_FALSE(windows.due(480'000));
}

TEST(WindowScheduler, TheFirstWindowIsDueTheAccumulationPastTheFirstSpeechSample) {
  WindowScheduler windows{VadProfile{}};
  // An onset with 160 ms of leading padding before its speech.
  windows.open(10'000, 12'560);
  EXPECT_FALSE(windows.due(10'000 + kAccumulation));
  EXPECT_FALSE(windows.due(12'560 + kAccumulation - 1));
  EXPECT_EQ(windows.due(12'560 + kAccumulation), (SampleWindow{10'000, 12'560 + kAccumulation}));
}

TEST(WindowScheduler, EachLaterWindowIsDueAStridePastTheLastStarted) {
  WindowScheduler windows{VadProfile{}};
  windows.open(0, 0);
  windows.started(kAccumulation);
  EXPECT_FALSE(windows.due(kAccumulation));
  EXPECT_FALSE(windows.due(kAccumulation + kStride - 1));
  EXPECT_EQ(windows.due(kAccumulation + kStride), (SampleWindow{0, kAccumulation + kStride}));
}

TEST(WindowScheduler, StridesThatPassedDuringADecodeGiveOneWindowOverTheLatestAudio) {
  WindowScheduler windows{VadProfile{}};
  windows.open(512, 512);
  windows.started(512 + kAccumulation);
  const std::uint64_t latest = 512 + kAccumulation + 3 * kStride + 100;
  EXPECT_EQ(windows.due(latest), (SampleWindow{512, latest}));
  // The next stride counts from that window's end.
  windows.started(latest);
  EXPECT_FALSE(windows.due(latest + kStride - 1));
  EXPECT_EQ(windows.due(latest + kStride), (SampleWindow{512, latest + kStride}));
}

TEST(WindowScheduler, ClosingEndsTheUtteranceAndTheNextStartsAfresh) {
  // A stride longer than the accumulation: the next utterance's first window
  // comes before the closed one's stride would have passed.
  VadProfile profile;
  profile.initial_accumulation = 100ms;
  profile.decode_stride = 500ms;
  WindowScheduler windows{profile};
  windows.open(0, 0);
  windows.started(20'000);
  windows.close();
  EXPECT_FALSE(windows.due(28'000));
  // A split opens the next utterance at its first sample, still in speech.
  windows.open(20'000, 20'000);
  EXPECT_FALSE(windows.due(21'599));
  EXPECT_EQ(windows.due(21'600), (SampleWindow{20'000, 21'600}));
}

constexpr std::uint64_t kFrame = 512;

TEST(WindowScheduler, ADueWindowWaitsInASilenceRunUntilSpeechIsScoredSinceAsked) {
  WindowScheduler windows{VadProfile{}};
  windows.open(0, 0);
  EXPECT_FALSE(windows.waits(kAccumulation, kAccumulation));
  // From the run's first frame on.
  EXPECT_TRUE(windows.waits(kAccumulation + kFrame, kAccumulation));
  EXPECT_TRUE(windows.waits(kAccumulation + 2 * kFrame, kAccumulation));
  // Speech scored since it was last asked, though silence followed.
  EXPECT_FALSE(windows.waits(kAccumulation + 4 * kFrame, kAccumulation + 3 * kFrame));
}

TEST(WindowScheduler, SpeechCountsOnlyUntilTheNextAsk) {
  WindowScheduler windows{VadProfile{}};
  windows.open(0, 0);
  ASSERT_TRUE(windows.waits(kAccumulation + kFrame, kAccumulation));
  ASSERT_FALSE(windows.waits(kAccumulation + 3 * kFrame, kAccumulation + 2 * kFrame));
  // Asked again at the same scored end: the same answer.
  EXPECT_FALSE(windows.waits(kAccumulation + 3 * kFrame, kAccumulation + 2 * kFrame));
  EXPECT_TRUE(windows.waits(kAccumulation + 5 * kFrame, kAccumulation + 2 * kFrame));
}

TEST(WindowScheduler, AStartedWindowAsksAfresh) {
  WindowScheduler windows{VadProfile{}};
  windows.open(0, 0);
  ASSERT_TRUE(windows.waits(kAccumulation + kFrame, kAccumulation));
  windows.started(kAccumulation + kFrame);
  // Its speech came after the last ask, which was for the window started.
  const std::uint64_t next = kAccumulation + kFrame + kStride;
  EXPECT_TRUE(windows.waits(next, next - kFrame));
}

TEST(WindowScheduler, TheNextUtteranceAsksAfresh) {
  WindowScheduler windows{VadProfile{}};
  windows.open(0, 0);
  ASSERT_TRUE(windows.waits(kAccumulation + kFrame, kAccumulation));
  windows.close();
  const std::uint64_t first = kAccumulation + 20 * kFrame;
  windows.open(first, first);
  EXPECT_TRUE(windows.waits(first + kAccumulation + kFrame, first + kAccumulation));
}
}  // namespace
}  // namespace tensorplate::serving
