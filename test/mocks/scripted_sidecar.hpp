// SPDX-License-Identifier: Apache-2.0
//
// A sidecar played by a thread, for tests of the adapter's side of the
// socket. It stands in for the in-tree runner
// (backends/python_pytorch/src/tensorplate_pytorch_backend/runner.py and
// jobs.py) and keeps that runner's rules itself, so a test cannot have it
// accept what the runner refuses:
//
//   - `ready_event` first; a unary answer carries the request's `message_id`,
//     a message of its own the next `s<n>`;
//   - a frame whose `schema_version` is not "0.1" is refused `unsupported`,
//     and one without a `message_id` or a `kind` `config_invalid` under an id
//     of this sidecar's own, both in arrival order behind what waits;
//   - a frame whose header is not a JSON object ends the connection;
//   - a load is refused as the runner refuses it (an empty `capabilities`
//     list, a capability it did not list, a runner with no job class a lane
//     runs), and one it takes declares the classes a lane runs and no other,
//     as the runner does for a backend without `vad_frames`;
//   - `health_check` is answered at once; any other unary request and every
//     admitted job wait in one FIFO, and a load or an unload fails the jobs
//     still waiting when it arrives;
//   - job and session messages are acted on under one lock, which is also
//     held while a job message is written; a job message before a load
//     enabled them, a submit whose `payload_length` is not its payload's, a
//     duplicate unreleased job id and a mismatched cancel are answered with
//     an `error_event`, a cancel for an unknown job with nothing, and a
//     released session is forgotten;
//   - at most 8 jobs wait behind the running one;
//   - a write that makes no progress for `write_stall` ends the connection,
//     and it never stops reading unless a test tells it to.
//
// Where it is known to differ from the runner:
//
//   - of a submit's input it checks the type and the payload's length only,
//     and keeps the payload for the test to read;
//   - a `vad_frames` job, which only Options::declares_any_job_class lets a
//     load declare, waits in the one FIFO, where the runner gives those jobs
//     a lane of their own;
//   - it has no backend: a job ends as the test says. The thread that plays
//     one (job_bridge_harness.hpp) always completes a job with the fixture
//     bridge's result, a `vad_frames` job of any frame size included. It
//     never raises and reports no progress, and a submit is refused as not
//     permitted only when a test arms it;
//   - that thread lets a job it holds go once an unload waits behind it. A
//     runner's job may end so; it may also outlast the unload's bound, which
//     that thread never plays and no conformance case reaches;
//   - it has no bound on the unary requests that wait, and it writes a unary
//     answer under the job lock, where the runner writes it outside;
//   - an id of its own for a frame without one is the next `s<n>`, where the
//     runner draws a random one, and a `message_id` that is not a string
//     counts as none, where the runner echoes it;
//   - it runs no tensor: `infer`, `infer_async` and `cancel` are refused as
//     unknown kinds, a `prime` is answered whether or not anything is
//     loaded, and a load is not held to its `model_spec`;
//   - `capabilities` that is `null` or an empty object is refused as an
//     empty list is (`config_invalid`), where the runner answers
//     `unsupported`;
//   - a load answered while an unload waits behind it enables jobs, and they
//     stay enabled after that unload; the runner's does not enable them;
//   - `health` carries `ready` alone, and an error's text is one letter;
//   - a frame whose prefix is wrong (the magic, the version, a length over
//     the codec's bound) is waited on as an incomplete frame, where the
//     runner ends the connection;
//   - at end of file it only stops reading, and still answers what waits
//     when the test says how it ends; the runner, once a load enabled jobs,
//     writes no more and drops what waits.
//
// The test is the backend: it takes the job at the head of the FIFO and says
// how it ends (job_bridge_harness.hpp has a thread that does). emit(), refuse(), stop_reading(),
// stop_reading_after(), hold_health(), Options::declares_any_job_class and the overrides in Options
// step outside the runner's behaviour for what the adapter has to survive.

#pragma once

#include <gtest/gtest.h>
#include <poll.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include <algorithm>
#include <atomic>
#include <cerrno>
#include <chrono>
#include <condition_variable>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <deque>
#include <functional>
#include <map>
#include <mutex>
#include <nlohmann/json.hpp>
#include <optional>
#include <set>
#include <string>
#include <thread>
#include <utility>
#include <variant>
#include <vector>

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/result.hpp"
#include "tensorplate/ipc/sidecar_codec.hpp"

#include "adapters/python_pytorch/sidecar_process.hpp"

