// SPDX-License-Identifier: Apache-2.0
#include "serving/session/vad_policy.hpp"

#include <gtest/gtest.h>

#include <chrono>
#include <cstdint>
#include <limits>
#include <optional>
#include <ostream>
#include <string>
#include <vector>

namespace tensorplate::serving {

void PrintTo(const VadFrameOutcome& outcome, std::ostream* out) {
  const auto print = [out](const char* name, const std::optional<std::uint64_t>& value) {
    *out << name << '=' << (value ? std::to_string(*value) : "-") << ' ';
  };
  print("split", outcome.duration_split);
  print("onset", outcome.onset);
  print("endpoint", outcome.silence_endpoint);
}

namespace {
using namespace std::chrono_literals;

constexpr float kSpeech = 0.9F;
constexpr float kSilence = 0.1F;
constexpr std::uint64_t kFrame = 512;
constexpr std::uint64_t kMaxUtterance = 480'000;
constexpr std::uint64_t kMax = std::numeric_limits<std::uint64_t>::max();

VadPolicy make_policy(const VadProfile& profile = {}) {
  auto policy = VadPolicy::create(profile);
  EXPECT_TRUE(policy) << policy.error().message;
  return *std::move(policy);
}

/// Feeds `frames` frames of one probability; returns the outcomes that decided something.
std::vector<VadFrameOutcome> feed(VadPolicy& policy, float probability, std::uint64_t frames) {
  std::vector<VadFrameOutcome> decided;
  for (std::uint64_t i = 0; i < frames; ++i) {
    const auto outcome = policy.observe(probability);
    EXPECT_TRUE(outcome);
    if (outcome && *outcome != VadFrameOutcome{}) {
      decided.push_back(*outcome);
    }
  }
  return decided;
}

VadFrameOutcome onset(std::uint64_t start) {
  VadFrameOutcome outcome;
  outcome.onset = start;
  return outcome;
}

VadFrameOutcome endpoint(std::uint64_t end) {
  VadFrameOutcome outcome;
  outcome.silence_endpoint = end;
  return outcome;
}

VadFrameOutcome split(std::uint64_t at) {
  VadFrameOutcome outcome;
  outcome.duration_split = at;
  return outcome;
}

using Outcomes = std::vector<VadFrameOutcome>;

TEST(VadPolicy, DefaultsAreTheInitialAlgorithmProfile) {
  const VadProfile profile;
  EXPECT_EQ(profile.threshold, 0.5F);
  EXPECT_EQ(profile.min_speech, 64ms);
  EXPECT_EQ(profile.silence_endpoint, 320ms);
  EXPECT_EQ(profile.leading_padding, 160ms);
  EXPECT_EQ(profile.initial_accumulation, 320ms);
  EXPECT_EQ(profile.decode_stride, 160ms);
  EXPECT_EQ(profile.endpoint_retention, 1000ms);
  EXPECT_EQ(profile.max_utterance, 30000ms);
  EXPECT_EQ(profile.frame_samples, 512U);
  EXPECT_EQ(make_policy().profile(), profile);
}

TEST(VadPolicy, OnsetNeedsTwoConsecutiveSpeechFrames) {
  auto policy = make_policy();
  EXPECT_TRUE(feed(policy, kSilence, 10).empty());
  for (int blip = 0; blip < 3; ++blip) {
    EXPECT_TRUE(feed(policy, kSpeech, 1).empty());
    EXPECT_TRUE(feed(policy, kSilence, 1).empty());
  }
  EXPECT_FALSE(policy.utterance_start());
  // The run starts at frame 16; the utterance 160 ms (2,560 samples) before it.
  EXPECT_TRUE(feed(policy, kSpeech, 1).empty());
  EXPECT_EQ(feed(policy, kSpeech, 1), Outcomes{onset(16 * kFrame - 2'560)});
  EXPECT_EQ(policy.utterance_start(), 16 * kFrame - 2'560);
  EXPECT_EQ(policy.next_frame_start(), 18 * kFrame);
}

TEST(VadPolicy, ThresholdIsSpeechAndJustBelowItIsNot) {
  auto policy = make_policy();
  EXPECT_EQ(feed(policy, 0.5F, 2), Outcomes{onset(0)});
  EXPECT_TRUE(feed(policy, 0.49999F, 9).empty());
  EXPECT_EQ(feed(policy, 0.49999F, 1), Outcomes{endpoint(12 * kFrame)});
}

TEST(VadPolicy, EndpointAfterTenSilenceFramesAndASpeechFrameRestartsIt) {
  auto policy = make_policy();
  EXPECT_EQ(feed(policy, kSpeech, 2), Outcomes{onset(0)});
  EXPECT_TRUE(feed(policy, kSilence, 9).empty());
  EXPECT_TRUE(feed(policy, kSpeech, 1).empty());
  EXPECT_TRUE(feed(policy, kSilence, 9).empty());
  EXPECT_EQ(policy.utterance_start(), 0U);
  // The endpoint is the end of the detecting silence run, 320 ms after speech.
  EXPECT_EQ(feed(policy, kSilence, 1), Outcomes{endpoint(22 * kFrame)});
  EXPECT_FALSE(policy.utterance_start());
  // Speech straight after the endpoint starts a new run.
  EXPECT_TRUE(feed(policy, kSpeech, 1).empty());
  EXPECT_EQ(feed(policy, kSpeech, 1), Outcomes{onset(22 * kFrame)});
}

TEST(VadPolicy, OnsetPaddingClampsAtThePreviousEndpointAndOffsetsStayAbsolute) {
  auto policy = make_policy();
  EXPECT_EQ(feed(policy, kSpeech, 2), Outcomes{onset(0)});
  EXPECT_EQ(feed(policy, kSilence, 10), Outcomes{endpoint(12 * kFrame)});
  // Speech two frames after the endpoint: its padding would reach before it.
  EXPECT_TRUE(feed(policy, kSilence, 2).empty());
  EXPECT_EQ(feed(policy, kSpeech, 2), Outcomes{onset(12 * kFrame)});
  EXPECT_EQ(feed(policy, kSilence, 10), Outcomes{endpoint(26 * kFrame)});
  // Speech seven frames after it: the whole padding fits.
  EXPECT_TRUE(feed(policy, kSilence, 7).empty());
  EXPECT_EQ(feed(policy, kSpeech, 2), Outcomes{onset(33 * kFrame - 2'560)});
}

TEST(VadPolicy, SplitsAtExactlyMaxUtteranceAndStaysInSpeech) {
  auto policy = make_policy();
  EXPECT_EQ(feed(policy, kSpeech, 2), Outcomes{onset(0)});
  // Frame 937 holds sample 480,000 at its 256th sample.
  EXPECT_TRUE(feed(policy, kSpeech, 935).empty());
  EXPECT_EQ(feed(policy, kSpeech, 1), Outcomes{split(kMaxUtterance)});
  EXPECT_EQ(policy.next_frame_start(), 938 * kFrame);
  EXPECT_EQ(policy.utterance_start(), kMaxUtterance);
  // The next limit, 960,000, is a frame end: the frame reaching it splits.
  EXPECT_TRUE(feed(policy, kSpeech, 936).empty());
  EXPECT_EQ(policy.next_frame_start(), 2 * kMaxUtterance - kFrame);
  EXPECT_EQ(feed(policy, kSpeech, 1), Outcomes{split(2 * kMaxUtterance)});
  EXPECT_EQ(policy.utterance_start(), 2 * kMaxUtterance);
}

TEST(VadPolicy, SilenceEndingExactlyAtTheLimitIsAnEndpointNotASplit) {
  auto policy = make_policy();
  EXPECT_EQ(feed(policy, kSpeech, 2), Outcomes{onset(0)});
  EXPECT_EQ(feed(policy, kSpeech, 936), Outcomes{split(kMaxUtterance)});
  EXPECT_TRUE(feed(policy, kSpeech, 927).empty());
  EXPECT_EQ(feed(policy, kSilence, 10), Outcomes{endpoint(2 * kMaxUtterance)});
  EXPECT_FALSE(policy.utterance_start());
}

TEST(VadPolicy, SilenceCountCarriesAcrossASplit) {
  auto policy = make_policy();
  EXPECT_EQ(feed(policy, kSpeech, 2), Outcomes{onset(0)});
  EXPECT_TRUE(feed(policy, kSpeech, 930).empty());
  // Five silence frames before the split's frame, the sixth holds the split.
  EXPECT_TRUE(feed(policy, kSilence, 5).empty());
  EXPECT_EQ(feed(policy, kSilence, 1), Outcomes{split(kMaxUtterance)});
  EXPECT_TRUE(feed(policy, kSilence, 3).empty());
  EXPECT_EQ(feed(policy, kSilence, 1), Outcomes{endpoint(942 * kFrame)});
}

TEST(VadPolicy, SplitInTheFrameEndingTheSilenceRunEndsBothUtterances) {
  auto policy = make_policy();
  EXPECT_EQ(feed(policy, kSpeech, 2), Outcomes{onset(0)});
  EXPECT_TRUE(feed(policy, kSpeech, 926).empty());
  EXPECT_TRUE(feed(policy, kSilence, 9).empty());
  VadFrameOutcome both = split(kMaxUtterance);
  both.silence_endpoint = 938 * kFrame;
  EXPECT_EQ(feed(policy, kSilence, 1), Outcomes{both});
  EXPECT_FALSE(policy.utterance_start());
}

TEST(VadPolicy, EarliestUtteranceStartIsWhereTheNextOnsetCouldStart) {
  auto policy = make_policy();
  EXPECT_EQ(policy.earliest_utterance_start(), 0U);
  EXPECT_TRUE(feed(policy, kSilence, 10).empty());
  EXPECT_EQ(policy.earliest_utterance_start(), 10 * kFrame - 2'560);
  // A speech run under way holds it at the run's padding.
  EXPECT_TRUE(feed(policy, kSpeech, 1).empty());
  EXPECT_EQ(policy.earliest_utterance_start(), 10 * kFrame - 2'560);
  EXPECT_EQ(feed(policy, kSpeech, 6), Outcomes{onset(10 * kFrame - 2'560)});
  EXPECT_EQ(policy.earliest_utterance_start(), 10 * kFrame - 2'560);
  EXPECT_EQ(feed(policy, kSilence, 10), Outcomes{endpoint(27 * kFrame)});
  // Clamped at the endpoint until the padding fits after it.
  EXPECT_EQ(policy.earliest_utterance_start(), 27 * kFrame);
  EXPECT_TRUE(feed(policy, kSilence, 2).empty());
  EXPECT_EQ(policy.earliest_utterance_start(), 27 * kFrame);
  EXPECT_TRUE(feed(policy, kSilence, 4).empty());
  EXPECT_EQ(policy.earliest_utterance_start(), 33 * kFrame - 2'560);
  ASSERT_TRUE(policy.reset(33 * kFrame + 100));
  EXPECT_EQ(policy.earliest_utterance_start(), 33 * kFrame + 100);
}

TEST(VadPolicy, ResetEndsTheUtteranceWhereTheDispatchEndedIt) {
  auto policy = make_policy();
  EXPECT_EQ(feed(policy, kSpeech, 2), Outcomes{onset(0)});
  const std::uint64_t end = 20 * kFrame + 100;
  ASSERT_TRUE(policy.reset(end));
  EXPECT_FALSE(policy.utterance_start());
  EXPECT_EQ(policy.next_frame_start(), end);
  // Frames follow from the end, and the next onset's padding stops there.
  EXPECT_EQ(feed(policy, kSpeech, 2), Outcomes{onset(end)});
  EXPECT_EQ(feed(policy, kSilence, 10), Outcomes{endpoint(end + 12 * kFrame)});
}

TEST(VadPolicy, ResetDropsAnUnfinishedSpeechRun) {
  auto policy = make_policy();
  EXPECT_TRUE(feed(policy, kSpeech, 1).empty());
  ASSERT_TRUE(policy.reset(policy.next_frame_start()));
  EXPECT_TRUE(feed(policy, kSpeech, 1).empty());
  EXPECT_EQ(feed(policy, kSpeech, 1), Outcomes{onset(kFrame)});
}

TEST(VadPolicy, ResetBeforeObservedAudioIsRefusedAndChangesNothing) {
  auto policy = make_policy();
  EXPECT_EQ(feed(policy, kSpeech, 3), Outcomes{onset(0)});
  const auto refused = policy.reset(3 * kFrame - 1);
  ASSERT_FALSE(refused);
  EXPECT_EQ(refused.error().code, Error::Code::Internal);
  EXPECT_EQ(refused.error().context, "vad_reset_before_observed_audio");
  EXPECT_EQ(policy.next_frame_start(), 3 * kFrame);
  EXPECT_EQ(policy.utterance_start(), 0U);
  EXPECT_TRUE(policy.reset(3 * kFrame));
}

TEST(VadPolicy, RefusesAProbabilityOutsideZeroToOne) {
  auto policy = make_policy();
  for (const float probability : {std::numeric_limits<float>::quiet_NaN(),
                                  std::numeric_limits<float>::infinity(), -0.01F, 1.01F}) {
    const auto refused = policy.observe(probability);
    ASSERT_FALSE(refused) << probability;
    EXPECT_EQ(refused.error().code, Error::Code::Internal);
    EXPECT_EQ(refused.error().context, "vad_probability_out_of_range");
    EXPECT_EQ(policy.next_frame_start(), 0U);
  }
  EXPECT_TRUE(feed(policy, 0.0F, 1).empty());
  EXPECT_EQ(feed(policy, 1.0F, 2), Outcomes{onset(0)});
}

TEST(VadPolicy, RefusesAFramePastTheLargestOffset) {
  auto policy = make_policy();
  ASSERT_TRUE(policy.reset(kMax - kFrame));
  EXPECT_TRUE(feed(policy, kSilence, 1).empty());
  const auto refused = policy.observe(kSilence);
  ASSERT_FALSE(refused);
  EXPECT_EQ(refused.error().context, "sample_offset_overflow");
  EXPECT_EQ(policy.next_frame_start(), kMax);
}

TEST(VadPolicy, DurationsRoundUpToWholeFrames) {
  VadProfile profile;
  profile.min_speech = 1ms;
  profile.silence_endpoint = 33ms;
  auto policy = make_policy(profile);
  EXPECT_EQ(feed(policy, kSpeech, 1), Outcomes{onset(0)});
  EXPECT_TRUE(feed(policy, kSilence, 1).empty());
  EXPECT_EQ(feed(policy, kSilence, 1), Outcomes{endpoint(3 * kFrame)});
}

TEST(VadPolicy, AcceptsProfilesAtTheirBounds) {
  const auto accepted = [](auto change) {
    VadProfile profile;
    change(profile);
    return static_cast<bool>(VadPolicy::create(profile));
  };
  EXPECT_TRUE(accepted([](VadProfile& p) { p.threshold = 1.0F; }));
  EXPECT_TRUE(accepted([](VadProfile& p) { p.frame_samples = 16'384; }));
  EXPECT_TRUE(accepted([](VadProfile& p) { p.frame_samples = 1; }));
  EXPECT_TRUE(accepted([](VadProfile& p) { p.endpoint_retention = 30000ms; }));
  EXPECT_TRUE(accepted([](VadProfile& p) {
    p.max_utterance = 225ms;
    p.min_speech = 50ms;
    p.silence_endpoint = p.initial_accumulation = p.decode_stride = p.endpoint_retention = 160ms;
  }));
}

TEST(VadPolicy, RefusesInvalidProfiles) {
  const auto refused = [](auto change) {
    VadProfile profile;
    change(profile);
    const auto policy = VadPolicy::create(profile);
    return !policy && policy.error().code == Error::Code::ConfigInvalid &&
           policy.error().context == "invalid_vad_profile";
  };
  EXPECT_TRUE(
      refused([](VadProfile& p) { p.threshold = std::numeric_limits<float>::quiet_NaN(); }));
  EXPECT_TRUE(refused([](VadProfile& p) { p.threshold = 0.0F; }));
  EXPECT_TRUE(refused([](VadProfile& p) { p.threshold = 1.0001F; }));
  EXPECT_TRUE(refused([](VadProfile& p) { p.frame_samples = 0; }));
  EXPECT_TRUE(refused([](VadProfile& p) { p.frame_samples = 16'385; }));
  EXPECT_TRUE(refused([](VadProfile& p) { p.max_utterance = 0ms; }));
  EXPECT_TRUE(refused([](VadProfile& p) { p.max_utterance = 30001ms; }));
  // In samples this wraps to exactly 30 seconds.
  EXPECT_TRUE(refused([](VadProfile& p) {
    p.max_utterance = std::chrono::milliseconds{(std::int64_t{1} << 60) + 30000};
  }));
  EXPECT_TRUE(refused([](VadProfile& p) { p.min_speech = 0ms; }));
  EXPECT_TRUE(refused([](VadProfile& p) { p.silence_endpoint = -1ms; }));
  EXPECT_TRUE(refused([](VadProfile& p) { p.leading_padding = 0ms; }));
  EXPECT_TRUE(refused([](VadProfile& p) { p.initial_accumulation = 0ms; }));
  EXPECT_TRUE(refused([](VadProfile& p) { p.decode_stride = 0ms; }));
  EXPECT_TRUE(refused([](VadProfile& p) { p.endpoint_retention = 0ms; }));
  EXPECT_TRUE(refused([](VadProfile& p) { p.endpoint_retention = 30001ms; }));
  // 160 ms of padding and 50 ms of speech, rounded to two frames, fill 224 ms.
  EXPECT_TRUE(refused([](VadProfile& p) {
    p.max_utterance = 224ms;
    p.min_speech = 50ms;
    p.silence_endpoint = p.initial_accumulation = p.decode_stride = p.endpoint_retention = 160ms;
  }));
}

TEST(VadPolicy, ConvertsAnInternalEndToInputSamplesRoundingUp) {
  EXPECT_EQ(to_input_samples(16'001, 16'000), 16'001U);
  // One second at 8 kHz ends at 8,000, and an odd internal end rounds up.
  EXPECT_EQ(to_input_samples(16'000, 8'000), 8'000U);
  EXPECT_EQ(to_input_samples(16'001, 8'000), 8'001U);
  EXPECT_EQ(to_input_samples(kMax / 16'000, 16'000), kMax / 16'000);
  const auto overflow = to_input_samples(kMax / 16'000 + 1, 16'000);
  ASSERT_FALSE(overflow);
  EXPECT_EQ(overflow.error().context, "sample_offset_overflow");
  const auto rate = to_input_samples(16'000, 44'100);
  ASSERT_FALSE(rate);
  EXPECT_EQ(rate.error().code, Error::Code::Internal);
  EXPECT_EQ(rate.error().context, "unsupported_input_rate");
}

TEST(VadPolicy, ConvertsAnInternalIntervalToMicrosecondsOutward) {
  EXPECT_EQ(to_microseconds(0, 16'000), (MicrosecondInterval{0, 1'000'000}));
  EXPECT_EQ(to_microseconds(1, 1), (MicrosecondInterval{62, 63}));
  EXPECT_EQ(to_microseconds(3, 5), (MicrosecondInterval{187, 313}));
  const std::uint64_t last = kMax / 1'000'000;
  EXPECT_EQ(to_microseconds(last, last),
            (MicrosecondInterval{last * 125 / 2, (last * 125 + 1) / 2}));
  const auto overflow = to_microseconds(0, last + 1);
  ASSERT_FALSE(overflow);
  EXPECT_EQ(overflow.error().context, "sample_offset_overflow");
  const auto reversed = to_microseconds(5, 4);
  ASSERT_FALSE(reversed);
  EXPECT_EQ(reversed.error().context, "interval_out_of_order");
}
}  // namespace
}  // namespace tensorplate::serving
