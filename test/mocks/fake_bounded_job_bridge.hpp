// SPDX-License-Identifier: Apache-2.0
//
// Scriptable in-process BoundedJobBridge.
//
// FakeBoundedJobBridge keeps the promises the bridge contract makes to its
// caller while its session lives (it does not model the session's
// destruction): the documented submit/cancel/release_session refusals, one
// JobEventSequence per job fed with every successful cancellation, no event
// delivered that the sequence refuses (the job then ends with `failed`
// carrying the refusal, and a refused `released` still follows it), a session
// release delivered only after that session's jobs were released and at most
// once however often it is requested, serialized callbacks, and no callback
// from inside a bridge method. The test plays the
// backend: it queues the events each job produces and decides when they
// arrive by calling deliver(), which runs callbacks on the calling thread
// without holding the fake's lock, so a callback may call back into the
// bridge. Concurrent deliver() calls take turns, so callbacks never overlap;
// a callback must not call deliver() itself.
//
// run_bounded_job() plays one bounded job: accepted, completed, released,
// and no progress.

#pragma once

#include <cstddef>
#include <cstdint>
#include <deque>
#include <memory>
#include <mutex>
#include <optional>
#include <string>
#include <unordered_map>
#include <unordered_set>
#include <utility>
#include <vector>

#include "tensorplate/backend/bounded_job_bridge.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/job_event.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/job_result.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate::testing {

class FakeBoundedJobBridge final : public BoundedJobBridge {
 public:
  /// A bridge that runs `served` job classes and admits at most
  /// `max_unreleased_jobs` jobs that were not yet released.
  explicit FakeBoundedJobBridge(std::vector<JobClass> served = {JobClass::SttDecode,
                                                                JobClass::TtsSynthesis,
                                                                JobClass::VadFrames},
                                std::size_t max_unreleased_jobs = 64)
      : served_(std::move(served)), max_unreleased_jobs_(max_unreleased_jobs) {}

  // -- BoundedJobBridge. ------------------------------------------------------

  Result<void> set_event_sink(std::shared_ptr<JobEventSink> sink) override {
    std::lock_guard<std::mutex> guard(mu_);
    if (!sink) {
      return refuse(Error::Code::ConfigInvalid, "event_sink_null");
    }
    if (sink_) {
      return refuse(Error::Code::ConfigInvalid, "event_sink_already_set");
    }
    sink_ = std::move(sink);
    return {};
  }

  Result<void> submit(JobRequest request) override {
    std::lock_guard<std::mutex> guard(mu_);
    if (!sink_) {
      return refuse(Error::Code::NotReady, "event_sink_missing");
    }
    if (next_submit_error_) {
      Error error = std::move(*next_submit_error_);
      next_submit_error_.reset();
      return unexpected(std::move(error));
    }
    bool served = false;
    for (const JobClass job_class : served_) {
      served = served || job_class == request.job_class();
    }
    if (!served) {
      return refuse(Error::Code::Unsupported, "job_class_unsupported");
    }
    const JobIdentity identity = request.identity();
    if (releasing_.count(identity.session_key) != 0 ||
        acknowledged_.count(identity.session_key) != 0) {
      return refuse(Error::Code::NotReady, "session_releasing");
    }
    if (jobs_.count(identity.job_id) != 0) {
      return refuse(Error::Code::ConfigInvalid, "duplicate_job_id");
    }
    if (jobs_.size() >= max_unreleased_jobs_) {
      return refuse(Error::Code::ResourceExhausted, "job_capacity_exhausted");
    }
    jobs_.emplace(identity.job_id, Job{identity, JobEventSequence(request)});
    submitted_.push_back(std::move(request));
    return {};
  }

  Result<void> cancel(std::uint64_t job_id) override {
    std::lock_guard<std::mutex> guard(mu_);
    const auto it = jobs_.find(job_id);
    if (it == jobs_.end()) {
      return refuse(Error::Code::NotReady, "unknown_job");
    }
    it->second.sequence.note_cancel_requested();
    cancelled_.push_back(job_id);
    return {};
  }

