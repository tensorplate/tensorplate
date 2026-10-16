// SPDX-License-Identifier: Apache-2.0
#include "serving/session/window_scheduler.hpp"

#include <chrono>

#include "tensorplate/core/job_request.hpp"

namespace tensorplate::serving {
namespace {
constexpr std::uint64_t kSamplesPerMs = kJobInputSampleRateHz / 1000;

std::uint64_t samples(std::chrono::milliseconds duration) {
  return static_cast<std::uint64_t>(duration.count()) * kSamplesPerMs;
}
}  // namespace

WindowScheduler::WindowScheduler(const VadProfile& profile) noexcept
    : accumulation_(samples(profile.initial_accumulation)),
      stride_(samples(profile.decode_stride)) {}

void WindowScheduler::open(std::uint64_t first, std::uint64_t speech_start) noexcept {
  open_ = true;
  first_ = first;
  speech_start_ = speech_start;
  last_end_.reset();
}

void WindowScheduler::close() noexcept {
  open_ = false;
}

std::optional<SampleWindow> WindowScheduler::due(std::uint64_t scored_end) const noexcept {
  if (!open_ || scored_end < speech_start_ + accumulation_ ||
      (last_end_ && scored_end < *last_end_ + stride_)) {
    return std::nullopt;
  }
  return SampleWindow{first_, scored_end};
}

void WindowScheduler::started(std::uint64_t end) noexcept {
  last_end_ = end;
}
}  // namespace tensorplate::serving