namespace tensorplate::testing {

class ScriptedSidecar {
 public:
  using Json = nlohmann::json;

  struct Options {
    std::vector<std::string> capabilities{"speech_jobs_v1"};
    /// False plays a runner older than the list: its `ready_event` has no
    /// `capabilities` key, and it lists nothing a load could enable.
    bool lists_capabilities = true;
    /// The job classes of the loaded "runner". A load with jobs declares those
    /// of them the runner declares for a backend without `vad_frames`
    /// (`stt_decode`, `tts_synthesis`) and is refused when that leaves none.
    std::vector<std::string> job_classes{"stt_decode", "tts_synthesis"};
    /// True declares `job_classes` as given, whether a lane runs them or not,
    /// which no runner does.
    bool declares_any_job_class = false;
    std::chrono::milliseconds write_stall{std::chrono::seconds{5}};
    /// The texts of the error a load this sidecar refuses is answered with.
    std::string load_refusal_message = "m";
    std::optional<std::string> load_refusal_context;
    /// Laid over the `ready_event`, and over the answer to a load this
    /// sidecar takes: field values no runner sends.
    Json ready_overrides = Json::object();
    Json load_response_overrides = Json::object();
  };

  /// How long a test waits for the other side before it fails.
  static constexpr std::chrono::seconds kPatience{30};

  ScriptedSidecar() = default;
  explicit ScriptedSidecar(Options options) : options_(std::move(options)) {}
  ~ScriptedSidecar() { finish(); }

  ScriptedSidecar(const ScriptedSidecar&) = delete;
  ScriptedSidecar& operator=(const ScriptedSidecar&) = delete;
  ScriptedSidecar(ScriptedSidecar&&) = delete;
  ScriptedSidecar& operator=(ScriptedSidecar&&) = delete;

  /// Starts this sidecar as the "process" of a SidecarProcess: it connects to
  /// the request's socket and announces itself.
  [[nodiscard]] adapters::python_pytorch::SidecarLauncher launcher() {
    return [this](const adapters::python_pytorch::SidecarLaunchRequest& request)
               -> Result<adapters::python_pytorch::SidecarHandle> {
      struct sockaddr_un address {};
      address.sun_family = AF_UNIX;
      std::strncpy(address.sun_path, request.socket_path.c_str(), sizeof(address.sun_path) - 1);
      fd_ = ::socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0);
      // NOLINTNEXTLINE(cppcoreguidelines-pro-type-reinterpret-cast)
      if (fd_ < 0 ||
          ::connect(fd_, reinterpret_cast<const sockaddr*>(&address), sizeof(address)) != 0) {
        return unexpected(Error::Code::LoadFailed, "scripted sidecar could not connect");
      }
      // A frame with the largest result is larger than the socket holds,
      // whatever the host's default is.
      const int held = kSocketBufferBytes;
      (void)::setsockopt(fd_, SOL_SOCKET, SO_SNDBUF, &held, sizeof(held));
      Json ready = originated("ready_event", Json::object());
      if (options_.lists_capabilities) {
        ready["capabilities"] = options_.capabilities;
      }
      ready.update(options_.ready_overrides);
      write_frame(ready, 0);
      reader_ = std::thread([this] { read_loop(); });
      return adapters::python_pytorch::SidecarHandle::make(
          0,
          [this](adapters::python_pytorch::SidecarHandle&) {
            if (on_terminate) {
              on_terminate();
            }
            finish();
            std::lock_guard<std::mutex> lock(log_mutex_);
            reaped_ = true;
          },
          [this](adapters::python_pytorch::SidecarHandle&) { return !dead_ && !reaped(); });
    };
  }

  // -- The backend. --------------------------------------------------------------

  /// A job the backend took, with the header and the payload of its submit;
  /// `job_id` is zero when none came.
  struct TakenJob {
    std::uint64_t job_id = 0;
    Json submit = Json::object();
    std::vector<std::byte> payload{};
  };

  /// Waits up to `patience` for a job at the head of the FIFO with none
  /// running and starts it.
  [[nodiscard]] TakenJob take(std::chrono::milliseconds patience) {
    std::unique_lock<std::mutex> lock(table_mutex_);
    if (!work_.wait_for(lock, patience, [this] { return job_due_locked(); })) {
      return {};
    }
    running_ = std::get<std::uint64_t>(lane_.front());
    lane_.pop_front();
    const Job& job = jobs_.at(running_);
    return TakenJob{running_, job.submit, job.payload};
  }