  Result<void> release_session(std::uint64_t session_key, std::uint64_t generation) override {
    std::lock_guard<std::mutex> guard(mu_);
    if (session_key == 0) {
      return refuse(Error::Code::ConfigInvalid, "session_key_zero");
    }
    if (generation == 0) {
      return refuse(Error::Code::ConfigInvalid, "generation_zero");
    }
    if (!sink_) {
      return refuse(Error::Code::NotReady, "event_sink_missing");
    }
    if (acknowledged_.count(session_key) != 0) {
      return {};  // Already acknowledged: at most once.
    }
    if (!releasing_.emplace(session_key, generation).second) {
      return {};  // Already releasing: idempotent.
    }
    for (auto& [job_id, job] : jobs_) {
      if (job.identity.session_key == session_key && !job.sequence.terminal()) {
        job.sequence.note_cancel_requested();
      }
    }
    released_sessions_.emplace_back(session_key, generation);
    return {};
  }

  Result<void> health() override {
    std::lock_guard<std::mutex> guard(mu_);
    return health_;
  }

  // -- Test programming surface: the backend's side. -------------------------

  void set_health(Result<void> health) {
    std::lock_guard<std::mutex> guard(mu_);
    health_ = std::move(health);
  }

  void fail_next_submit(Error error) {
    std::lock_guard<std::mutex> guard(mu_);
    next_submit_error_ = std::move(error);
  }

  /// One bounded job: accepted, completed with `result`, released.
  void run_bounded_job(std::uint64_t job_id, JobResult result) {
    queue_accepted(job_id);
    queue_completed(job_id, std::move(result));
    queue_released(job_id);
  }

  void queue_accepted(std::uint64_t job_id) { queue(JobEvent::accepted(identity_of(job_id))); }
  void queue_progress(std::uint64_t job_id, std::uint32_t sequence, JobResult fragment) {
    queue(JobEvent::progress(identity_of(job_id), sequence, std::move(fragment)));
  }
  void queue_completed(std::uint64_t job_id, JobResult result) {
    queue(JobEvent::completed(identity_of(job_id), std::move(result)));
  }
  void queue_failed(std::uint64_t job_id, Error error) {
    queue(JobEvent::failed(identity_of(job_id), std::move(error)));
  }
  void queue_cancel_acknowledged(std::uint64_t job_id) {
    queue(JobEvent::cancel_acknowledged(identity_of(job_id)));
  }
  void queue_released(std::uint64_t job_id) { queue(JobEvent::released(identity_of(job_id))); }
  /// Queues any event, for example one carrying a stale identity.
  void queue(Result<JobEvent> event) {
    std::lock_guard<std::mutex> guard(mu_);
    if (!event) {
      refusals_.push_back(std::move(event).error());
      return;
    }
    queue_.push_back(std::move(event).value());
  }

  /// Delivers queued events in order, and each session release as soon as
  /// its session has no unreleased job, until nothing is left to deliver.
  /// Returns how many callbacks ran. A refused event is recorded in
  /// refusals() and handled as the contract says.
  std::size_t deliver() {
    const std::lock_guard<std::mutex> delivering(delivery_mu_);
    std::size_t delivered = 0;
    for (;;) {
      std::shared_ptr<JobEventSink> sink;
      std::vector<Delivery> batch;
      {
        std::lock_guard<std::mutex> guard(mu_);
        if (!sink_) {
          return delivered;
        }
        sink = sink_;
        if (auto release = next_session_release_locked()) {
          batch.push_back(*release);
        } else if (!queue_.empty()) {
          JobEvent event = std::move(queue_.front());
          queue_.pop_front();
          batch = admit_locked(std::move(event));
        } else {
          return delivered;
        }
      }
      for (const Delivery& next : batch) {
        dispatch(*sink, next);
        ++delivered;
      }
    }
  }

  // -- Observers. ------------------------------------------------------------

  [[nodiscard]] std::vector<JobRequest> submitted() const {
    std::lock_guard<std::mutex> guard(mu_);
    return submitted_;
  }
  [[nodiscard]] std::vector<std::uint64_t> cancelled() const {
    std::lock_guard<std::mutex> guard(mu_);
    return cancelled_;
  }
  [[nodiscard]] std::vector<std::pair<std::uint64_t, std::uint64_t>> released_sessions() const {
    std::lock_guard<std::mutex> guard(mu_);
    return released_sessions_;
  }
  /// Every event the fake refused to deliver, in order.
  [[nodiscard]] std::vector<Error> refusals() const {
    std::lock_guard<std::mutex> guard(mu_);
    return refusals_;
  }
  [[nodiscard]] std::size_t unreleased_jobs() const {
    std::lock_guard<std::mutex> guard(mu_);
    return jobs_.size();
  }

