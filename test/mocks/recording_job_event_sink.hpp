// SPDX-License-Identifier: Apache-2.0
//
// JobEventSink that records every delivery and checks it independently of
// the bridge that delivered it: the callback matches the event kind, each
// job's events pass the sink's own JobEventSequence, callbacks never overlap,
// and a session release arrives only after every job of that session was
// released, and at most once per session. A test tells the sink what it submits and cancels before
// calling the bridge, exactly as a serving-layer receiver that checks events again records it,
// because the bridge may answer on another thread before the call returns.

#pragma once

#include <atomic>
#include <cstdint>
#include <functional>
#include <mutex>
#include <optional>
#include <string>
#include <unordered_map>
#include <utility>
#include <vector>

#include "tensorplate/backend/bounded_job_bridge.hpp"
#include "tensorplate/core/job_event.hpp"
#include "tensorplate/core/job_request.hpp"

namespace tensorplate::testing {

class RecordingJobEventSink final : public JobEventSink {
 public:
  /// Registers a job before it is submitted.
  void expect_job(const JobRequest& request) {
    std::lock_guard<std::mutex> guard(mu_);
    jobs_.insert_or_assign(request.identity().job_id,
                           Job{request.identity(), JobEventSequence(request)});
  }

  /// Notes a cancellation before BoundedJobBridge::cancel(job_id) is called.
  void expect_cancel(std::uint64_t job_id) {
    std::lock_guard<std::mutex> guard(mu_);
    if (const auto it = jobs_.find(job_id); it != jobs_.end()) {
      it->second.sequence.note_cancel_requested();
    }
  }

  /// Notes a session release before BoundedJobBridge::release_session() is
  /// called for `session_key`.
  void expect_session_release(std::uint64_t session_key) {
    std::lock_guard<std::mutex> guard(mu_);
    for (auto& [job_id, job] : jobs_) {
      if (job.identity.session_key == session_key && !job.sequence.terminal()) {
        job.sequence.note_cancel_requested();
      }
    }
  }

  /// Runs `hook` inside every job callback, after the event is recorded and
  /// without the sink's lock held; used to call back into the bridge.
  void set_callback_hook(std::function<void(const JobEvent&)> hook) {
    std::lock_guard<std::mutex> guard(mu_);
    hook_ = std::move(hook);
  }

  /// Runs `hook` inside on_session_released, after the release is recorded
  /// and without the sink's lock held.
  void set_session_release_hook(std::function<void(std::uint64_t, std::uint64_t)> hook) {
    std::lock_guard<std::mutex> guard(mu_);
    session_release_hook_ = std::move(hook);
  }

  void on_accepted(const JobEvent& event) noexcept override {
    record(JobEventKind::Accepted, event);
  }
  void on_progress(const JobEvent& event) noexcept override {
    record(JobEventKind::Progress, event);
  }
  void on_completed(const JobEvent& event) noexcept override {
    record(JobEventKind::Completed, event);
  }
  void on_failed(const JobEvent& event) noexcept override { record(JobEventKind::Failed, event); }
  void on_cancel_acknowledged(const JobEvent& event) noexcept override {
    record(JobEventKind::CancelAcknowledged, event);
  }
  void on_released(const JobEvent& event) noexcept override {
    record(JobEventKind::Released, event);
  }

  void on_session_released(std::uint64_t session_key, std::uint64_t generation) noexcept override {
    const OverlapGuard overlap(*this);
    std::function<void(std::uint64_t, std::uint64_t)> hook;
    {
      std::lock_guard<std::mutex> guard(mu_);
      for (const auto& [job_id, job] : jobs_) {
        if (job.identity.session_key == session_key && !job.sequence.released()) {
          violations_.push_back("session_released_before_job " + std::to_string(job_id));
        }
      }
      for (const auto& [released_key, released_generation] : session_releases_) {
        if (released_key == session_key) {
          violations_.push_back("duplicate_session_released " + std::to_string(session_key));
          break;
        }
      }
      session_releases_.emplace_back(session_key, generation);
      hook = session_release_hook_;
    }
    if (hook) {
      hook(session_key, generation);
    }
  }

  [[nodiscard]] std::vector<JobEvent> events() const {
    std::lock_guard<std::mutex> guard(mu_);
    return events_;
  }

  [[nodiscard]] std::vector<JobEventKind> kinds_for(std::uint64_t job_id) const {
    std::lock_guard<std::mutex> guard(mu_);
    std::vector<JobEventKind> kinds;
    for (const JobEvent& event : events_) {
      if (event.identity().job_id == job_id) {
        kinds.push_back(event.kind());
      }
    }
    return kinds;
  }

  /// The sink's checker for a job; std::nullopt for a job never expected.
  [[nodiscard]] std::optional<JobEventSequence> sequence(std::uint64_t job_id) const {
    std::lock_guard<std::mutex> guard(mu_);
    const auto it = jobs_.find(job_id);
    return it == jobs_.end() ? std::nullopt : std::optional<JobEventSequence>(it->second.sequence);
  }

  [[nodiscard]] std::vector<std::pair<std::uint64_t, std::uint64_t>> session_releases() const {
    std::lock_guard<std::mutex> guard(mu_);
    return session_releases_;
  }

  /// Every check that failed, as "<reason> <job id>". Empty when all passed.
  [[nodiscard]] std::vector<std::string> violations() const {
    std::lock_guard<std::mutex> guard(mu_);
    return violations_;
  }

 private:
  struct Job {
    JobIdentity identity;
    JobEventSequence sequence;
  };

  // Flags a callback that starts while another is still running.
  class OverlapGuard {
   public:
    explicit OverlapGuard(RecordingJobEventSink& sink) : sink_(sink) {
      if (sink_.in_callback_.fetch_add(1) != 0) {
        std::lock_guard<std::mutex> guard(sink_.mu_);
        sink_.violations_.emplace_back("overlapping_callbacks");
      }
    }
    ~OverlapGuard() { sink_.in_callback_.fetch_sub(1); }
    OverlapGuard(const OverlapGuard&) = delete;
    OverlapGuard& operator=(const OverlapGuard&) = delete;
    OverlapGuard(OverlapGuard&&) = delete;
    OverlapGuard& operator=(OverlapGuard&&) = delete;

   private:
    RecordingJobEventSink& sink_;
  };

  void record(JobEventKind callback, const JobEvent& event) {
    const OverlapGuard overlap(*this);
    std::function<void(const JobEvent&)> hook;
    {
      std::lock_guard<std::mutex> guard(mu_);
      const std::string job = std::to_string(event.identity().job_id);
      events_.push_back(event);
      if (event.kind() != callback) {
        violations_.push_back("callback_kind_mismatch " + job);
      }
      const auto it = jobs_.find(event.identity().job_id);
      if (it == jobs_.end()) {
        violations_.push_back("unexpected_job " + job);
      } else if (auto observed = it->second.sequence.observe(event); !observed) {
        violations_.push_back(observed.error().context.value_or("") + " " + job);
      }
      hook = hook_;
    }
    if (hook) {
      hook(event);
    }
  }

  mutable std::mutex mu_;
  std::atomic<int> in_callback_{0};
  std::unordered_map<std::uint64_t, Job> jobs_;
  std::vector<JobEvent> events_;
  std::vector<std::pair<std::uint64_t, std::uint64_t>> session_releases_;
  std::vector<std::string> violations_;
  std::function<void(const JobEvent&)> hook_;
  std::function<void(std::uint64_t, std::uint64_t)> session_release_hook_;
};

}  // namespace tensorplate::testing
