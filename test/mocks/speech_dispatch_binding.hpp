// SPDX-License-Identifier: Apache-2.0
#pragma once

// The speech dispatch over the fixture job bridge, as the dispatch contract's
// cases and its own know it.

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <map>
#include <memory>
#include <mutex>
#include <optional>
#include <span>
#include <string>
#include <string_view>
#include <utility>
#include <variant>
#include <vector>

#include "tensorplate/backend/bounded_job_bridge.hpp"
#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/job_result.hpp"

#include "fixture_job_bridge.hpp"
#include "serving/session/speech_dispatch.hpp"
#include "session_dispatch_binding.hpp"
#include "synthetic_session_dispatch.hpp"

namespace tensorplate::testing {

/// The PCM a synthesis of `text` yields under result_following_input().
[[nodiscard]] inline std::vector<std::byte> speech_following(std::string_view text) {
  std::vector<std::byte> pcm;
  const std::uint64_t samples = text.size() * SyntheticSessionDispatch::kSamplesPerTextByte;
  pcm.reserve(samples * 2);
  for (std::uint64_t index = 0; index < samples; ++index) {
    const auto sample =
        static_cast<std::uint16_t>(SyntheticSessionDispatch::sample_of(text, index));
    pcm.push_back(static_cast<std::byte>(sample & 0xffU));
    pcm.push_back(static_cast<std::byte>(sample >> 8U));
  }
  return pcm;
}

/// A fixture backend whose results follow their input, so that the wrong
/// bytes of the right size are caught: a decode's text is the digest of its
/// window, a synthesis's PCM follows its text.
[[nodiscard]] inline Result<JobResult> result_following_input(const JobRequest& request,
                                                              BufferManager& buffers) {
  if (const auto* audio = std::get_if<AudioFrames>(&request.payload())) {
    const auto held = buffers.data(audio->pcm.buffer);
    if (!held) {
      return unexpected(held.error());
    }
    return JobResult::create(TranscriptResult{SyntheticSessionDispatch::transcript_of(held->subspan(
                                                  audio->pcm.byte_offset, audio->pcm.byte_size)),
                                              {},
                                              {}});
  }
  const auto speech = speech_following(std::get<TextSegment>(request.payload()).text);
  const auto buffer = buffers.allocate(speech.size());
  if (!buffer) {
    return unexpected(buffer.error());
  }
  const auto pcm = buffers.data(*buffer);
  if (!pcm) {
    return unexpected(pcm.error());
  }
  std::copy(speech.begin(), speech.end(), pcm->begin());
  return JobResult::create(
      AudioChunkResult{kJobOutputAudioFormat, PcmWindow{*buffer, 0, speech.size()}, 0});
}

/// Passes every call to the bridge behind it and counts what the dispatch
/// asked. It can answer one session release as the adapter does when its
/// release record is full.
class CountingJobBridge final : public BoundedJobBridge {
 public:
  explicit CountingJobBridge(std::shared_ptr<BoundedJobBridge> inner) : inner_(std::move(inner)) {}

  [[nodiscard]] Result<void> set_event_sink(std::shared_ptr<JobEventSink> sink) override {
    return inner_->set_event_sink(std::move(sink));
  }
  [[nodiscard]] Result<void> submit(JobRequest request) override {
    {
      const std::lock_guard guard(mu_);
      submitted_.push_back(request);
    }
    return inner_->submit(std::move(request));
  }
  [[nodiscard]] Result<void> cancel(std::uint64_t job_id) override {
    return inner_->cancel(job_id);
  }
  [[nodiscard]] Result<void> release_session(std::uint64_t session_key,
                                             std::uint64_t generation) override {
    {
      const std::lock_guard guard(mu_);
      ++releases_[session_key];
      if (refuse_next_release_) {
        refuse_next_release_ = false;
        return unexpected(Error::make(Error::Code::ResourceExhausted,
                                      "session_release_capacity_exhausted",
                                      "session_release_capacity_exhausted"));
      }
    }
    return inner_->release_session(session_key, generation);
  }
  [[nodiscard]] Result<void> health() override {
    ++health_calls_;
    return inner_->health();
  }

  void refuse_next_release() {
    const std::lock_guard guard(mu_);
    refuse_next_release_ = true;
  }
  /// How often release_session() was called for `session_key`.
  [[nodiscard]] std::uint64_t releases(std::uint64_t session_key) const {
    const std::lock_guard guard(mu_);
    const auto found = releases_.find(session_key);
    return found == releases_.end() ? 0 : found->second;
  }
  /// Every request submit() was called with, in order.
  [[nodiscard]] std::vector<JobRequest> submitted() const {
    const std::lock_guard guard(mu_);
    return submitted_;
  }
  [[nodiscard]] std::uint64_t health_calls() const { return health_calls_.load(); }