 private:
  struct Job {
    JobIdentity identity;
    JobEventSequence sequence;
  };
  struct SessionReleased {
    std::uint64_t session_key = 0;
    std::uint64_t generation = 0;
  };
  struct Delivery {
    std::optional<JobEvent> event;
    SessionReleased session;
  };

  static Unexpected refuse(Error::Code code, const char* reason) {
    return unexpected(Error::make(code, reason, reason));
  }

  // Identity of an unreleased job; any other id gets an identity that no
  // job holds, so its events are refused at delivery.
  JobIdentity identity_of(std::uint64_t job_id) const {
    std::lock_guard<std::mutex> guard(mu_);
    const auto it = jobs_.find(job_id);
    return it == jobs_.end() ? JobIdentity{job_id, 1, 1} : it->second.identity;
  }

  std::optional<Delivery> next_session_release_locked() {
    for (auto it = releasing_.begin(); it != releasing_.end(); ++it) {
      bool live = false;
      for (const auto& [job_id, job] : jobs_) {
        live = live || job.identity.session_key == it->first;
      }
      if (!live) {
        Delivery release{std::nullopt, SessionReleased{it->first, it->second}};
        // Recorded before deliver() drops the lock, so a repeat made while
        // or after the acknowledgement runs changes nothing.
        acknowledged_.insert(it->first);
        releasing_.erase(it);
        return release;
      }
    }
    return std::nullopt;
  }

  // Checks one backend event and returns what reaches the sink for it.
  std::vector<Delivery> admit_locked(JobEvent event) {
    const auto it = jobs_.find(event.identity().job_id);
    if (it == jobs_.end()) {
      refusals_.push_back(Error::make(Error::Code::InferenceFailed, "no such job", "unknown_job"));
      return {};
    }
    JobEventSequence& sequence = it->second.sequence;
    const JobIdentity identity = it->second.identity;
    std::vector<Delivery> out;
    if (auto observed = sequence.observe(event); !observed) {
      Error refusal = std::move(observed).error();
      refusals_.push_back(refusal);
      if (sequence.terminal() || event.identity() != identity) {
        return {};  // Late or foreign: dropped.
      }
      // A backend fault ends the job with the refusal in place of the event.
      auto failed = JobEvent::failed(identity, std::move(refusal));
      if (!failed || !sequence.observe(failed.value())) {
        return {};
      }
      out.push_back(Delivery{std::move(failed).value(), {}});
      if (event.kind() != JobEventKind::Released || !sequence.observe(event)) {
        return out;
      }
      // The backend did release the job: deliver that after the failure.
    }
    if (event.kind() == JobEventKind::Released) {
      jobs_.erase(it);
    }
    out.push_back(Delivery{std::move(event), {}});
    return out;
  }

  static void dispatch(JobEventSink& sink, const Delivery& next) {
    if (!next.event) {
      sink.on_session_released(next.session.session_key, next.session.generation);
      return;
    }
    const JobEvent& event = *next.event;
    switch (event.kind()) {
      case JobEventKind::Accepted:
        sink.on_accepted(event);
        return;
      case JobEventKind::Progress:
        sink.on_progress(event);
        return;
      case JobEventKind::Completed:
        sink.on_completed(event);
        return;
      case JobEventKind::Failed:
        sink.on_failed(event);
        return;
      case JobEventKind::CancelAcknowledged:
        sink.on_cancel_acknowledged(event);
        return;
      case JobEventKind::Released:
        sink.on_released(event);
        return;
    }
  }

  mutable std::mutex mu_;
  std::mutex delivery_mu_;  // Held across a whole deliver(): serializes callbacks.
  std::vector<JobClass> served_;
  std::size_t max_unreleased_jobs_;
  std::shared_ptr<JobEventSink> sink_;
  std::unordered_map<std::uint64_t, Job> jobs_;
  std::unordered_map<std::uint64_t, std::uint64_t> releasing_;
  std::unordered_set<std::uint64_t> acknowledged_;  // Session releases delivered.
  std::deque<JobEvent> queue_;
  std::vector<JobRequest> submitted_;
  std::vector<std::uint64_t> cancelled_;
  std::vector<std::pair<std::uint64_t, std::uint64_t>> released_sessions_;
  std::vector<Error> refusals_;
  std::optional<Error> next_submit_error_;
  Result<void> health_;
};

}  // namespace tensorplate::testing