  /// take() with this sidecar's patience.
  /// @return the job's id; zero when none came.
  [[nodiscard]] std::uint64_t take_job() { return take(kPatience).job_id; }

  /// What the backend sees of the FIFO: the job it runs, zero for none,
  /// whether take() would return one now, and whether an `unload` waits in
  /// it, which it does only behind the running job.
  struct Lane {
    std::uint64_t running = 0;
    bool job_due = false;
    bool unload_waits = false;
  };
  [[nodiscard]] Lane lane() {
    std::lock_guard<std::mutex> lock(table_mutex_);
    const bool unload_waits = std::any_of(lane_.begin(), lane_.end(), [](const Work& work) {
      const Json* request = std::get_if<Json>(&work);
      return request != nullptr && text_of(*request, "kind") == "unload";
    });
    return Lane{running_, job_due_locked(), unload_waits};
  }

  /// Ends the running job as the runner does when its backend call returns:
  /// `job_completed` with `result` and `payload_bytes` zero bytes, or
  /// `job_failed` (`cancelled`) when it was cancelled meanwhile; then
  /// `job_released`.
  void complete(std::uint64_t job_id, const Json& result, std::size_t payload_bytes = 0) {
    std::lock_guard<std::mutex> lock(table_mutex_);
    running_ = 0;
    const auto job = jobs_.find(job_id);
    if (job == jobs_.end()) {
      return;
    }
    if (job->second.cancelled) {
      end_locked(job_id, "cancelled", nullptr);
    } else {
      emit_locked("job_completed", job_id, {{"result", result}}, payload_bytes);
      release_locked(job_id);
    }
    pump_locked();
  }

  /// The backend refuses the next submit: `job_failed` (`unsupported`,
  /// `job_not_permitted`) without `job_accepted`, then `job_released`.
  void refuse_next_submit() {
    std::lock_guard<std::mutex> lock(table_mutex_);
    refuse_next_ = true;
  }

  // -- What the adapter did. -----------------------------------------------------

  /// Waits until the adapter has sent `count` frames of `kind` and this
  /// sidecar has acted on them.
  [[nodiscard]] std::vector<Json> sent(const std::string& kind, std::size_t count) {
    std::unique_lock<std::mutex> lock(log_mutex_);
    std::vector<Json> of_kind;
    log_changed_.wait_for(lock, kPatience, [&] {
      of_kind.clear();
      for (const Json& header : sent_) {
        if (text_of(header, "kind") == kind) {
          of_kind.push_back(header);
        }
      }
      return of_kind.size() >= count;
    });
    return of_kind;
  }

  /// The payload of the `index`th frame of `kind` that sent() returns.
  [[nodiscard]] std::vector<std::byte> payload(const std::string& kind, std::size_t index) const {
    std::lock_guard<std::mutex> lock(log_mutex_);
    for (std::size_t i = 0; i < sent_.size(); ++i) {
      if (text_of(sent_[i], "kind") == kind && index-- == 0) {
        return payloads_[i];
      }
    }
    return {};
  }

  /// Every frame this sidecar wrote, in order.
  [[nodiscard]] std::vector<Json> written() const {
    std::lock_guard<std::mutex> lock(log_mutex_);
    return written_;
  }

  /// How many frames this sidecar wrote.
  [[nodiscard]] std::size_t written_count() const {
    std::lock_guard<std::mutex> lock(log_mutex_);
    return written_.size();
  }

  /// How many times the adapter asked for `job_id` to be cancelled while this
  /// sidecar held it: by a `job_cancel` that named it or by the release of its
  /// session. Only the first is acted on, so the frames written do not tell.
  [[nodiscard]] std::size_t cancel_requests(std::uint64_t job_id) {
    std::lock_guard<std::mutex> lock(table_mutex_);
    const auto asked = cancel_requests_.find(job_id);
    return asked == cancel_requests_.end() ? 0 : asked->second;
  }

  /// Waits until this sidecar holds its job lock to write a `kind` message.
  [[nodiscard]] bool wait_until_writing(const std::string& kind) {
    std::unique_lock<std::mutex> lock(log_mutex_);
    return log_changed_.wait_for(lock, kPatience, [&] { return writing_kind_ == kind; });
  }

  /// Bytes of the adapter's that arrived and were not read.
  [[nodiscard]] std::size_t unread_bytes() const {
    int queued = 0;
    // NOLINTNEXTLINE(cppcoreguidelines-pro-type-vararg)
    return ::ioctl(fd_, FIONREAD, &queued) == 0 ? static_cast<std::size_t>(queued) : 0;
  }