 private:
  const std::shared_ptr<BoundedJobBridge> inner_;
  mutable std::mutex mu_;
  std::map<std::uint64_t, std::uint64_t> releases_;
  std::vector<JobRequest> submitted_;
  bool refuse_next_release_ = false;
  std::atomic<std::uint64_t> health_calls_{0};
};

class SpeechUnderTest final : public DispatchUnderTest {
 public:
  static constexpr std::uint64_t kGeneration = 9;

  explicit SpeechUnderTest(const SchedulerClock& clock, FixtureJobBridgeOptions options = {},
                           FixtureJobBridge::ResultFunction result = result_following_input)
      : buffers(BufferManager::create({}).value()),
        fixture(std::make_shared<FixtureJobBridge>(*buffers, std::move(result), options)),
        bridge(std::make_shared<CountingJobBridge>(fixture)),
        strategy(
            serving::SpeechJobDispatch::create(bridge, *buffers, clock, {kGeneration}).value()) {
    fixture->prime();
  }

  serving::SessionDispatch& dispatch() override { return *strategy; }
  serving::SessionRequest audio_request() const override {
    return {serving::SessionInputKind::Audio,
            "en",
            "",
            {serving::StreamAudioEncoding::PcmS16Le, 16'000, 1}};
  }
  serving::SessionRequest text_request() const override {
    return {serving::SessionInputKind::Text, "en-US", "voice-a", {}};
  }
  std::vector<serving::TranscriptSegment> transcript(
      std::uint64_t first_sample, std::span<const std::byte> audio) const override {
    if (audio.empty()) {
      return {};
    }
    // 62.5 us a sample, the start rounded down and the end up.
    const std::uint64_t end_sample = first_sample + audio.size() / 2;
    return {{first_sample * 125 / 2, (end_sample * 125 + 1) / 2,
             SyntheticSessionDispatch::transcript_of(audio)}};
  }
  std::vector<std::byte> speech(std::string_view text) const override {
    return speech_following(text);
  }
  /// The dispatch idle, then the bridge settled, then the dispatch still
  /// idle having taken no step meanwhile: neither has work the other gave it.
  bool settled(std::chrono::milliseconds patience) override {
    const auto deadline = std::chrono::steady_clock::now() + patience;
    for (;;) {
      const auto left = std::chrono::duration_cast<std::chrono::milliseconds>(
          deadline - std::chrono::steady_clock::now());
      if (left <= std::chrono::milliseconds{0} || !strategy->wait_idle(left)) {
        return false;
      }
      const std::uint64_t steps = strategy->stats().steps;
      if (!fixture->settled(left)) {
        return false;
      }
      if (strategy->wait_idle(std::chrono::milliseconds{0}) && strategy->stats().steps == steps) {
        return true;
      }
    }
  }
  std::size_t held_sessions() const override { return strategy->stats().sessions; }
  void hold_jobs() override { fixture->hold_jobs(); }
  void release_jobs() override { fixture->release_jobs(); }

  /// Buffers nobody has released.
  [[nodiscard]] std::size_t buffers_in_use() const { return buffers->accounting().active_count; }

  std::unique_ptr<BufferManager> buffers;
  std::shared_ptr<FixtureJobBridge> fixture;
  std::shared_ptr<CountingJobBridge> bridge;
  std::unique_ptr<serving::SpeechJobDispatch> strategy;
};

/// The speech dispatch of `binding`, with its fixture backend.
[[nodiscard]] inline SpeechUnderTest& speech_of(ReferenceBinding& binding) {
  return static_cast<SpeechUnderTest&>(*binding.under_test);
}

[[nodiscard]] inline std::unique_ptr<DispatchUnderTest> speech_dispatch(
    const SchedulerClock& clock) {
  return std::make_unique<SpeechUnderTest>(clock);
}
/// The backend refuses a job at capacity from submit() itself.
[[nodiscard]] inline std::unique_ptr<DispatchUnderTest> speech_dispatch_refusing_at_submit(
    const SchedulerClock& clock) {
  return std::make_unique<SpeechUnderTest>(clock, FixtureJobBridgeOptions{true, false});
}
/// The backend's cancellations come too late: a cancelled job completes.
[[nodiscard]] inline std::unique_ptr<DispatchUnderTest> speech_dispatch_with_late_cancels(
    const SchedulerClock& clock) {
  return std::make_unique<SpeechUnderTest>(clock, FixtureJobBridgeOptions{false, true});
}
/// The fixture backend's own results: `hello`, and silence for speech.
[[nodiscard]] inline std::unique_ptr<DispatchUnderTest> speech_dispatch_with_fixture_results(
    const SchedulerClock& clock) {
  return std::make_unique<SpeechUnderTest>(clock, FixtureJobBridgeOptions{}, fixture_job_result);
}
}  // namespace tensorplate::testing
