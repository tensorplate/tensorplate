// SPDX-License-Identifier: Apache-2.0
#pragma once

// The speech dispatch over the fixture job bridge, as the dispatch contract's
// cases and its own know it.

#include <gtest/gtest.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <condition_variable>
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

/// Passes every event to the sink behind it, but holds back the
/// acknowledgements of session releases while told to, as a runner's
/// separate frame can come after the session's last job has ended. What it
/// held it delivers on the thread that lets it go.
class HeldAcknowledgements final : public JobEventSink {
 public:
  explicit HeldAcknowledgements(std::shared_ptr<JobEventSink> inner) : inner_(std::move(inner)) {}

  void on_accepted(const JobEvent& event) noexcept override { inner_->on_accepted(event); }
  void on_progress(const JobEvent& event) noexcept override { inner_->on_progress(event); }
  void on_completed(const JobEvent& event) noexcept override { inner_->on_completed(event); }
  void on_failed(const JobEvent& event) noexcept override { inner_->on_failed(event); }
  void on_cancel_acknowledged(const JobEvent& event) noexcept override {
    inner_->on_cancel_acknowledged(event);
  }
  void on_released(const JobEvent& event) noexcept override { inner_->on_released(event); }
  void on_session_released(std::uint64_t session_key, std::uint64_t generation) noexcept override {
    {
      const std::lock_guard guard(mu_);
      if (holding_) {
        held_.emplace_back(session_key, generation);
        return;
      }
    }
    inner_->on_session_released(session_key, generation);
  }

  void hold() {
    const std::lock_guard guard(mu_);
    holding_ = true;
  }
  void let_go() {
    std::vector<std::pair<std::uint64_t, std::uint64_t>> held;
    {
      const std::lock_guard guard(mu_);
      holding_ = false;
      held.swap(held_);
    }
    for (const auto& [session_key, generation] : held) {
      inner_->on_session_released(session_key, generation);
    }
  }

 private:
  const std::shared_ptr<JobEventSink> inner_;
  std::mutex mu_;
  bool holding_ = false;
  std::vector<std::pair<std::uint64_t, std::uint64_t>> held_;
};

/// Passes every call to the fixture bridge behind it and counts what the
/// dispatch asked. It can answer one session release as the adapter does when
/// its release record is full, and a job class as a backend that does not run
/// it. It can hold back the backend's acknowledgements of session releases,
/// and hold the caller of the next release_session() until the test lets it
/// go.
///
/// Told to (deliver_callbacks_first()), its submit() and release_session()
/// return only once the fixture has settled: every callback the call caused
/// has run by then, on the fixture's thread, as far as a held job allows. A
/// bridge may deliver in that order, and it is the one a caller that records
/// a job after submitting it gets wrong. It is not the default: a job nobody
/// holds has then ended before its submit returns, so no cancel and no
/// release would find one running.
class CountingJobBridge final : public BoundedJobBridge {
 public:
  explicit CountingJobBridge(std::shared_ptr<FixtureJobBridge> inner) : inner_(std::move(inner)) {}