  /// True once the SidecarProcess terminated this "process".
  [[nodiscard]] bool reaped() const {
    std::lock_guard<std::mutex> lock(log_mutex_);
    return reaped_;
  }

  /// The process is gone, as after a crash: the connection ends and the
  /// handle a SidecarProcess holds reports it dead.
  void die() {
    dead_ = true;
    close();
  }

  /// From now on `health_check` reports `ready: false`, which no runner
  /// does while a backend is loaded.
  void report_not_ready() { unready_ = true; }

  /// True once a write stalled or failed and this sidecar gave the peer up.
  [[nodiscard]] bool gave_up() const {
    std::lock_guard<std::mutex> lock(log_mutex_);
    return gave_up_;
  }

  /// Waits until this sidecar's reader reads end of file: the adapter shut
  /// its side down, unless this sidecar ended the connection itself.
  [[nodiscard]] bool read_end_of_file() {
    std::unique_lock<std::mutex> lock(log_mutex_);
    return log_changed_.wait_for(lock, kPatience, [this] { return end_of_file_; });
  }

  // -- Outside the runner's behaviour. -------------------------------------------

  /// Writes one job message of an admitted job as given, under the job lock;
  /// `job_released` also lets go of the job. For traces the one-lane runner
  /// does not produce and for messages the adapter must fault.
  void emit(const std::string& kind, std::uint64_t job_id, const Json& extra = Json::object(),
            std::size_t payload_bytes = 0) {
    std::lock_guard<std::mutex> lock(table_mutex_);
    // A test that plays a job this sidecar has not admitted yet has raced it.
    EXPECT_TRUE(jobs_.contains(job_id)) << "the scripted sidecar holds no job " << job_id;
    if (kind == "job_released") {
      release_locked(job_id);
    } else {
      emit_locked(kind, job_id, extra, payload_bytes);
    }
  }

  /// Writes any frame.
  void write(const Json& header, std::size_t payload_bytes = 0) {
    write_frame(header.dump(), header, payload_bytes);
  }

  /// Writes a frame whose header is `text`, JSON or not.
  void write_raw(const std::string& text) { write_frame(text, Json(text), 0); }

  /// An `error_event` as the runner answers the message `message_id` with.
  void refuse(const std::string& message_id, const std::string& code, const char* context) {
    write(refusal(Json{{"message_id", message_id}}, "error_event", code, context));
  }

  /// Nothing the adapter sends from now on is read. Returns once the reader
  /// has parked: every frame it read before has been acted on.
  void stop_reading() {
    reading_ = false;
    std::unique_lock<std::mutex> lock(log_mutex_);
    EXPECT_TRUE(log_changed_.wait_for(lock, kPatience, [this] { return parked_; }));
  }

  /// Once this sidecar has answered the next `kind` request from its FIFO it
  /// reads nothing more until resume_reading(): what the adapter sends next
  /// waits unread.
  void stop_reading_after(const std::string& kind) {
    std::lock_guard<std::mutex> lock(log_mutex_);
    stop_after_ = kind;
  }

  /// From now until release_health() a `health_check` is read and not
  /// answered, where the runner's reader answers it at once.
  void hold_health() {
    std::lock_guard<std::mutex> lock(log_mutex_);
    holding_health_ = true;
  }

  /// Answers the health checks held, in arrival order, and holds no more.
  void release_health() {
    std::vector<Json> held;
    {
      std::lock_guard<std::mutex> lock(log_mutex_);
      holding_health_ = false;
      held.swap(held_health_);
    }
    for (const Json& request : held) {
      answer_health(request);
    }
  }

  /// Waits until the reader has parked.
  [[nodiscard]] bool parked() {
    std::unique_lock<std::mutex> lock(log_mutex_);
    return log_changed_.wait_for(lock, kPatience, [this] { return parked_; });
  }

  void resume_reading() {
    reading_ = true;
    std::lock_guard<std::mutex> lock(log_mutex_);
    parked_ = false;
  }

  /// Ends the connection, as a sidecar that died does.
  void close() {
    if (fd_ >= 0) {
      (void)::shutdown(fd_, SHUT_RDWR);
    }
  }

  /// Runs when the SidecarProcess terminates this "process", before this
  /// sidecar closes its end of the socket. Set before the launch.
  std::function<void()> on_terminate;

