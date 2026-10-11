// SPDX-License-Identifier: Apache-2.0

#include "job_channel.hpp"

#include <poll.h>
#include <sys/socket.h>

#include <algorithm>
#include <cerrno>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <functional>
#include <memory>
#include <mutex>
#include <optional>
#include <span>
#include <string>
#include <string_view>
#include <system_error>
#include <thread>
#include <utility>
#include <variant>
#include <vector>

#include "tensorplate/backend/bounded_job_bridge.hpp"
#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/job_event.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/result.hpp"
#include "tensorplate/ipc/sidecar_codec.hpp"
#include "tensorplate/ipc/unix_socket.hpp"

#include "job_wire.hpp"
#include "sidecar_frame_io.hpp"

namespace tensorplate::adapters::python_pytorch {

namespace {

const Error kConnectionEnded =
    Error::make(Error::Code::Unavailable, "the sidecar connection ended");

Unexpected refuse(Error::Code code, std::string_view reason) {
  return unexpected(Error::make(code, "job channel: " + std::string(reason), std::string(reason)));
}

// Blocks until the descriptor has bytes, reached EOF, was shut down or failed.
void wait_readable(int fd) noexcept {
  struct pollfd waited {};
  waited.fd = fd;
  waited.events = POLLIN;
  while (::poll(&waited, 1, -1) < 0 && errno == EINTR) {
  }
}

// `error` with texts a `failed` event can carry.
Error bounded(Error error) {
  cut_to_job_error_bound(error.message);
  if (error.context.has_value()) {
    cut_to_job_error_bound(*error.context);
  }
  return error;
}

// Copies the payload region of a `job_submit` for `request` to `out`; false
// when the buffer manager does not hold the request's window.
bool read_payload(const JobRequest& request, const BufferManager& manager,
                  std::vector<std::byte>& out) {
  const PcmWindow* pcm = nullptr;
  if (const auto* audio = std::get_if<AudioFrames>(&request.payload())) {
    pcm = &audio->pcm;
  } else if (const auto* vad = std::get_if<VadFrames>(&request.payload())) {
    pcm = &vad->pcm;
  }
  if (pcm == nullptr) {
    const std::string& text = std::get<TextSegment>(request.payload()).text;
    const auto* bytes = reinterpret_cast<const std::byte*>(text.data());  // NOLINT
    out.assign(bytes, bytes + text.size());
    return true;
  }
  const auto held = manager.data(pcm->buffer);
  if (!held.has_value() || pcm->byte_offset > held.value().size() ||
      pcm->byte_size > held.value().size() - pcm->byte_offset) {
    return false;
  }
  const auto window = held.value().subspan(pcm->byte_offset, pcm->byte_size);
  out.assign(window.begin(), window.end());
  return true;
}

}  // namespace

JobChannel::JobChannel(BufferManager* manager, std::chrono::milliseconds write_timeout)
    : manager_(manager), write_timeout_(write_timeout) {}

bool JobChannel::live_locked() const noexcept {
  return phase_ != Phase::Idle && phase_ != Phase::Stopped && !ended_;
}

bool JobChannel::queue_full_locked() const noexcept {
  return outbox_.size() >= kMaxQueuedFrames;
}

// -- The session's side. -------------------------------------------------------

Result<void> JobChannel::start(ipc::UnixSocket& socket, const std::vector<JobClass>& job_classes) {
  std::unique_lock<std::mutex> lock(mutex_);
  if (phase_ != Phase::Idle && phase_ != Phase::Stopped) {
    return unexpected(Error::Code::Internal, "job channel already started");
  }
  // A reader on no descriptor waits for ever and stop() could not wake it.
  if (!socket.is_open()) {
    return refuse(Error::Code::Internal, "socket_closed");
  }
  job_classes_.clear();
  for (const JobClass job_class : job_classes) {
    if (job_class == JobClass::SttDecode || job_class == JobClass::TtsSynthesis) {
      job_classes_.push_back(job_class);
    }
  }
  socket_ = &socket;
  try {
    writer_ = std::thread(&JobChannel::write_loop, this);
    reader_ = std::thread(&JobChannel::read_loop, this);
  } catch (const std::system_error&) {
    // Only the reader can be missing; the writer leaves an ended connection.
    ended_ = true;
    lock.unlock();
    outbox_cv_.notify_all();
    if (writer_.joinable()) {
      writer_.join();
    }
    lock.lock();
    ended_ = false;
    socket_ = nullptr;
    return unexpected(Error::make(Error::Code::Internal, "job channel threads could not start",
                                  "job_channel_thread_unavailable"));
  }
  phase_ = Phase::Started;
  return Result<void>{};
}

void JobChannel::admit_jobs() noexcept {
  std::lock_guard<std::mutex> lock(mutex_);
  if (phase_ == Phase::Started) {
    phase_ = Phase::Admitting;
  }
}

void JobChannel::close_admission() noexcept {
  std::lock_guard<std::mutex> lock(mutex_);
  if (phase_ == Phase::Started || phase_ == Phase::Admitting) {
    phase_ = Phase::Closing;
  }
}

Result<ipc::SidecarFrame> JobChannel::exchange(const ipc::SidecarFrame& request,
                                               std::string_view message_id, TimePoint deadline) {
  auto encoded = ipc::encode_frame(request);
  if (!encoded.has_value()) {
    return unexpected(encoded.error());
  }
  std::unique_lock<std::mutex> lock(mutex_);
  if (phase_ == Phase::Idle || phase_ == Phase::Stopped) {
    return unexpected(Error::Code::NotReady, "job channel not started");
  }
  if (failure_.has_value()) {
    return unexpected(*failure_);
  }
  if (unary_ != nullptr) {
    return unexpected(Error::Code::Internal, "a sidecar exchange is already waiting");
  }
  if (queue_full_locked()) {
    return refuse(Error::Code::ResourceExhausted, "send_queue_full");
  }
  // Queued first: a push that throws must leave no waiter registered.
  queue_locked(std::move(encoded).value());
  UnaryWaiter waiter{message_id, std::nullopt};
  unary_ = &waiter;
  unary_cv_.wait_until(lock, deadline, [&waiter] { return waiter.outcome.has_value(); });
  if (waiter.outcome.has_value()) {
    return std::move(*waiter.outcome);
  }
  unary_ = nullptr;
  return unexpected(Error::Code::Timeout, "deadline elapsed waiting for the sidecar's response");
}

void JobChannel::stop(std::string_view reason, const std::function<void()>& reap) {
  bool started = false;
  {
    std::lock_guard<std::mutex> lock(mutex_);
    if (phase_ != Phase::Idle && phase_ != Phase::Stopped) {
      stop_reason_ = std::string(reason);
      stopping_ = true;
      started = true;
    }
  }
  if (started) {
    // Wakes a reader in poll or recv and a writer inside a write.
    (void)::shutdown(socket_->fd(), SHUT_RDWR);
    reader_.join();
    writer_.join();
  }
  if (reap) {
    reap();
  }
  if (!started) {
    return;
  }
  // The process is gone, so everything it held is released.
  std::vector<Delivery> out;
  {
    std::lock_guard<std::mutex> lock(mutex_);
    while (!jobs_.empty()) {
      release_locked(jobs_.begin(), out);
    }
    while (!releasing_.empty()) {
      acknowledge_locked(releasing_.begin(), out);
    }
    outbox_.clear();
    failure_.reset();
    write_failure_.reset();
    socket_ = nullptr;
    ended_ = false;
    stopping_ = false;
    phase_ = Phase::Stopped;
  }
  deliver(out);
}

// -- The job side. ---------------------------------------------------------------

Result<void> JobChannel::set_event_sink(std::shared_ptr<JobEventSink> sink) {
  if (!sink) {
    return refuse(Error::Code::ConfigInvalid, "event_sink_null");
  }
  std::lock_guard<std::mutex> lock(mutex_);
  if (sink_) {
    return refuse(Error::Code::ConfigInvalid, "event_sink_already_set");
  }
  sink_ = std::move(sink);
  return Result<void>{};
}

Result<void> JobChannel::submit(const JobRequest& request) {
  const JobIdentity& identity = request.identity();
  std::lock_guard<std::mutex> lock(mutex_);
  if (!sink_) {
    return refuse(Error::Code::NotReady, "event_sink_missing");
  }
  if (phase_ == Phase::Idle || phase_ == Phase::Started || manager_ == nullptr) {
    return refuse(Error::Code::NotReady, "backend_not_ready");
  }
  if (phase_ != Phase::Admitting || ended_) {
    return refuse(Error::Code::Unavailable, "backend_unavailable");
  }
  if (std::find(job_classes_.begin(), job_classes_.end(), request.job_class()) ==
      job_classes_.end()) {
    return refuse(Error::Code::Unsupported, "job_class_unsupported");
  }
  if (releasing_.contains(identity.session_key) ||
      std::find(remembered_sessions_.begin(), remembered_sessions_.end(), identity.session_key) !=
          remembered_sessions_.end()) {
    return refuse(Error::Code::NotReady, "session_releasing");
  }
  if (jobs_.contains(identity.job_id)) {
    return refuse(Error::Code::ConfigInvalid, "duplicate_job_id");
  }
  if (jobs_.size() >= kMaxOpenJobs) {
    return refuse(Error::Code::ResourceExhausted, "job_capacity_exhausted");
  }
  if (queue_full_locked()) {
    return refuse(Error::Code::ResourceExhausted, "send_queue_full");
  }
  std::string message_id = next_message_id();
  auto header = encode_job_submit(request, message_id);
  if (!header.has_value()) {
    return unexpected(header.error());
  }
  std::vector<std::byte> payload;
  if (!read_payload(request, *manager_, payload)) {
    return refuse(Error::Code::ConfigInvalid, "pcm_buffer_invalid");
  }
  auto frame = ipc::encode_frame(ipc::SidecarFrame{std::move(header).value(), std::move(payload)});
  if (!frame.has_value()) {
    return unexpected(frame.error());
  }
  message_jobs_.emplace(message_id, identity.job_id);
  jobs_.emplace(identity.job_id,
                Job{identity, JobEventSequence(request), std::move(message_id), std::string{}});
  queue_locked(std::move(frame).value());
  return Result<void>{};
}

Result<void> JobChannel::cancel(std::uint64_t job_id) {
  std::lock_guard<std::mutex> lock(mutex_);
  const auto found = jobs_.find(job_id);
  if (found == jobs_.end()) {
    return refuse(Error::Code::NotReady, "unknown_job");
  }
  Job& job = found->second;
  if (job.sequence.terminal() || !job.cancel_message_id.empty()) {
    return Result<void>{};
  }
  if (queue_full_locked()) {
    return refuse(Error::Code::ResourceExhausted, "send_queue_full");
  }
  std::string message_id = next_message_id();
  auto header = encode_job_cancel(job.identity, message_id);
  if (!header.has_value()) {
    return unexpected(header.error());
  }
  auto frame = ipc::encode_frame(ipc::SidecarFrame{std::move(header).value(), {}});
  if (!frame.has_value()) {
    return unexpected(frame.error());
  }
  job.sequence.note_cancel_requested();
  message_jobs_.emplace(message_id, job_id);
  job.cancel_message_id = std::move(message_id);
  queue_locked(std::move(frame).value());
  return Result<void>{};
}

Result<void> JobChannel::release_session(std::uint64_t session_key, std::uint64_t generation) {
  std::lock_guard<std::mutex> lock(mutex_);
  if (!sink_) {
    return refuse(Error::Code::NotReady, "event_sink_missing");
  }
  if (!live_locked()) {
    return refuse(Error::Code::Unavailable, "backend_unavailable");
  }
  if (releasing_.contains(session_key) ||
      std::find(remembered_sessions_.begin(), remembered_sessions_.end(), session_key) !=
          remembered_sessions_.end()) {
    return Result<void>{};
  }
  if (releasing_.size() >= kMaxPendingSessionReleases) {
    return refuse(Error::Code::ResourceExhausted, "session_release_capacity_exhausted");
  }
  if (queue_full_locked()) {
    return refuse(Error::Code::ResourceExhausted, "send_queue_full");
  }
  auto header = encode_session_release(session_key, generation, next_message_id());
  if (!header.has_value()) {
    return unexpected(header.error());
  }
  auto frame = ipc::encode_frame(ipc::SidecarFrame{std::move(header).value(), {}});
  if (!frame.has_value()) {
    return unexpected(frame.error());
  }
  releasing_.emplace(session_key, PendingRelease{generation, false});
  // The sidecar cancels the session's unfinished jobs and acknowledges each.
  for (auto& [job_id, job] : jobs_) {
    if (job.identity.session_key == session_key) {
      job.sequence.note_cancel_requested();
    }
  }
  queue_locked(std::move(frame).value());
  return Result<void>{};
}

JobChannelCounters JobChannel::counters() const {
  std::lock_guard<std::mutex> lock(mutex_);
  return counters_;
}

// -- The threads. ----------------------------------------------------------------

void JobChannel::read_loop() {
  for (;;) {
    wait_readable(socket_->fd());
    auto frame = read_frame(*socket_, TimePoint::max());
    if (!frame.has_value()) {
      end_connection(frame.error());
      return;
    }
    if (!route(std::move(frame).value())) {
      end_connection(
          Error::make(Error::Code::InferenceFailed, "sidecar sent an unreadable header"));
      return;
    }
  }
}

void JobChannel::write_loop() {
  for (;;) {
    std::vector<std::byte> frame;
    {
      std::unique_lock<std::mutex> lock(mutex_);
      outbox_cv_.wait(lock, [this] { return !outbox_.empty() || ended_; });
      if (ended_) {
        return;
      }
      frame = std::move(outbox_.front());
      outbox_.pop_front();
    }
    auto written = socket_->write_all(frame, ipc::UnixSocket::Clock::now() + write_timeout_);
    if (!written.has_value()) {
      {
        std::lock_guard<std::mutex> lock(mutex_);
        write_failure_ = written.error();
      }
      // A frame was cut short: the reader ends the connection for both.
      (void)::shutdown(socket_->fd(), SHUT_RDWR);
      return;
    }
  }
}

void JobChannel::end_connection(const Error& error) {
  std::vector<Delivery> out;
  {
    std::lock_guard<std::mutex> lock(mutex_);
    // A write that failed first is what ended it.
    failure_ = write_failure_.value_or(error);
    ended_ = true;
    if (unary_ != nullptr) {
      std::exchange(unary_, nullptr)->outcome = unexpected(*failure_);
    }
    Error ended = kConnectionEnded;
    ended.context = stopping_ ? stop_reason_ : std::string(kBackendReset);
    for (auto& [job_id, job] : jobs_) {
      fail_locked(job, ended, out);
    }
  }
  // The sidecar reads end of file and a writer inside a frame is woken.
  (void)::shutdown(socket_->fd(), SHUT_RDWR);
  unary_cv_.notify_all();
  outbox_cv_.notify_all();
  deliver(out);
}

bool JobChannel::route(ipc::SidecarFrame frame) {
  std::optional<BufferRef> published;
  PcmPublisher publish;
  if (manager_ != nullptr) {
    publish = [&](std::span<const std::byte> pcm) -> Result<BufferRef> {
      auto buffer = manager_->allocate(pcm.size());
      if (!buffer.has_value()) {
        return unexpected(buffer.error());
      }
      published = buffer.value();
      auto bytes = manager_->data(buffer.value());
      if (!bytes.has_value()) {
        return unexpected(bytes.error());
      }
      std::memcpy(bytes.value().data(), pcm.data(), pcm.size());
      return buffer.value();
    };
  }
  SidecarJobMessage message = decode_sidecar_message(frame, publish);
  if (std::holds_alternative<UnreadableHeader>(message)) {
    return false;
  }
  std::vector<Delivery> out;
  bool chunk_delivered = false;
  {
    std::lock_guard<std::mutex> lock(mutex_);
    if (const auto* event = std::get_if<JobEvent>(&message)) {
      chunk_delivered = admit_locked(*event, out);
    } else if (const auto* fault = std::get_if<JobMessageFault>(&message)) {
      const auto found = jobs_.find(fault->job_id);
      if (found != jobs_.end() && !found->second.sequence.terminal()) {
        fail_locked(found->second, fault->error, out);
      } else if (fault->job_id != 0 || fault->session.has_value()) {
        ++counters_.dropped_job_messages;
      } else {
        ++counters_.dropped_other_messages;
      }
    } else if (const auto* released = std::get_if<SessionReleased>(&message)) {
      const auto pending = releasing_.find(released->session_key);
      if (pending == releasing_.end() || pending->second.acknowledged ||
          pending->second.generation != released->generation) {
        ++counters_.dropped_job_messages;
      } else {
        pending->second.acknowledged = true;
        settle_locked(released->session_key, out);
      }
    } else if (auto* refused = std::get_if<MessageRefused>(&message)) {
      refuse_locked(*refused, std::move(frame), out);
    } else {
      offer_unary_locked(std::move(frame), nullptr);
    }
  }
  // No event the sink receives holds the chunk, so nobody else releases it.
  if (published.has_value() && !chunk_delivered) {
    (void)manager_->release_if_owned(*published);
  }
  deliver(out);
  return true;
}

void JobChannel::deliver(const std::vector<Delivery>& deliveries) {
  if (deliveries.empty()) {
    return;
  }
  std::shared_ptr<JobEventSink> sink;
  {
    std::lock_guard<std::mutex> lock(mutex_);
    sink = sink_;
  }
  for (const Delivery& delivery : deliveries) {
    if (const auto* session = std::get_if<SessionReleased>(&delivery)) {
      sink->on_session_released(session->session_key, session->generation);
      continue;
    }
    const auto& event = std::get<JobEvent>(delivery);
    switch (event.kind()) {
      case JobEventKind::Accepted:
        sink->on_accepted(event);
        break;
      case JobEventKind::Progress:
        sink->on_progress(event);
        break;
      case JobEventKind::Completed:
        sink->on_completed(event);
        break;
      case JobEventKind::Failed:
        sink->on_failed(event);
        break;
      case JobEventKind::CancelAcknowledged:
        sink->on_cancel_acknowledged(event);
        break;
      case JobEventKind::Released:
        sink->on_released(event);
        break;
    }
  }
}

// -- Under the mutex. ------------------------------------------------------------

bool JobChannel::admit_locked(const JobEvent& event, std::vector<Delivery>& out) {
  const auto found = jobs_.find(event.identity().job_id);
  if (found == jobs_.end()) {
    ++counters_.dropped_job_messages;
    return false;
  }
  Job& job = found->second;
  if (const auto observed = job.sequence.observe(event); !observed.has_value()) {
    if (job.sequence.terminal() || event.identity() != job.identity) {
      ++counters_.dropped_job_messages;
      return false;
    }
    // A sidecar fault: the job ends with the refusal in the event's place.
    fail_locked(job, observed.error(), out);
    // A release the sidecar did report still follows.
    if (event.kind() != JobEventKind::Released) {
      return false;
    }
    release_locked(found, out);
    return false;
  }
  out.emplace_back(event);
  if (event.kind() == JobEventKind::Released) {
    forget_locked(found, out);
  }
  return true;
}

void JobChannel::fail_locked(Job& job, const Error& error, std::vector<Delivery>& out) {
  auto failed = JobEvent::failed(job.identity, bounded(error));
  if (failed.has_value() && job.sequence.observe(failed.value()).has_value()) {
    out.emplace_back(std::move(failed).value());
  }
}

void JobChannel::release_locked(std::map<std::uint64_t, Job>::iterator job,
                                std::vector<Delivery>& out) {
  auto released = JobEvent::released(job->second.identity);
  if (released.has_value() && job->second.sequence.observe(released.value()).has_value()) {
    out.emplace_back(std::move(released).value());
  }
  forget_locked(job, out);
}

void JobChannel::forget_locked(std::map<std::uint64_t, Job>::iterator job,
                               std::vector<Delivery>& out) {
  const std::uint64_t session_key = job->second.identity.session_key;
  message_jobs_.erase(job->second.submit_message_id);
  message_jobs_.erase(job->second.cancel_message_id);
  jobs_.erase(job);
  settle_locked(session_key, out);
}

void JobChannel::settle_locked(std::uint64_t session_key, std::vector<Delivery>& out) {
  const auto pending = releasing_.find(session_key);
  if (pending == releasing_.end() || !pending->second.acknowledged) {
    return;
  }
  for (const auto& [job_id, job] : jobs_) {
    if (job.identity.session_key == session_key) {
      return;
    }
  }
  acknowledge_locked(pending, out);
}

void JobChannel::acknowledge_locked(std::map<std::uint64_t, PendingRelease>::iterator pending,
                                    std::vector<Delivery>& out) {
  out.emplace_back(SessionReleased{pending->first, pending->second.generation});
  remembered_sessions_.push_back(pending->first);
  if (remembered_sessions_.size() > kMaxRememberedSessions) {
    remembered_sessions_.pop_front();
  }
  releasing_.erase(pending);
}

void JobChannel::refuse_locked(const MessageRefused& refused, ipc::SidecarFrame&& frame,
                               std::vector<Delivery>& out) {
  const auto named = message_jobs_.find(refused.message_id);
  if (named == message_jobs_.end()) {
    offer_unary_locked(std::move(frame), &refused.message_id);
    return;
  }
  const auto found = jobs_.find(named->second);
  if (found->second.submit_message_id != refused.message_id) {
    // A refused cancel: the job runs on and ends by itself.
    ++counters_.dropped_job_messages;
    return;
  }
  // The sidecar never took the job, so it holds nothing to release.
  fail_locked(found->second, refused.error, out);
  release_locked(found, out);
}

void JobChannel::offer_unary_locked(ipc::SidecarFrame&& frame, const std::string* message_id) {
  if (unary_ == nullptr || (message_id != nullptr && *message_id != unary_->message_id)) {
    ++counters_.dropped_other_messages;
    return;
  }
  // Taking the waiter is what answers it: a later frame finds none.
  std::exchange(unary_, nullptr)->outcome = std::move(frame);
  unary_cv_.notify_all();
}

void JobChannel::queue_locked(std::vector<std::byte> frame) {
  outbox_.push_back(std::move(frame));
  outbox_cv_.notify_one();
}

}  // namespace tensorplate::adapters::python_pytorch