  [[nodiscard]] Result<void> set_event_sink(std::shared_ptr<JobEventSink> sink) override {
    if (sink == nullptr) {
      return inner_->set_event_sink(nullptr);
    }
    auto held = std::make_shared<HeldAcknowledgements>(std::move(sink));
    auto set = inner_->set_event_sink(held);
    if (set) {
      const std::lock_guard guard(mu_);
      acknowledgements_ = std::move(held);
    }
    return set;
  }
  [[nodiscard]] Result<void> submit(JobRequest request) override {
    {
      const std::lock_guard guard(mu_);
      submitted_.push_back(request);
      if (refused_class_ == request.job_class()) {
        return unexpected(Error::make(Error::Code::Unsupported, "job_class_unsupported",
                                      "job_class_unsupported"));
      }
    }
    auto submitted = inner_->submit(std::move(request));
    settle_if_told();
    return submitted;
  }
  [[nodiscard]] Result<void> cancel(std::uint64_t job_id) override {
    {
      const std::lock_guard guard(mu_);
      cancelled_.push_back(job_id);
    }
    return inner_->cancel(job_id);
  }
  [[nodiscard]] Result<void> release_session(std::uint64_t session_key,
                                             std::uint64_t generation) override {
    {
      std::unique_lock lock(mu_);
      ++releases_[session_key];
      if (hold_next_release_) {
        hold_next_release_ = false;
        release_held_ = true;
        gate_.notify_all();
        gate_.wait(lock, [&] { return !release_held_; });
      }
      if (refuse_next_release_) {
        refuse_next_release_ = false;
        return unexpected(Error::make(Error::Code::ResourceExhausted,
                                      "session_release_capacity_exhausted",
                                      "session_release_capacity_exhausted"));
      }
    }
    auto released = inner_->release_session(session_key, generation);
    settle_if_told();
    return released;
  }
  [[nodiscard]] Result<void> health() override {
    ++health_calls_;
    return inner_->health();
  }

  void deliver_callbacks_first() { callbacks_first_.store(true); }
  /// The next release_session() waits, before it reaches the fixture, until
  /// let_release_go().
  void hold_next_release() {
    const std::lock_guard guard(mu_);
    hold_next_release_ = true;
  }
  /// True once a release_session() waits to be let go.
  [[nodiscard]] bool release_held(std::chrono::milliseconds patience) {
    std::unique_lock lock(mu_);
    return gate_.wait_for(lock, patience, [&] { return release_held_; });
  }
  /// Also takes back a hold no release_session() has met yet.
  void let_release_go() {
    {
      const std::lock_guard guard(mu_);
      hold_next_release_ = false;
      release_held_ = false;
    }
    gate_.notify_all();
  }
  void hold_acknowledgements() { acknowledgements().hold(); }
  void let_acknowledgements_go() { acknowledgements().let_go(); }
  void refuse_next_release() {
    const std::lock_guard guard(mu_);
    refuse_next_release_ = true;
  }
  void refuse_class(JobClass job_class) {
    const std::lock_guard guard(mu_);
    refused_class_ = job_class;
  }
  /// How often release_session() was called for `session_key`.
  [[nodiscard]] std::uint64_t releases(std::uint64_t session_key) const {
    const std::lock_guard guard(mu_);
    const auto found = releases_.find(session_key);
    return found == releases_.end() ? 0 : found->second;
  }
  /// Every request submit() was called with, in order, the refused too.
  [[nodiscard]] std::vector<JobRequest> submitted() const {
    const std::lock_guard guard(mu_);
    return submitted_;
  }
  /// Every job id cancel() was called with, in order.
  [[nodiscard]] std::vector<std::uint64_t> cancelled() const {
    const std::lock_guard guard(mu_);
    return cancelled_;
  }
  [[nodiscard]] std::uint64_t health_calls() const { return health_calls_.load(); }

 private:
  void settle_if_told() {
    if (callbacks_first_.load()) {
      (void)inner_->settled(ReferenceBinding::kPatience);
    }
  }

  HeldAcknowledgements& acknowledgements() {
    const std::lock_guard guard(mu_);
    return *acknowledgements_;
  }

  const std::shared_ptr<FixtureJobBridge> inner_;
  mutable std::mutex mu_;
  std::condition_variable gate_;
  bool hold_next_release_ = false;
  bool release_held_ = false;
  std::shared_ptr<HeldAcknowledgements> acknowledgements_;
  std::map<std::uint64_t, std::uint64_t> releases_;
  std::vector<JobRequest> submitted_;
  std::vector<std::uint64_t> cancelled_;
  bool refuse_next_release_ = false;
  std::optional<JobClass> refused_class_;
  std::atomic<std::uint64_t> health_calls_{0};
  std::atomic<bool> callbacks_first_{false};
};