 private:
  struct Job {
    std::uint64_t session_key = 0;
    std::uint64_t generation = 0;
    bool cancelled = false;
    Json submit = Json::object();
    std::vector<std::byte> payload{};
  };

  static constexpr std::size_t kLaneDepth = 8;
  static constexpr int kSocketBufferBytes = 131'072;

  void finish() {
    stopped_ = true;
    close();
    if (reader_.joinable()) {
      reader_.join();
    }
    if (fd_ >= 0) {
      (void)::close(fd_);
      fd_ = -1;
    }
  }

  // -- Frames. -------------------------------------------------------------------

  Json originated(const std::string& kind, Json fields) {
    fields["schema_version"] = "0.1";
    fields["message_id"] = "s" + std::to_string(++originated_);
    fields["kind"] = kind;
    return fields;
  }

  static Json error_object(const std::string& code, const char* context) {
    Json error{{"schema_version", "0.1"}, {"code", code}, {"message", "m"}};
    if (context != nullptr) {
      error["context"] = context;
    }
    return error;
  }

  // A string field of a header the adapter sent; empty when it has none.
  static std::string text_of(const Json& header, const char* name) {
    if (!header.is_object()) {
      return {};
    }
    const auto field = header.find(name);
    return field != header.end() && field->is_string() ? field->get<std::string>() : std::string{};
  }

  // The kind the runner answers a request of `kind` with.
  static std::string response_kind(const std::string& kind) {
    for (const char* unary :
         {"load_model", "prime", "infer", "infer_async", "cancel", "unload", "health_check"}) {
      if (kind == unary) {
        return kind + "_response";
      }
    }
    return "error_event";
  }

  static Json answer(const Json& request, const std::string& kind) {
    return Json{{"schema_version", "0.1"},
                {"message_id", text_of(request, "message_id")},
                {"kind", kind},
                {"status", "ok"}};
  }

  static Json refusal(const Json& request, const std::string& kind, const std::string& code,
                      const char* context) {
    Json header = answer(request, kind);
    header["status"] = "error";
    header["error"] = error_object(code, context);
    return header;
  }

  void write_frame(const Json& header, std::size_t payload_bytes) {
    write_frame(header.dump(), header, payload_bytes);
  }

