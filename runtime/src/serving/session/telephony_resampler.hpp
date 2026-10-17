// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <array>
#include <cstddef>
#include <cstdint>
#include <span>
#include <string_view>
#include <vector>

namespace tensorplate::serving {

/// The linear sample of a G.711 mu-law byte, as the Sun g711.c reference and
/// CPython's audioop decode it.
[[nodiscard]] std::int16_t mulaw_to_linear(std::byte code) noexcept;

/// Resamples 8 kHz input onto the 16 kHz internal timeline with the pinned
/// filter kId: zero-stuffed by two, then a 63-tap Q15 low-pass at 3,680 Hz
/// with gain two whose group delay is compensated, so output sample n is
/// source time n / 16,000 s. Over a whole input followed by flush() it is
/// bit-exact with interpolate_2x in tools/validation/candidate_audio.py.
/// Pure: no clock, no thread.
class TelephonyResampler {
 public:
  static constexpr std::string_view kId = "tp-fir63-blackman-q15-v1";
  static constexpr std::size_t kTaps = 63;
  /// Output samples that wait for later input: the filter's look-ahead.
  static constexpr std::uint64_t kPending = (kTaps - 1) / 2;

  /// The filter, in Q15; the taps sum to exactly 32,768.
  [[nodiscard]] static const std::array<std::int16_t, kTaps>& taps() noexcept;

  /// Takes the next mu-law input bytes and appends every output sample whose
  /// inputs have all arrived and that no flush emitted: after N inputs,
  /// 2N - kPending in all, or 2F for the next 15 inputs after a flush at F.
  void push(std::span<const std::byte> mulaw, std::vector<std::int16_t>& out);
  /// Appends the pending samples as if the input ended here, so N inputs
  /// have given exactly 2N outputs. Later input goes on from the same
  /// history; only the samples flushed differ from a resample of the whole.
  void flush(std::vector<std::int16_t>& out);

  [[nodiscard]] std::uint64_t input_samples() const noexcept { return received_; }
  [[nodiscard]] std::uint64_t output_samples() const noexcept { return emitted_; }

 private:
  void take(std::int16_t sample, std::vector<std::int16_t>& out);
  [[nodiscard]] std::int16_t output(std::uint64_t n) const noexcept;

  /// The inputs one output reads: 32 at most.
  static constexpr std::size_t kHistory = 32;
  std::array<std::int16_t, kHistory> history_{};
  std::uint64_t received_ = 0;
  std::uint64_t emitted_ = 0;
};
}  // namespace tensorplate::serving
