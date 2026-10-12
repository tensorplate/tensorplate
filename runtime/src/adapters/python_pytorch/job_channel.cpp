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
#include <deque>
#include <functional>
#include <iterator>
#include <map>
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

Error refusal(Error::Code code, std::string_view reason) {
  return Error::make(code, "job channel: " + std::string(reason), std::string(reason));
}

Unexpected refuse(Error::Code code, std::string_view reason) {
  return unexpected(refusal(code, reason));
}

// What a frame that carries the health probe's id says of the backend.
Result<void> health_answer(const ipc::SidecarFrame& frame) {
  const auto ready = reports_ready(frame.json_header);
  if (!ready.has_value()) {
    return unexpected(
        Error::make(Error::Code::Unavailable, ready.error().message, "backend_unavailable"));
  }
  if (!ready.value()) {
    return refuse(Error::Code::NotReady, "backend_not_ready");
  }
  return Result<void>{};
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

bool ReleasedSessionKeys::contains(std::uint64_t key) const noexcept {
  const auto after = runs_.upper_bound(key);
  return after != runs_.begin() && std::prev(after)->second >= key;
}

void ReleasedSessionKeys::insert(std::uint64_t key) {
  const auto after = runs_.upper_bound(key);
  const auto before = after == runs_.begin() ? runs_.end() : std::prev(after);
  if (before != runs_.end() && before->second >= key) {
    return;
  }
  // `before` ends below `key` and `after` begins above it: neither sum wraps.
  const bool joins_before = before != runs_.end() && before->second + 1 == key;
  const bool joins_after = after != runs_.end() && key + 1 == after->first;
  if (joins_before && joins_after) {
    before->second = after->second;
    runs_.erase(after);
  } else if (joins_before) {
    before->second = key;
  } else if (joins_after) {
    auto run = runs_.extract(after);
    run.key() = key;
    runs_.insert(std::move(run));
  } else {
    runs_.emplace(key, key);
  }
}

JobChannel::JobChannel(BufferManager* manager, std::chrono::milliseconds write_timeout)
    : manager_(manager), write_timeout_(write_timeout) {}

bool JobChannel::live_locked() const noexcept {
  return phase_ != Phase::Idle && phase_ != Phase::Stopped && !ended_;
}

std::optional<Error> JobChannel::not_running_locked() const {
  if (retired_) {
    return refusal(Error::Code::Unavailable, "backend_unavailable");
  }
  if (phase_ == Phase::Idle || phase_ == Phase::Started || manager_ == nullptr) {
    return refusal(Error::Code::NotReady, "backend_not_ready");
  }
  if (phase_ != Phase::Admitting || ended_) {
    return refusal(Error::Code::Unavailable, "backend_unavailable");
  }
  return std::nullopt;
}

// -- The session's side. -------------------------------------------------------

Result<void> JobChannel::start(ipc::UnixSocket& socket, const std::vector<JobClass>& job_classes) {
  std::unique_lock<std::mutex> lock(mutex_);
  if (retired_) {
    return refuse(Error::Code::Internal, "job_channel_retired");
  }
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
  if (job_classes_.empty()) {
    return refuse(Error::Code::Unsupported, "job_class_unsupported");
  }
  socket_ = &socket;
  try {
    writer_ = std::thread(&JobChannel::write_loop, this);
    reader_ = std::thread(&JobChannel::read_loop, this);
  } catch (const std::system_error&) {
    // A writer that was created leaves an ended connection.
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
    // A health call waits for an answer the sidecar may never send.
    probe_cv_.notify_all();
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
  // Queued first: a push that throws must leave no waiter registered.
  queue_locked(QueuedFor::Exchange, 0, std::move(encoded).value());
  UnaryWaiter waiter{message_id, std::nullopt};
  unary_ = &waiter;
  unary_cv_.wait_until(lock, deadline, [&waiter] { return waiter.outcome.has_value(); });
  if (waiter.outcome.has_value()) {
    return std::move(*waiter.outcome);
  }
  unary_ = nullptr;
  unqueue_locked(QueuedFor::Exchange, 0);
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
    failure_.reset();
    write_failure_.reset();
    socket_ = nullptr;
    ended_ = false;
    stopping_ = false;
    phase_ = Phase::Stopped;
    owe_locked(out);
  }
  deliver(out);
}

void JobChannel::retire() noexcept {
  std::lock_guard<std::mutex> lock(mutex_);
  retired_ = true;
  // A health call that waits returns for the state, as when admission closes.
  probe_cv_.notify_all();
}

// -- The job side. ---------------------------------------------------------------

Result<void> JobChannel::set_event_sink(std::shared_ptr<JobEventSink> sink) {
  if (!sink) {
    return refuse(Error::Code::ConfigInvalid, "event_sink_null");
  }
  std::lock_guard<std::mutex> lock(mutex_);
  if (sink_ || sink_detached_) {
    return refuse(Error::Code::ConfigInvalid, "event_sink_already_set");
  }
  sink_ = std::move(sink);
  return Result<void>{};
}

void JobChannel::detach_event_sink() {
  // Declared first: the sink is let go once the mutex is.
  std::shared_ptr<JobEventSink> detached;
  std::unique_lock<std::mutex> lock(mutex_);
  sink_detached_ = true;
  ++detach_waiters_;
  delivered_cv_.wait(lock, [this] { return delivering_ == std::thread::id{}; });
  --detach_waiters_;
  detached = std::move(sink_);
}

bool JobChannel::in_sink_callback() const {
  std::lock_guard<std::mutex> lock(mutex_);
  return delivering_ == std::this_thread::get_id();
}

std::size_t JobChannel::detach_waiters() const {
  std::lock_guard<std::mutex> lock(mutex_);
  return detach_waiters_;
}

Result<void> JobChannel::submit(const JobRequest& request) {
  const JobIdentity& identity = request.identity();
  std::lock_guard<std::mutex> lock(mutex_);
  if (!sink_) {
    return refuse(Error::Code::NotReady, "event_sink_missing");
  }
  if (auto refused = not_running_locked(); refused.has_value()) {
    return unexpected(std::move(*refused));
  }
  if (std::find(job_classes_.begin(), job_classes_.end(), request.job_class()) ==
      job_classes_.end()) {
    return refuse(Error::Code::Unsupported, "job_class_unsupported");
  }
  if (releasing_.contains(identity.session_key) ||
      released_sessions_.contains(identity.session_key)) {
    return refuse(Error::Code::NotReady, "session_releasing");
  }
  if (jobs_.contains(identity.job_id)) {
    return refuse(Error::Code::ConfigInvalid, "duplicate_job_id");
  }
  if (jobs_.size() >= kMaxOpenJobs) {
    return refuse(Error::Code::ResourceExhausted, "job_capacity_exhausted");
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
  queue_locked(QueuedFor::Submit, identity.job_id, std::move(frame).value());
  return Result<void>{};
}

Result<void> JobChannel::cancel(std::uint64_t job_id) {
  std::lock_guard<std::mutex> lock(mutex_);
  const auto found = jobs_.find(job_id);
  // A job still open when the sink was detached has nobody to cancel it for.
  if (found == jobs_.end() || sink_detached_) {
    return refuse(Error::Code::NotReady, "unknown_job");
  }
  Job& job = found->second;
  if (job.sequence.terminal() || !job.cancel_message_id.empty()) {
    return Result<void>{};
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
  queue_locked(QueuedFor::Cancel, job_id, std::move(frame).value());
  return Result<void>{};
}

Result<void> JobChannel::release_session(std::uint64_t session_key, std::uint64_t generation) {
  std::lock_guard<std::mutex> lock(mutex_);
  if (!sink_) {
    return refuse(Error::Code::NotReady, "event_sink_missing");
  }
  if (retired_) {
    // No job is taken again, so there is no key to keep from one.
    return refuse(Error::Code::Unavailable, "backend_unavailable");
  }
  if (!live_locked()) {
    // The caller takes the session as released and never submits its key
    // again. A channel not started yet has had no session to release, and
    // arguments a live channel refuses name none.
    if (phase_ != Phase::Idle &&
        encode_session_release(session_key, generation, "unsent").has_value()) {
      released_sessions_.insert(session_key);
    }
    return refuse(Error::Code::Unavailable, "backend_unavailable");
  }
  if (releasing_.contains(session_key) || released_sessions_.contains(session_key)) {
    return Result<void>{};
  }
  if (releasing_.size() >= kMaxPendingSessionReleases) {
    return refuse(Error::Code::ResourceExhausted, "session_release_capacity_exhausted");
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
  queue_locked(QueuedFor::Release, session_key, std::move(frame).value());
  return Result<void>{};
}

Result<void> JobChannel::probe_health(TimePoint deadline) {
  std::unique_lock<std::mutex> lock(mutex_);
  if (auto refused = not_running_locked(); refused.has_value()) {
    return unexpected(std::move(*refused));
  }
  if (!probe_) {
    auto asked = std::make_shared<HealthProbe>();
    asked->message_id = next_message_id();
    auto header = encode_health_check(asked->message_id);
    if (!header.has_value()) {
      return unexpected(header.error());
    }
    auto frame = ipc::encode_frame(ipc::SidecarFrame{std::move(header).value(), {}});
    if (!frame.has_value()) {
      return unexpected(frame.error());
    }
    // Queued first: a push that throws must leave no probe standing.
    queue_locked(QueuedFor::Probe, 0, std::move(frame).value());
    probe_ = std::move(asked);
  }
  // The channel lets go of the probe when it ends; this caller reads it after.
  const std::shared_ptr<HealthProbe> probe = probe_;
  ++probe->waiting;
  probe_cv_.wait_until(lock, deadline, [this, &probe] {
    return probe->outcome.has_value() || not_running_locked().has_value();
  });
  --probe->waiting;
  // The channel's state first, as for a call made now: an unload may have
  // closed admission while the call waited, answered or not.
  if (auto refused = not_running_locked(); refused.has_value()) {
    return unexpected(std::move(*refused));
  }
  const std::optional<Result<void>> outcome = probe->outcome;
  if (!outcome.has_value()) {
    return refuse(Error::Code::Timeout, "health_timeout");
  }
  return *outcome;
}

std::size_t JobChannel::health_waiters() const {
  std::lock_guard<std::mutex> lock(mutex_);
  return probe_ ? probe_->waiting : 0;
}

JobChannelBacklog JobChannel::backlog() const {
  std::lock_guard<std::mutex> lock(mutex_);
  return JobChannelBacklog{jobs_.size(), releasing_.size(), owed_callbacks_};
}

JobChannelCounters JobChannel::counters() const {
  std::lock_guard<std::mutex> lock(mutex_);
  return counters_;
}

std::size_t JobChannel::released_session_runs() const {
  std::lock_guard<std::mutex> lock(mutex_);
  return released_sessions_.runs();
}

std::size_t JobChannel::queued_frames() const {
  std::lock_guard<std::mutex> lock(mutex_);
  return outbox_.size();
}

// -- The threads. ----------------------------------------------------------------

void JobChannel::read_loop() {
  for (;;) {
    wait_readable(socket_->fd());
    auto frame = read_frame(*socket_, TimePoint::max(), kMaxJobFramePayloadBytes);
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
      // Out of the queue before the first byte: nothing can remove it now.
      frame = std::move(outbox_.front().bytes);
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
      unqueue_locked(QueuedFor::Exchange, 0);
    }
    if (probe_) {
      end_probe_locked(refuse(Error::Code::Unavailable, "backend_unavailable"));
    }
    Error ended = kConnectionEnded;
    ended.context = stopping_ ? stop_reason_ : std::string(kBackendReset);
    for (auto& [job_id, job] : jobs_) {
      fail_locked(job, ended, out);
    }
    owe_locked(out);
  }
  // The sidecar reads end of file and a writer inside a frame is woken.
  (void)::shutdown(socket_->fd(), SHUT_RDWR);
  unary_cv_.notify_all();
  outbox_cv_.notify_all();
  deliver(out);
}

bool JobChannel::names_probe_locked(std::string_view message_id) const noexcept {
  return probe_ && probe_->message_id == message_id;
}

void JobChannel::end_probe_locked(Result<void> outcome) {
  probe_->outcome = std::move(outcome);
  probe_.reset();
  unqueue_locked(QueuedFor::Probe, 0);
  probe_cv_.notify_all();
}

void JobChannel::answer_probe_locked(Result<void> outcome) {
  if (probe_->waiting == 0) {
    // Every caller gave up before it came.
    ++counters_.dropped_other_messages;
  }
  end_probe_locked(std::move(outcome));
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
      offer_other_locked(std::get<NotJobMessage>(message).message_id, std::move(frame));
    }
    owe_locked(out);
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
  // The batch is marked in flight, so detach_event_sink() leaves the sink in
  // place until it has returned: the channel's reference is the only one.
  JobEventSink* sink = nullptr;
  {
    std::lock_guard<std::mutex> lock(mutex_);
    sink = sink_.get();
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
  {
    std::lock_guard<std::mutex> lock(mutex_);
    owed_callbacks_ -= deliveries.size();
    delivering_ = std::thread::id{};
  }
  delivered_cv_.notify_all();
}

// -- Under the mutex. ------------------------------------------------------------

std::optional<JobEvent> JobChannel::unloaded_locked(const JobEvent& event) const {
  if (phase_ != Phase::Closing) {
    return std::nullopt;
  }
  std::optional<Error> error = event.error();
  if (!error.has_value() || error->code != Error::Code::Unavailable ||
      error->context != "backend_unavailable") {
    return std::nullopt;
  }
  error->context = std::string(kSessionUnloaded);
  auto unloaded = JobEvent::failed(event.identity(), std::move(*error));
  return unloaded.has_value() ? std::optional<JobEvent>(std::move(unloaded).value()) : std::nullopt;
}

void JobChannel::owe_locked(std::vector<Delivery>& out) noexcept {
  if (sink_detached_) {
    out.clear();
    return;
  }
  if (!out.empty()) {
    owed_callbacks_ += out.size();
    delivering_ = std::this_thread::get_id();
  }
}

bool JobChannel::admit_locked(const JobEvent& received, std::vector<Delivery>& out) {
  const std::optional<JobEvent> unloaded = unloaded_locked(received);
  const JobEvent& event = unloaded.has_value() ? *unloaded : received;
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
  // A detached sink receives nothing: owe_locked() empties the batch.
  return !sink_detached_;
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
  const std::uint64_t job_id = job->first;
  const std::uint64_t session_key = job->second.identity.session_key;
  message_jobs_.erase(job->second.submit_message_id);
  message_jobs_.erase(job->second.cancel_message_id);
  jobs_.erase(job);
  unqueue_locked(QueuedFor::Submit, job_id);
  unqueue_locked(QueuedFor::Cancel, job_id);
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
  released_sessions_.insert(pending->first);
  unqueue_locked(QueuedFor::Release, pending->first);
  releasing_.erase(pending);
}

void JobChannel::refuse_locked(const MessageRefused& refused, ipc::SidecarFrame&& frame,
                               std::vector<Delivery>& out) {
  const auto named = message_jobs_.find(refused.message_id);
  if (named == message_jobs_.end()) {
    if (names_probe_locked(refused.message_id)) {
      answer_probe_locked(unexpected(
          Error::make(Error::Code::Unavailable, refused.error.message, "backend_unavailable")));
      return;
    }
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

void JobChannel::offer_other_locked(const std::string& message_id, ipc::SidecarFrame&& frame) {
  if (names_probe_locked(message_id)) {
    // Ahead of the exchange's slot: the probe's answer is never an exchange's.
    answer_probe_locked(health_answer(frame));
    return;
  }
  offer_unary_locked(std::move(frame), nullptr);
}

void JobChannel::offer_unary_locked(ipc::SidecarFrame&& frame, const std::string* message_id) {
  if (unary_ == nullptr || (message_id != nullptr && *message_id != unary_->message_id)) {
    ++counters_.dropped_other_messages;
    return;
  }
  // Taking the waiter is what answers it: a later frame finds none.
  std::exchange(unary_, nullptr)->outcome = std::move(frame);
  unqueue_locked(QueuedFor::Exchange, 0);
  unary_cv_.notify_all();
}

void JobChannel::queue_locked(QueuedFor owner, std::uint64_t key, std::vector<std::byte> frame) {
  outbox_.push_back(QueuedFrame{owner, key, std::move(frame)});
  outbox_cv_.notify_one();
}

void JobChannel::unqueue_locked(QueuedFor owner, std::uint64_t key) {
  std::erase_if(outbox_, [owner, key](const QueuedFrame& queued) {
    return queued.owner == owner && queued.key == key;
  });
}

}  // namespace tensorplate::adapters::python_pytorch