  void write_frame(const std::string& text, const Json& header, std::size_t payload_bytes) {
    const auto frame = ipc::encode_frame(
        ipc::SidecarFrame{text, std::vector<std::byte>(payload_bytes, std::byte{0})});
    std::lock_guard<std::mutex> write_lock(write_mutex_);
    {
      std::lock_guard<std::mutex> lock(log_mutex_);
      written_.push_back(header);
    }
    std::size_t done = 0;
    while (!ended_ && done < frame.value().size()) {
      const auto n =
          ::send(fd_, frame.value().data() + done, frame.value().size() - done, MSG_NOSIGNAL);
      if (n > 0) {
        done += static_cast<std::size_t>(n);
        continue;
      }
      struct pollfd waited {};
      waited.fd = fd_;
      waited.events = POLLOUT;
      const bool blocked = n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK);
      const int stall = static_cast<int>(options_.write_stall.count());
      if (!blocked || ::poll(&waited, 1, stall) <= 0 || (waited.revents & POLLOUT) == 0) {
        ended_ = true;
        (void)::shutdown(fd_, SHUT_RDWR);
        std::lock_guard<std::mutex> lock(log_mutex_);
        gave_up_ = true;
      }
    }
  }

  // -- The reader. ---------------------------------------------------------------

  void read_loop() {
    std::vector<std::byte> buffer;
    std::vector<std::byte> chunk(1U << 16U);
    while (!stopped_ && !ended_) {
      std::size_t consumed = 0;
      auto frame = ipc::decode_frame(buffer, &consumed);
      if (frame.has_value()) {
        buffer.erase(buffer.begin(), buffer.begin() + static_cast<std::ptrdiff_t>(consumed));
        Json header = Json::parse(frame.value().json_header, nullptr, false);
        if (!header.is_object()) {
          // Not a message: a frame error, which ends the connection.
          ended_ = true;
          (void)::shutdown(fd_, SHUT_RDWR);
          return;
        }
        route(header, std::move(frame.value().payload));
        continue;
      }
      if (!reading_) {
        {
          std::lock_guard<std::mutex> lock(log_mutex_);
          parked_ = !reading_;
        }
        log_changed_.notify_all();
        std::this_thread::sleep_for(std::chrono::milliseconds{5});
        continue;
      }
      struct pollfd waited {};
      waited.fd = fd_;
      waited.events = POLLIN;
      if (::poll(&waited, 1, 20) <= 0 || !reading_) {
        continue;
      }
      const auto n = ::recv(fd_, chunk.data(), chunk.size(), 0);
      if (n == 0) {
        {
          std::lock_guard<std::mutex> lock(log_mutex_);
          end_of_file_ = true;
        }
        log_changed_.notify_all();
      }
      if (n == 0 || (n < 0 && errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR)) {
        return;
      }
      if (n > 0) {
        buffer.insert(buffer.end(), chunk.begin(), chunk.begin() + n);
      }
    }
  }

  // Acts on one frame of the adapter's, then records it: a test that has
  // seen a frame in sent() knows this sidecar acted on it.
  void route(const Json& header, std::vector<std::byte> payload) {
    act_on(header, payload);
    {
      std::lock_guard<std::mutex> lock(log_mutex_);
      sent_.push_back(header);
      payloads_.push_back(std::move(payload));
    }
    log_changed_.notify_all();
  }

  void act_on(const Json& header, const std::vector<std::byte>& payload) {
    const std::string kind = text_of(header, "kind");
    if (text_of(header, "schema_version") != "0.1" || text_of(header, "message_id").empty() ||
        kind.empty()) {
      // The serving thread refuses it, in arrival order.
      std::lock_guard<std::mutex> lock(table_mutex_);
      lane_.emplace_back(header);
      pump_locked();
      return;
    }
    if (kind == "health_check") {
      {
        std::lock_guard<std::mutex> lock(log_mutex_);
        if (holding_health_) {
          held_health_.push_back(header);
          return;
        }
      }
      answer_health(header);
      return;
    }
    std::lock_guard<std::mutex> lock(table_mutex_);
    const bool job_kind = kind == "job_submit" || kind == "job_cancel" || kind == "session_release";
    if (job_kind && (jobs_open_ || (kind != "job_submit" && !jobs_.empty()))) {
      handle_locked(header, kind, payload);
    } else {
      if (kind == "load_model" || kind == "unload") {
        // It replaces or unloads the backend: no more jobs, waiting ones fail.
        jobs_open_ = false;
        for (const std::uint64_t job_id : waiting_locked()) {
          lane_.erase(std::find(lane_.begin(), lane_.end(), Work{job_id}));
          end_locked(job_id, "unavailable", "backend_unavailable");
        }
      }
      lane_.emplace_back(header);
    }
    pump_locked();
  }

  void answer_health(const Json& request) {
    Json response = answer(request, "health_check_response");
    response["health"] = {{"ready", loaded_.load() && !unready_.load()}};
    write_frame(response, 0);
  }

  // -- Under the job lock. -------------------------------------------------------

  using Work = std::variant<std::uint64_t, Json>;

  std::vector<std::uint64_t> waiting_locked() const {
    std::vector<std::uint64_t> waiting;
    for (const Work& work : lane_) {
      if (const auto* job_id = std::get_if<std::uint64_t>(&work)) {
        waiting.push_back(*job_id);
      }
    }
    return waiting;
  }

  bool job_due_locked() const {
    return running_ == 0 && !lane_.empty() && std::holds_alternative<std::uint64_t>(lane_.front());
  }

  // Answers the unary requests at the head of the FIFO while no job runs.
  void pump_locked() {
    while (running_ == 0 && !lane_.empty() && std::holds_alternative<Json>(lane_.front())) {
      const Json request = std::get<Json>(lane_.front());
      lane_.pop_front();
      write_frame(unary_locked(request), 0);
      std::lock_guard<std::mutex> lock(log_mutex_);
      if (!stop_after_.empty() && text_of(request, "kind") == stop_after_) {
        stop_after_.clear();
        reading_ = false;
      }
    }
    work_.notify_all();
  }

  Json unary_locked(const Json& request) {
    const std::string kind = text_of(request, "kind");
    const bool unnamed = text_of(request, "message_id").empty();
    if (unnamed || text_of(request, "schema_version") != "0.1") {
      // The version is judged first; an answer always carries an id.
      Json named = request;
      if (unnamed) {
        named["message_id"] = "s" + std::to_string(++originated_);
      }
      const bool another_version = text_of(request, "schema_version") != "0.1";
      return refusal(named, response_kind(kind), another_version ? "unsupported" : "config_invalid",
                     nullptr);
    }
    if (kind.empty()) {
      return refusal(request, "error_event", "config_invalid", nullptr);
    }
    if (kind == "prime") {
      return answer(request, "prime_response");
    }
    if (kind == "unload") {
      loaded_ = false;
      return answer(request, "unload_response");
    }
    if (kind != "load_model") {
      return refusal(request, "error_event", "unsupported", nullptr);
    }
    const auto refused = [&](const std::string& code) {
      Json header = refusal(request, "load_model_response", code, nullptr);
      header["error"]["message"] = options_.load_refusal_message;
      if (options_.load_refusal_context.has_value()) {
        header["error"]["context"] = *options_.load_refusal_context;
      }
      return header;
    };
    const Json enabled = request.value("capabilities", Json::array());
    if (request.contains("capabilities") && enabled.empty()) {
      return refused("config_invalid");
    }
    const auto listed = [this](const Json& name) {
      return options_.lists_capabilities && name.is_string() &&
             std::find(options_.capabilities.begin(), options_.capabilities.end(),
                       name.get<std::string>()) != options_.capabilities.end();
    };
    if (!enabled.is_array() || !std::all_of(enabled.begin(), enabled.end(), listed)) {
      return refused("unsupported");
    }
    const bool with_jobs =
        std::find(enabled.begin(), enabled.end(), "speech_jobs_v1") != enabled.end();
    const std::vector<std::string> declared = declared_classes();
    if (with_jobs && declared.empty()) {
      return refused("unsupported");
    }
    loaded_ = true;
    Json response = answer(request, "load_model_response");
    if (with_jobs) {
      jobs_open_ = true;
      response["job_classes"] = declared;
    }
    response.update(options_.load_response_overrides);
    return response;
  }

  // The classes a load with jobs declares: as the runner does for a backend
  // without `vad_frames`, those of its classes that have a lane, in lane order
  // and once each.
  std::vector<std::string> declared_classes() const {
    if (options_.declares_any_job_class) {
      return options_.job_classes;
    }
    std::vector<std::string> declared;
    for (const char* with_a_lane : {"stt_decode", "tts_synthesis"}) {
      if (std::find(options_.job_classes.begin(), options_.job_classes.end(), with_a_lane) !=
          options_.job_classes.end()) {
        declared.emplace_back(with_a_lane);
      }
    }
    return declared;
  }

  // True when a submit's input is of a known type and its `payload_length`
  // is the length of the payload that came with it.
  static bool input_matches(const Json& header, std::size_t payload_bytes) {
    const auto input = header.find("input");
    if (input == header.end() || !input->is_object()) {
      return false;
    }
    const std::string type = input->value("type", "");
    const auto length = input->find("payload_length");
    return (type == "audio_frames" || type == "text_segment" || type == "vad_frames") &&
           length != input->end() && length->is_number_unsigned() &&
           length->get<std::size_t>() == payload_bytes;
  }

  void handle_locked(const Json& header, const std::string& kind,
                     const std::vector<std::byte>& payload) {
    const auto field = [&](const char* name) {
      const auto it = header.find(name);
      return it != header.end() && it->is_number_unsigned() ? it->get<std::uint64_t>() : 0;
    };
    const std::uint64_t job_id = field("job_id");
    const Job named{field("session_key"), field("generation")};
    const auto job = jobs_.find(job_id);
    if (named.session_key == 0 || named.generation == 0 ||
        (kind != "session_release" && job_id == 0) ||
        (kind == "job_submit" && !input_matches(header, payload.size()))) {
      write_frame(refusal(header, "error_event", "config_invalid", nullptr), 0);
    } else if (kind == "session_release") {
      releasing_.insert({named.session_key, named.generation});
      for (const auto& [id, held] : std::map<std::uint64_t, Job>(jobs_)) {
        if (held.session_key == named.session_key && held.generation == named.generation) {
          cancel_locked(id);
        }
      }
      settle_locked(named);
    } else if (kind == "job_cancel") {
      if (job == jobs_.end()) {
        return;
      }
      if (job->second.session_key != named.session_key ||
          job->second.generation != named.generation) {
        write_frame(refusal(header, "error_event", "config_invalid", "identity_mismatch"), 0);
      } else {
        cancel_locked(job_id);
      }
    } else if (job != jobs_.end()) {
      // Not job messages: they would land in the live job's stream.
      write_frame(refusal(header, "error_event", "config_invalid", "duplicate_job_id"), 0);
    } else {
      jobs_.emplace(job_id, Job{named.session_key, named.generation, false, header, payload});
      const std::vector<std::string> declared = declared_classes();
      const bool runs = std::find(declared.begin(), declared.end(),
                                  header.value("job_class", "")) != declared.end();
      if (!runs) {
        end_locked(job_id, "unsupported", "job_class_unsupported");
      } else if (std::exchange(refuse_next_, false)) {
        end_locked(job_id, "unsupported", "job_not_permitted");
      } else if (releasing_.contains({named.session_key, named.generation})) {
        end_locked(job_id, "not_ready", "session_releasing");
      } else if (waiting_locked().size() >= kLaneDepth) {
        end_locked(job_id, "resource_exhausted", "job_capacity_exhausted");
      } else {
        emit_locked("job_accepted", job_id, Json::object(), 0);
        lane_.emplace_back(job_id);
      }
    }
  }

  void cancel_locked(std::uint64_t job_id) {
    Job& job = jobs_.at(job_id);
    ++cancel_requests_[job_id];
    if (job.cancelled) {
      return;
    }
    job.cancelled = true;
    emit_locked("job_cancel_acknowledged", job_id, Json::object(), 0);
    // The running job ends when its backend call returns; a waiting one now.
    if (job_id != running_) {
      lane_.erase(std::find(lane_.begin(), lane_.end(), Work{job_id}));
      end_locked(job_id, "cancelled", nullptr);
    }
  }

  void end_locked(std::uint64_t job_id, const std::string& code, const char* context) {
    emit_locked("job_failed", job_id, {{"error", error_object(code, context)}}, 0);
    release_locked(job_id);
  }

  void release_locked(std::uint64_t job_id) {
    const auto job = jobs_.find(job_id);
    if (job == jobs_.end()) {
      return;
    }
    const Job released = job->second;
    emit_locked("job_released", job_id, Json::object(), 0);
    jobs_.erase(job_id);
    settle_locked(released);
  }

  // Sends `session_released` and forgets the session once none of its jobs is left.
  void settle_locked(const Job& session) {
    const std::pair<std::uint64_t, std::uint64_t> key{session.session_key, session.generation};
    const bool held = std::any_of(jobs_.begin(), jobs_.end(), [&](const auto& entry) {
      return entry.second.session_key == key.first && entry.second.generation == key.second;
    });
    if (held || releasing_.erase(key) == 0) {
      return;
    }
    write_frame(
        originated("session_released", {{"session_key", key.first}, {"generation", key.second}}),
        0);
  }

  void emit_locked(const std::string& kind, std::uint64_t job_id, const Json& extra,
                   std::size_t payload_bytes) {
    const auto job = jobs_.find(job_id);
    if (job == jobs_.end()) {
      return;
    }
    Json fields = extra;
    fields["job_id"] = job_id;
    fields["session_key"] = job->second.session_key;
    fields["generation"] = job->second.generation;
    {
      std::lock_guard<std::mutex> lock(log_mutex_);
      writing_kind_ = kind;
    }
    log_changed_.notify_all();
    write_frame(originated(kind, std::move(fields)), payload_bytes);
    std::lock_guard<std::mutex> lock(log_mutex_);
    writing_kind_.clear();
  }

  Options options_;
  int fd_ = -1;
  std::thread reader_;
  std::atomic<bool> stopped_{false};
  std::atomic<bool> ended_{false};
  std::atomic<bool> reading_{true};
  std::atomic<bool> loaded_{false};
  std::atomic<bool> dead_{false};
  std::atomic<bool> unready_{false};
  std::atomic<std::uint64_t> originated_{0};

  // Lock order: the job lock, then the write lock, then the log lock.
  std::mutex table_mutex_;
  std::condition_variable work_;
  std::map<std::uint64_t, Job> jobs_;
  std::deque<Work> lane_;
  std::uint64_t running_ = 0;
  std::set<std::pair<std::uint64_t, std::uint64_t>> releasing_;
  std::map<std::uint64_t, std::size_t> cancel_requests_;
  bool jobs_open_ = false;
  bool refuse_next_ = false;

  std::mutex write_mutex_;

  mutable std::mutex log_mutex_;
  std::condition_variable log_changed_;
  std::vector<Json> sent_;
  std::vector<std::vector<std::byte>> payloads_;
  std::vector<Json> written_;
  std::string writing_kind_;
  std::string stop_after_;
  bool holding_health_ = false;
  std::vector<Json> held_health_;
  bool reaped_ = false;
  bool gave_up_ = false;
  bool parked_ = false;
  bool end_of_file_ = false;
};

}  // namespace tensorplate::testing
