// SPDX-License-Identifier: Apache-2.0

#include "fixture_job_bridge.hpp"

#include <algorithm>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <iterator>
#include <map>
#include <memory>
#include <mutex>
#include <optional>
#include <string>
#include <utility>
#include <variant>
#include <vector>

#include "tensorplate/backend/bounded_job_bridge.hpp"
#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/job_result.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate::testing {

namespace {

Error reason(Error::Code code, const char* text) {
  return Error::make(code, text, text);
}

// As the runner ends a cancelled job: the code alone, no reason.
Error cancelled() {
  return Error::make(Error::Code::Cancelled, "the job was cancelled");
}

}  // namespace

TranscriptResult fixture_transcript(const AudioFrames& audio) {
  const std::uint64_t end = audio.start_sample + audio.pcm.byte_size / 4;
  return TranscriptResult{"hello", {1}, {WordUnit{"hello", 0, 1, audio.start_sample, end}}};
}

Result<JobResult> fixture_job_result(const JobRequest& request, BufferManager& buffers) {
  if (const auto* audio = std::get_if<AudioFrames>(&request.payload())) {
    return JobResult::create(fixture_transcript(*audio));
  }
  const std::string& text = std::get<TextSegment>(request.payload()).text;
  const std::size_t bytes = kFixtureSpeechBytesPerTextByte * text.size();
  auto buffer = buffers.allocate(bytes);
  if (!buffer.has_value()) {
    return unexpected(buffer.error());
  }
  const auto pcm = buffers.data(buffer.value()).value();
  std::fill(pcm.begin(), pcm.end(), std::byte{0});
  return JobResult::create(
      AudioChunkResult{kJobOutputAudioFormat, PcmWindow{buffer.value(), 0, bytes}, 0});
}

FixtureJobBridge::FixtureJobBridge(BufferManager& buffers, ResultFunction result)
    : buffers_(buffers),
      result_(std::move(result)),
      rules_({JobClass::SttDecode, JobClass::TtsSynthesis}),
      thread_(&FixtureJobBridge::run, this) {}

FixtureJobBridge::~FixtureJobBridge() {
  unload();
  {
    const std::lock_guard<std::mutex> lock(mutex_);
    stopping_ = true;
  }
  wake_.notify_one();
  thread_.join();
}

// -- BoundedJobBridge. -----------------------------------------------------------

Result<void> FixtureJobBridge::set_event_sink(std::shared_ptr<JobEventSink> sink) {
  const std::lock_guard<std::mutex> lock(mutex_);
  auto set = rules_.set_event_sink(std::move(sink));
  sink_set_ = sink_set_ || set.has_value();
  return set;
}

Result<void> FixtureJobBridge::submit(JobRequest request) {
  const std::lock_guard<std::mutex> lock(mutex_);
  // A missing sink is refused first, by rules_.
  if (sink_set_ && phase_ != Phase::Ready) {
    return phase_ == Phase::Loaded
               ? unexpected(reason(Error::Code::NotReady, "backend_not_ready"))
               : unexpected(reason(Error::Code::Unavailable, "backend_unavailable"));
  }
  const std::uint64_t job_id = request.identity().job_id;
  if (auto taken = rules_.submit(request); !taken.has_value()) {
    return taken;
  }
  if (lane_.size() >= kWaitingJobs) {
    rules_.queue_failed(job_id, reason(Error::Code::ResourceExhausted, "job_capacity_exhausted"));
    rules_.queue_released(job_id);
  } else {
    rules_.queue_accepted(job_id);
    open_.emplace(job_id, OpenJob{std::move(request), false});
    if (running_ == 0) {
      running_ = job_id;
    } else {
      lane_.push_back(job_id);
    }
  }
  wake_locked();
  return {};
}

Result<void> FixtureJobBridge::cancel(std::uint64_t job_id) {
  const std::lock_guard<std::mutex> lock(mutex_);
  if (auto noted = rules_.cancel(job_id); !noted.has_value()) {
    return noted;
  }
  // Not open: the job has its terminal event, queued or delivered.
  if (const auto job = open_.find(job_id); job != open_.end() && !job->second.cancelled) {
    cancel_locked(job);
    wake_locked();
  }
  return {};
}

Result<void> FixtureJobBridge::release_session(std::uint64_t session_key,
                                               std::uint64_t generation) {
  const std::lock_guard<std::mutex> lock(mutex_);
  if (phase_ == Phase::Lost || phase_ == Phase::Unloaded) {
    return unexpected(reason(Error::Code::Unavailable, "backend_unavailable"));
  }
  if (auto noted = rules_.release_session(session_key, generation); !noted.has_value()) {
    return noted;
  }
  for (auto job = open_.begin(); job != open_.end();) {
    const auto next = std::next(job);
    if (job->second.request.identity().session_key == session_key && !job->second.cancelled) {
      cancel_locked(job);
    }
    job = next;
  }
  wake_locked();
  return {};
}