class SpeechUnderTest final : public DispatchUnderTest {
 public:
  static constexpr std::uint64_t kGeneration = 9;

  /// `dispatch` is given this generation, and its stalls are recorded.
  explicit SpeechUnderTest(const SchedulerClock& clock, FixtureJobBridgeOptions options = {},
                           FixtureJobBridge::ResultFunction result = result_following_input,
                           BufferManagerConfig pool = {},
                           serving::SpeechDispatchOptions dispatch = {})
      : buffers(BufferManager::create(std::move(pool)).value()),
        fixture(std::make_shared<FixtureJobBridge>(*buffers, std::move(result), options)),
        bridge(std::make_shared<CountingJobBridge>(fixture)),
        strategy(serving::SpeechJobDispatch::create(bridge, *buffers, clock,
                                                    recording_stalls(std::move(dispatch)))
                     .value()) {
    fixture->prime();
  }
  /// With the dispatch stopped and the backend unloaded no job reads a buffer
  /// and no result is on its way: every case ends with the pool read.
  ~SpeechUnderTest() override {
    strategy->stop();
    fixture->unload();
    EXPECT_EQ(buffers_in_use(), 0U);
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

  /// Buffers nobody has released. A release the pool refused is a second
  /// release of one buffer, so every reading also holds that there was none
  /// but those the case's own backend caused (`stale_results`).
  [[nodiscard]] std::size_t buffers_in_use() const {
    const auto pool = buffers->accounting();
    EXPECT_EQ(pool.release_failures, stale_results) << "a buffer was released twice";
    return pool.active_count;
  }

  /// What the dispatch reported through `on_stall`, in order.
  [[nodiscard]] std::vector<Error> stalls() const {
    const std::lock_guard guard(stalls_mu_);
    return stalls_;
  }
  /// Waits until the dispatch has reported a stall.
  [[nodiscard]] bool stalled(std::chrono::milliseconds patience) {
    std::unique_lock lock(stalls_mu_);
    return stalls_cv_.wait_for(lock, patience, [&] { return !stalls_.empty(); });
  }

  // Ahead of `strategy`, whose thread reports stalls.
 private:
  mutable std::mutex stalls_mu_;
  std::condition_variable stalls_cv_;
  std::vector<Error> stalls_;

 public:
  /// Results the case's backend gave in a buffer it had released itself: the
  /// dispatch's one release of each is refused.
  std::uint64_t stale_results = 0;
  std::unique_ptr<BufferManager> buffers;
  std::shared_ptr<FixtureJobBridge> fixture;
  std::shared_ptr<CountingJobBridge> bridge;
  std::unique_ptr<serving::SpeechJobDispatch> strategy;

 private:
  serving::SpeechDispatchOptions recording_stalls(serving::SpeechDispatchOptions dispatch) {
    dispatch.generation = kGeneration;
    dispatch.on_stall = [this](const Error& cause) {
      {
        const std::lock_guard guard(stalls_mu_);
        stalls_.push_back(cause);
      }
      stalls_cv_.notify_all();
    };
    return dispatch;
  }
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
/// The callbacks a submit or a session release causes run before it returns.
[[nodiscard]] inline std::unique_ptr<DispatchUnderTest> speech_dispatch_with_callbacks_first(
    const SchedulerClock& clock) {
  auto speech = std::make_unique<SpeechUnderTest>(clock);
  speech->bridge->deliver_callbacks_first();
  return speech;
}
/// The fixture backend's own results: `hello`, and silence for speech.
[[nodiscard]] inline std::unique_ptr<DispatchUnderTest> speech_dispatch_with_fixture_results(
    const SchedulerClock& clock) {
  return std::make_unique<SpeechUnderTest>(clock, FixtureJobBridgeOptions{}, fixture_job_result);
}
}  // namespace tensorplate::testing
