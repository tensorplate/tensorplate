// SPDX-License-Identifier: Apache-2.0
#include "serving/session/telephony_resampler.hpp"

#include <algorithm>
#include <limits>
#include <span>

namespace tensorplate::serving {
namespace {

// The literal table of candidate_audio.py's FIR_TAPS_Q15.
constexpr std::array<std::int16_t, TelephonyResampler::kTaps> kTapsQ15{
    0,    0,     -1,    1,    6,     -1,    -16,   -3,   31,    16,    -52,  -46,  74,
    100,  -88,   -186,  79,   307,   -24,   -462,  -105, 644,   348,   -837, -771, 1021,
    1518, -1174, -3112, 1275, 10305, 15074, 10305, 1275, -3112, -1174, 1518, 1021, -771,
    -837, 348,   644,   -105, -462,  -24,   307,   79,   -186,  -88,   100,  74,   -46,
    -52,  16,    31,    -3,   -16,   -1,    6,     1,    -1,    0,     0};

constexpr int kMulawBias = 0x84;

constexpr std::array<std::int16_t, 256> mulaw_table() {
  std::array<std::int16_t, 256> table{};
  int code = 0;
  for (std::int16_t& entry : table) {
    const int value = ~code++ & 0xFF;
    const int magnitude =
        ((((value & 0x0F) << 3) + kMulawBias) << ((value >> 4) & 0x07)) - kMulawBias;
    entry = static_cast<std::int16_t>((value & 0x80) != 0 ? -magnitude : magnitude);
  }
  return table;
}

constexpr std::array<std::int16_t, 256> kMulaw = mulaw_table();

// Half away from zero, as the reference rounds.
std::int64_t round_q15(std::int64_t accumulator) noexcept {
  if (accumulator >= 0) {
    return (accumulator + 16384) >> 15;
  }
  return -((-accumulator + 16384) >> 15);
}

}  // namespace

std::int16_t mulaw_to_linear(std::byte code) noexcept {
  return std::span{kMulaw}[std::to_integer<std::size_t>(code)];
}

const std::array<std::int16_t, TelephonyResampler::kTaps>& TelephonyResampler::taps() noexcept {
  return kTapsQ15;
}

void TelephonyResampler::push(std::span<const std::byte> mulaw, std::vector<std::int16_t>& out) {
  for (const std::byte code : mulaw) {
    take(mulaw_to_linear(code), out);
  }
}

void TelephonyResampler::flush(std::vector<std::int16_t>& out) {
  while (emitted_ < 2 * received_) {
    out.push_back(output(emitted_++));
  }
}

void TelephonyResampler::take(std::int16_t sample, std::vector<std::int16_t>& out) {
  std::span{history_}[received_ % kHistory] = sample;
  ++received_;
  while (emitted_ + kPending < 2 * received_) {
    out.push_back(output(emitted_++));
  }
}

std::int16_t TelephonyResampler::output(std::uint64_t n) const noexcept {
  // Output n reads the inputs i with |2i - n| <= kPending, through tap
  // n + kPending - 2i; none at or past the inputs received.
  const std::uint64_t first = n > kPending ? (n - kPending + 1) / 2 : 0;
  const std::uint64_t end = std::min((n + kPending) / 2 + 1, received_);
  const std::span taps{kTapsQ15};
  const std::span history{history_};
  std::int64_t accumulator = 0;
  for (std::uint64_t input = first; input < end; ++input) {
    accumulator += std::int64_t{taps[n + kPending - 2 * input]} * history[input % kHistory];
  }
  const std::int64_t rounded = round_q15(accumulator * 2);
  return static_cast<std::int16_t>(std::clamp<std::int64_t>(
      rounded, std::numeric_limits<std::int16_t>::min(), std::numeric_limits<std::int16_t>::max()));
}
}  // namespace tensorplate::serving