Result<void> FixtureJobBridge::health() {
  const std::lock_guard<std::mutex> lock(mutex_);
  if (phase_ == Phase::Loaded) {
    return unexpected(reason(Error::Code::NotReady, "backend_not_ready"));
  }
  if (phase_ != Phase::Ready) {
    return unexpected(reason(Error::Code::Unavailable, "backend_unavailable"));
  }
  return health_;
}

// -- The session's side. ---------------------------------------------------------

void FixtureJobBridge::prime() {
  const std::lock_guard<std::mutex> lock(mutex_);
  if (phase_ == Phase::Loaded) {
    phase_ = Phase::Ready;
  }
}

void FixtureJobBridge::lose_backend() {
  const std::lock_guard<std::mutex> lock(mutex_);
  phase_ = Phase::Lost;
  for (const auto& [job_id, job] : open_) {
    rules_.queue_failed(job_id, reason(Error::Code::Unavailable, "backend_reset"));
    unreleased_.push_back(job_id);
  }
  open_.clear();
  lane_.clear();
  running_ = 0;
  wake_locked();
}

void FixtureJobBridge::unload() {
  {
    const std::lock_guard<std::mutex> lock(mutex_);
    phase_ = Phase::Unloaded;
    for (const auto& [job_id, job] : open_) {
      rules_.queue_failed(job_id, reason(Error::Code::Unavailable, "session_unloaded"));
      rules_.queue_released(job_id);
    }
    for (const std::uint64_t job_id : unreleased_) {
      rules_.queue_released(job_id);
    }
    open_.clear();
    lane_.clear();
    unreleased_.clear();
    running_ = 0;
  }
  // On this thread, so that nothing is owed once the unload has returned.
  (void)rules_.deliver();
}

// -- The test's hold on the backend. ---------------------------------------------

void FixtureJobBridge::hold_jobs() {
  const std::lock_guard<std::mutex> lock(mutex_);
  held_ = true;
}

void FixtureJobBridge::release_jobs() {
  const std::lock_guard<std::mutex> lock(mutex_);
  held_ = false;
  wake_locked();
}

void FixtureJobBridge::fail_next_job(Error error) {
  const std::lock_guard<std::mutex> lock(mutex_);
  next_failure_ = std::move(error);
}

void FixtureJobBridge::set_health(Result<void> health) {
  const std::lock_guard<std::mutex> lock(mutex_);
  health_ = std::move(health);
}

bool FixtureJobBridge::settled(std::chrono::milliseconds patience) {
  std::unique_lock<std::mutex> lock(mutex_);
  return settled_.wait_for(lock, patience, [this] { return !pending_ && !delivering_; });
}

std::vector<Error> FixtureJobBridge::refusals() const {
  return rules_.refusals();
}

// -- The backend. ----------------------------------------------------------------

void FixtureJobBridge::run() {
  std::unique_lock<std::mutex> lock(mutex_);
  for (;;) {
    // Whoever made the job due set pending_, so its events are delivered next.
    if (running_ != 0 && !held_) {
      end_running_locked();
      continue;
    }
    if (pending_) {
      pending_ = false;
      delivering_ = true;
      lock.unlock();
      (void)rules_.deliver();
      lock.lock();
      delivering_ = false;
      settled_.notify_all();
      continue;
    }
    if (stopping_) {
      return;
    }
    wake_.wait(lock);
  }
}

void FixtureJobBridge::end_running_locked() {
  const auto job = open_.find(running_);
  if (job->second.cancelled) {
    rules_.queue_failed(running_, cancelled());
  } else if (next_failure_.has_value()) {
    rules_.queue_failed(running_, *std::exchange(next_failure_, std::nullopt));
  } else if (auto result = result_(job->second.request, buffers_); result.has_value()) {
    rules_.queue_completed(running_, std::move(result).value());
  } else {
    rules_.queue_failed(running_, result.error());
  }
  rules_.queue_released(running_);
  open_.erase(job);
  running_ = 0;
  if (!lane_.empty()) {
    running_ = lane_.front();
    lane_.pop_front();
  }
}

void FixtureJobBridge::cancel_locked(std::map<std::uint64_t, OpenJob>::iterator job) {
  const std::uint64_t job_id = job->first;
  job->second.cancelled = true;
  rules_.queue_cancel_acknowledged(job_id);
  if (job_id != running_) {
    lane_.erase(std::find(lane_.begin(), lane_.end(), job_id));
    rules_.queue_failed(job_id, cancelled());
    rules_.queue_released(job_id);
    open_.erase(job);
  }
}

void FixtureJobBridge::wake_locked() {
  pending_ = true;
  wake_.notify_one();
}

}  // namespace tensorplate::testing
