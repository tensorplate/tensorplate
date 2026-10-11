// SPDX-License-Identifier: Apache-2.0
//
// A job channel on a live sidecar connection, for tests: a recording sink,
// requests whose PCM is in the buffer manager, waits that fail instead of
// hanging, and the load handshake a session makes before it starts the
// channel.

#pragma once

#include <gtest/gtest.h>
#include <sys/socket.h>

#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstddef>
#include <cstdint>
#include <functional>
#include <memory>
#include <mutex>
#include <nlohmann/json.hpp>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/job_event.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/result.hpp"
#include "tensorplate/ipc/sidecar_codec.hpp"

#include "adapters/python_pytorch/job_channel.hpp"
#include "adapters/python_pytorch/job_wire.hpp"
#include "adapters/python_pytorch/sidecar_frame_io.hpp"
#include "adapters/python_pytorch/sidecar_process.hpp"
#include "recording_job_event_sink.hpp"

namespace tensorplate::testing {

/// What a test of jobs needs around a channel, whoever starts and stops it.
class JobBench {
 public:
  using Json = nlohmann::json;
  using Kinds = std::vector<JobEventKind>;

  /// How long a wait lasts before the test fails.
  static constexpr std::chrono::seconds kPatience{30};

  JobBench() : manager(make_manager()), sink(std::make_shared<RecordingJobEventSink>()) {
    sink->set_callback_hook([this](const JobEvent& event) {
      notify();
      if (on_event) {
        on_event(event);
      }
    });
    sink->set_session_release_hook([this](std::uint64_t session_key, std::uint64_t generation) {
      notify();
      if (on_session_release) {
        on_session_release(session_key, generation);
      }
    });
  }

  JobBench(const JobBench&) = delete;
  JobBench& operator=(const JobBench&) = delete;
  JobBench(JobBench&&) = delete;
  JobBench& operator=(JobBench&&) = delete;

  /// A unary request through the channel's slot; the header of its response.
  [[nodiscard]] Result<Json> unary(
      const std::string& kind,
      std::chrono::milliseconds timeout =
          std::chrono::duration_cast<std::chrono::milliseconds>(kPatience)) {
    const std::string id = adapters::python_pytorch::next_message_id();
    const Json header{{"schema_version", "0.1"}, {"message_id", id}, {"kind", kind}};
    auto response =
        jobs_->exchange({header.dump(), {}}, id,
                        adapters::python_pytorch::JobChannel::TimePoint::clock::now() + timeout);
    if (!response.has_value()) {
      return unexpected(response.error());
    }
    return Json::parse(response.value().json_header);
  }

  // -- Requests, recorded with the sink before the channel sees them. ------------

  [[nodiscard]] JobRequest stt(std::uint64_t job_id, std::uint64_t session_key,
                               std::size_t pcm_bytes = 3'200, std::uint32_t progress_limit = 0) {
    const BufferRef buffer = manager->allocate(pcm_bytes).value();
    return JobRequest::create(JobIdentity{job_id, session_key, 1}, JobClass::SttDecode,
                              AudioFrames{kJobInputAudioFormat, PcmWindow{buffer, 0, pcm_bytes}, 0},
                              JobOptions{.language = "en"}, progress_limit)
        .value();
  }

  [[nodiscard]] static JobRequest tts(std::uint64_t job_id, std::uint64_t session_key,
                                      std::string text = "hello there.") {
    return JobRequest::create(
               JobIdentity{job_id, session_key, 1}, JobClass::TtsSynthesis,
               TextSegment{std::move(text)},
               JobOptions{.language = "en-US", .voice = "af_heart", .speed_milli = 1000})
        .value();
  }

  [[nodiscard]] Result<void> submit(const JobRequest& request) {
    sink->expect_job(request);
    return jobs_->submit(request);
  }

  [[nodiscard]] Result<void> cancel(std::uint64_t job_id) {
    sink->expect_cancel(job_id);
    return jobs_->cancel(job_id);
  }

  [[nodiscard]] Result<void> release(std::uint64_t session_key) {
    sink->expect_session_release(session_key);
    return jobs_->release_session(session_key, 1);
  }

  // -- Waits. --------------------------------------------------------------------

  /// Waits until `reached` holds; false when it never did.
  [[nodiscard]] bool wait(const std::function<bool()>& reached) {
    const auto limit = std::chrono::steady_clock::now() + kPatience;
    std::unique_lock<std::mutex> lock(mutex_);
    // Polled as well: not every condition a test waits for is a sink event.
    while (!reached()) {
      if (std::chrono::steady_clock::now() >= limit) {
        return false;
      }
      changed_.wait_for(lock, std::chrono::milliseconds{2});
    }
    return true;
  }

  /// Waits until the sink has exactly `kinds` for the job.
  [[nodiscard]] bool wait_kinds(std::uint64_t job_id, const Kinds& kinds) {
    return wait([&] { return sink->kinds_for(job_id) == kinds; });
  }

  /// Bytes the buffer manager has handed out and not taken back.
  [[nodiscard]] std::size_t bytes_in_use() const { return manager->accounting().in_use_bytes; }

