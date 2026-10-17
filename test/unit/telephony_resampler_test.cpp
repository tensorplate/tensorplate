// SPDX-License-Identifier: Apache-2.0
// The cases replay what the pinned Python reference gives
// (fixtures/telephony_resampler/).
#include "serving/session/telephony_resampler.hpp"

#include <gtest/gtest.h>

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <fstream>
#include <nlohmann/json.hpp>
#include <random>
#include <span>
#include <string>
#include <vector>

namespace tensorplate::serving {
namespace {

nlohmann::json load_record() {
  std::ifstream in(std::string{TP_SOURCE_DIR} +
                   "/test/unit/fixtures/telephony_resampler/tp_fir63_blackman_q15_v1.json");
  EXPECT_TRUE(in.is_open());
  return nlohmann::json::parse(in);
}

const nlohmann::json& record() {
  static const nlohmann::json loaded = load_record();
  return loaded;
}

std::vector<std::byte> bytes_of(const std::string& hex) {
  std::vector<std::byte> bytes;
  for (std::size_t at = 0; at + 1 < hex.size(); at += 2) {
    bytes.push_back(static_cast<std::byte>(std::stoi(hex.substr(at, 2), nullptr, 16)));
  }
  return bytes;
}

std::vector<std::int16_t> samples_of(const std::string& hex) {
  const auto bytes = bytes_of(hex);
  std::vector<std::int16_t> samples;
  for (std::size_t at = 0; at + 1 < bytes.size(); at += 2) {
    samples.push_back(
        static_cast<std::int16_t>(std::to_integer<std::uint16_t>(bytes[at]) |
                                  (std::to_integer<std::uint16_t>(bytes[at + 1]) << 8U)));
  }
  return samples;
}

const nlohmann::json& case_named(const std::string& name) {
  for (const auto& entry : record().at("cases")) {
    if (entry.at("name") == name) {
      return entry;
    }
  }
  ADD_FAILURE() << "no recorded case " << name;
  static const nlohmann::json kNone = nlohmann::json::object();
  return kNone;
}

TEST(TelephonyResampler, DecodesMulawAsTheReference) {
  const auto table = record().at("mulaw_to_linear").get<std::vector<int>>();
  ASSERT_EQ(table.size(), 256U);
  for (std::size_t code = 0; code < table.size(); ++code) {
    EXPECT_EQ(mulaw_to_linear(static_cast<std::byte>(code)), table[code]) << "byte " << code;
  }
}

TEST(TelephonyResampler, IsThePinnedFilter) {
  EXPECT_EQ(record().at("resampler_id").get<std::string>(), TelephonyResampler::kId);
  const auto taps = record().at("taps_q15").get<std::vector<int>>();
  EXPECT_TRUE(std::equal(taps.begin(), taps.end(), TelephonyResampler::taps().begin(),
                         TelephonyResampler::taps().end()));
}

TEST(TelephonyResampler, AWholeInputFlushedIsTheReferenceResample) {
  for (const char* name : {"noise", "tones", "short"}) {
    const auto& recorded = case_named(name);
    const auto input = bytes_of(recorded.at("mulaw_hex").get<std::string>());
    TelephonyResampler resampler;
    std::vector<std::int16_t> out;
    resampler.push(input, out);
    resampler.flush(out);
    EXPECT_EQ(out, samples_of(recorded.at("pcm_s16le_hex").get<std::string>())) << name;
    EXPECT_EQ(resampler.output_samples(), 2 * input.size()) << name;
  }
  // The noise case rounds a tie of each sign and passes each 16-bit limit.
  const auto& noise = case_named("noise");
  EXPECT_GT(noise.at("rounding_ties").at("positive").get<int>(), 0);
  EXPECT_GT(noise.at("rounding_ties").at("negative").get<int>(), 0);
  EXPECT_GT(noise.at("clamped_outputs").at("above").get<int>(), 0);
  EXPECT_GT(noise.at("clamped_outputs").at("below").get<int>(), 0);
}

TEST(TelephonyResampler, HowTheInputIsSplitChangesNothing) {
  for (const char* name : {"noise", "tones"}) {
    const auto& recorded = case_named(name);
    const auto input = bytes_of(recorded.at("mulaw_hex").get<std::string>());
    std::mt19937 split(7);
    TelephonyResampler resampler;
    std::vector<std::int16_t> out;
    for (std::size_t at = 0; at < input.size();) {
      const std::size_t size = std::min<std::size_t>(input.size() - at, 1 + split() % 400);
      resampler.push(std::span{input}.subspan(at, size), out);
      at += size;
      ASSERT_EQ(out.size(),
                2 * at > TelephonyResampler::kPending ? 2 * at - TelephonyResampler::kPending : 0U);
    }
    resampler.flush(out);
    EXPECT_EQ(out, samples_of(recorded.at("pcm_s16le_hex").get<std::string>())) << name;
  }
}

TEST(TelephonyResampler, ThirtyOneOutputsWaitForTheNextSixteenInputs) {
  const auto input = bytes_of(case_named("tones").at("mulaw_hex").get<std::string>());
  TelephonyResampler resampler;
  std::vector<std::int16_t> out;
  resampler.push(std::span{input}.first(15), out);
  EXPECT_TRUE(out.empty());
  resampler.push(std::span{input}.subspan(15, 1), out);
  EXPECT_EQ(out.size(), 1U);
  EXPECT_EQ(resampler.input_samples(), 16U);
  resampler.flush(out);
  EXPECT_EQ(out.size(), 32U);
  resampler.flush(out);
  EXPECT_EQ(out.size(), 32U);
}

TEST(TelephonyResampler, AFlushEndsTheInputThereAndLaterInputKeepsItsHistory) {
  const auto& prefix = record().at("flushed_prefix");
  const auto& recorded = case_named(prefix.at("case").get<std::string>());
  const auto input = bytes_of(recorded.at("mulaw_hex").get<std::string>());
  const auto whole = samples_of(recorded.at("pcm_s16le_hex").get<std::string>());
  const auto at = prefix.at("inputs").get<std::size_t>();
  TelephonyResampler resampler;
  std::vector<std::int16_t> out;
  resampler.push(std::span{input}.first(at), out);
  resampler.flush(out);
  EXPECT_EQ(out, samples_of(prefix.at("pcm_s16le_hex").get<std::string>()));
  out.clear();
  // The flushed outputs are not emitted again while the next 16 inputs arrive.
  resampler.push(std::span{input}.subspan(at, 15), out);
  EXPECT_TRUE(out.empty());
  EXPECT_EQ(resampler.output_samples(), 2 * at);
  resampler.push(std::span{input}.subspan(at + 15), out);
  resampler.flush(out);
  EXPECT_TRUE(std::equal(out.begin(), out.end(),
                         whole.begin() + static_cast<std::ptrdiff_t>(2 * at), whole.end()));
}
}  // namespace
}  // namespace tensorplate::serving