  std::unique_ptr<BufferManager> manager;
  std::shared_ptr<RecordingJobEventSink> sink;
  /// Runs inside every job callback, after the event is recorded. Both hooks
  /// are read on the channel's reader without a lock: a test sets them
  /// before it submits its first job.
  std::function<void(const JobEvent&)> on_event;
  /// Runs inside every session release callback, after it is recorded.
  std::function<void(std::uint64_t, std::uint64_t)> on_session_release;
  /// What the sink has reported once the harness is done; none unless a test
  /// names one.
  std::vector<std::string> expected_violations;

 protected:
  ~JobBench() = default;

  void bind(adapters::python_pytorch::JobChannel& channel) { jobs_ = &channel; }

  /// For the harness's destructor, once its channel has stopped: by then
  /// every delivery has been made, the stop's own among them.
  void check_sink() const { EXPECT_EQ(sink->violations(), expected_violations); }

 private:
  static std::unique_ptr<BufferManager> make_manager() {
    BufferManagerConfig config;
    config.pool_name = "job_channel_test";
    config.capacity_bytes = 16U << 20U;
    config.max_buffer_bytes = 2U << 20U;
    return BufferManager::create(config).value();
  }

  void notify() {
    { std::lock_guard<std::mutex> lock(mutex_); }
    changed_.notify_all();
  }

  adapters::python_pytorch::JobChannel* jobs_ = nullptr;
  std::mutex mutex_;
  std::condition_variable changed_;
};

/// A channel the test starts and stops itself, as a session would.
class JobChannelHarness : public JobBench {
 public:
  /// SO_SNDBUF of the adapter's socket.
  static constexpr int kSocketBufferBytes = 131'072;

  /// @param with_manager  false gives the channel no buffer manager.
  explicit JobChannelHarness(std::chrono::milliseconds write_timeout = std::chrono::seconds{5},
                             bool with_manager = true)
      : channel(with_manager ? manager.get() : nullptr, write_timeout) {
    bind(channel);
  }

  ~JobChannelHarness() {
    stop(adapters::python_pytorch::kSessionUnloaded);
    check_sink();
  }

  JobChannelHarness(const JobChannelHarness&) = delete;
  JobChannelHarness& operator=(const JobChannelHarness&) = delete;
  JobChannelHarness(JobChannelHarness&&) = delete;
  JobChannelHarness& operator=(JobChannelHarness&&) = delete;

  /// Starts the sidecar, takes its `ready_event`, loads `artifact_path` with
  /// the job capability enabled and starts the channel on the socket.
  /// @return the header of the load response.
  [[nodiscard]] Result<Json> connect(const adapters::python_pytorch::SidecarLauncher& launcher,
                                     adapters::python_pytorch::SidecarLaunchRequest request = {},
                                     const std::string& artifact_path = "/dev/null") {
    namespace adapter = adapters::python_pytorch;
    const auto deadline = adapter::SidecarProcess::Clock::now() + kPatience;
    auto started = adapter::SidecarProcess::start(request, launcher, deadline);
    if (!started.has_value()) {
      return unexpected(started.error());
    }
    process = std::move(started).value();
    // A frame with the largest window is larger than the socket holds,
    // whatever the host's default is.
    const int held = kSocketBufferBytes;
    (void)::setsockopt(process->socket().fd(), SOL_SOCKET, SO_SNDBUF, &held, sizeof(held));
    auto ready = adapter::read_frame(process->socket(), deadline);
    if (!ready.has_value()) {
      return unexpected(ready.error());
    }
    const auto offered = adapter::offers_speech_jobs(ready.value().json_header);
    if (!offered.has_value() || !offered.value()) {
      return unexpected(Error::Code::Unsupported, "the sidecar does not offer speech jobs");
    }
    const Json load{{"schema_version", "0.1"},
                    {"message_id", adapter::next_message_id()},
                    {"kind", "load_model"},
                    {"model_spec",
                     {{"schema_version", "0.1"},
                      {"model_id", "speech"},
                      {"model_class", "speech"},
                      {"artifact_path", artifact_path},
                      {"backend_hint", "python_pytorch"}}},
                    {"capabilities", {std::string(adapter::kSpeechJobsCapability)}}};
    if (auto written = adapter::write_frame(process->socket(), {load.dump(), {}}, deadline);
        !written.has_value()) {
      return unexpected(written.error());
    }
    auto loaded = adapter::read_frame(process->socket(), deadline);
    if (!loaded.has_value()) {
      return unexpected(loaded.error());
    }
    Json response = Json::parse(loaded.value().json_header);
    if (response.value("status", "") != "ok") {
      return unexpected(Error::Code::LoadFailed, response.dump());
    }
    const auto classes = adapter::declared_job_classes(loaded.value().json_header);
    if (!classes.has_value()) {
      return unexpected(classes.error());
    }
    if (auto begun = channel.start(process->socket(), classes.value()); !begun.has_value()) {
      return unexpected(begun.error());
    }
    return response;
  }

  /// Ends the channel and reaps the sidecar, as a session's unload does.
  void stop(std::string_view reason) {
    channel.stop(reason, [this] {
      if (process) {
        process->shutdown();
        reaped = true;
      }
    });
  }

  std::unique_ptr<adapters::python_pytorch::SidecarProcess> process;
  /// True once stop() has shut the sidecar process down.
  std::atomic<bool> reaped{false};
  adapters::python_pytorch::JobChannel channel;
};

}  // namespace tensorplate::testing
