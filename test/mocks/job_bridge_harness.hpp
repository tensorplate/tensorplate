// SPDX-License-Identifier: Apache-2.0
//
// The Python/PyTorch adapter's session with its job bridge, for tests: the
// pair the adapter's factory builds over a sidecar the test supplies, a
// thread that plays a scripted sidecar's backend, and the waits that tell a
// test nothing is on its way any more.

#pragma once

#include <gtest/gtest.h>

#include <chrono>
#include <condition_variable>
#include <cstddef>
#include <cstdint>
#include <functional>
#include <memory>
#include <mutex>
#include <nlohmann/json.hpp>
#include <string>
#include <thread>
#include <utility>

#include "tensorplate/backend/bounded_job_bridge.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/result.hpp"

#include "adapters/python_pytorch/job_channel.hpp"
#include "adapters/python_pytorch/job_wire.hpp"
#include "adapters/python_pytorch/python_pytorch_session.hpp"
#include "adapters/python_pytorch/sidecar_process.hpp"
#include "fixture_job_bridge.hpp"
#include "job_channel_harness.hpp"
#include "scripted_sidecar.hpp"

namespace tensorplate::testing {

/// The backend of a ScriptedSidecar played by a thread. It takes each job the
/// sidecar admits and ends it with the fixture bridge's result for its submit
/// (fixture_job_bridge.hpp): the word `hello` over the decoded window, or
/// kFixtureSpeechBytesPerTextByte bytes of silence for each byte of text.
class ScriptedBackend {
 public:
  using Json = nlohmann::json;

  explicit ScriptedBackend(ScriptedSidecar& sidecar)
      : sidecar_(sidecar), thread_([this] { run(); }) {}

  ~ScriptedBackend() {
    {
      const std::lock_guard<std::mutex> lock(mutex_);
      stopping_ = true;
    }
    changed_.notify_all();
    thread_.join();
  }

  ScriptedBackend(const ScriptedBackend&) = delete;
  ScriptedBackend& operator=(const ScriptedBackend&) = delete;
  ScriptedBackend(ScriptedBackend&&) = delete;
  ScriptedBackend& operator=(ScriptedBackend&&) = delete;

  /// From now on a job it has taken is not ended.
  void hold() { set(holding_, true); }

  /// Ends the job it holds, and holds none after it.
  void release() { set(holding_, false); }

  /// The session is about to unload. A held job ends once the sidecar has
  /// acted on the `unload` and not before: ended earlier, the jobs behind it
  /// could start, and the unload would find none waiting.
  void end_a_held_job_at_the_unload() { set(unloading_, true); }

  /// The sidecar is gone: nothing is ended from here on.
  void lose() { set(lost_, true); }

  /// True when it has nothing to do: it holds the job the sidecar runs, no
  /// job is due, or the sidecar is gone.
  [[nodiscard]] bool idle() {
    const ScriptedSidecar::Lane lane = sidecar_.lane();
    const std::lock_guard<std::mutex> lock(mutex_);
    if (lost_) {
      return true;
    }
    return lane.running == 0 ? !lane.job_due : parked_ && holding_ && !unloading_;
  }

 private:
  // How often the thread looks up from a sidecar that admits no job.
  static constexpr std::chrono::milliseconds kStep{20};

  void set(bool& flag, bool value) {
    {
      const std::lock_guard<std::mutex> lock(mutex_);
      flag = value;
    }
    changed_.notify_all();
  }

  void run() {
    for (;;) {
      const ScriptedSidecar::TakenJob job = sidecar_.take(kStep);
      std::unique_lock<std::mutex> lock(mutex_);
      if (stopping_) {
        return;
      }
      if (job.job_id == 0) {
        continue;
      }
      parked_ = true;
      changed_.wait(lock, [this] { return !holding_ || unloading_ || lost_ || stopping_; });
      parked_ = false;
      if (stopping_) {
        return;
      }
      if (lost_) {
        continue;
      }
      const bool at_the_unload = holding_ && unloading_;
      lock.unlock();
      if (at_the_unload) {
        (void)sidecar_.sent("unload", 1);
      }
      end(job);
    }
  }

  void end(const ScriptedSidecar::TakenJob& job) {
    const Json& input = job.submit.at("input");
    if (input.at("type") == "text_segment") {
      const std::size_t bytes = kFixtureSpeechBytesPerTextByte * job.payload_bytes;
      sidecar_.complete(
          job.job_id,
          Json{{"type", "audio_chunk"},
               {"format", {{"encoding", "pcm_s16le"}, {"sample_rate_hz", 24000}, {"channels", 1}}},
               {"clipped_samples", 0},
               {"payload_length", bytes}},
          bytes);
      return;
    }
    const std::uint64_t start = input.at("start_sample").get<std::uint64_t>();
    const Json word{{"text", "hello"},
                    {"token_begin", 0},
                    {"token_end", 1},
                    {"start_sample", start},
                    {"end_sample", start + job.payload_bytes / 4}};
    sidecar_.complete(job.job_id, Json{{"type", "transcript"},
                                       {"text", "hello"},
                                       {"tokens", {1}},
                                       {"words", Json::array({word})}});
  }

  ScriptedSidecar& sidecar_;
  std::mutex mutex_;
  std::condition_variable changed_;
  bool holding_ = false;
  bool unloading_ = false;
  bool lost_ = false;
  bool stopping_ = false;
  /// The thread has a job in hand and waits to end it.
  bool parked_ = false;
  std::thread thread_;
};

using SettleClock = std::chrono::steady_clock;

/// Waits until `reached` holds; false when `deadline` came first.
[[nodiscard]] inline bool wait_until(SettleClock::time_point deadline,
                                     const std::function<bool()>& reached) {
  while (!reached()) {
    if (SettleClock::now() >= deadline) {
      return false;
    }
    std::this_thread::sleep_for(std::chrono::milliseconds{1});
  }
  return true;
}

/// A `health_check` through the channel's exchange slot, which is free unless
/// a lifecycle call of the session is running. The sidecar answers it in
/// arrival order and the channel's reader hands the answer over only after
/// the callbacks of every frame before it have returned.
[[nodiscard]] inline Result<void> round_trip(adapters::python_pytorch::JobChannel& channel,
                                             SettleClock::time_point deadline) {
  const std::string id = adapters::python_pytorch::next_message_id();
  const nlohmann::json header{
      {"schema_version", "0.1"}, {"message_id", id}, {"kind", "health_check"}};
  auto answered = channel.exchange({header.dump(), {}}, id, deadline);
  if (!answered.has_value()) {
    return unexpected(answered.error());
  }
  return Result<void>{};
}

/// What is left to wait for when a round trip failed: nothing after a
/// timeout; else the connection is gone or was never there, and the last
/// batch its end or its stop decided has to return.
[[nodiscard]] inline bool settled_without_a_connection(
    const adapters::python_pytorch::JobChannel& channel, const Error& failure,
    SettleClock::time_point deadline) {
  return failure.code != Error::Code::Timeout &&
         wait_until(deadline, [&] { return channel.backlog().owed_callbacks == 0; });
}

/// True once nothing is on its way between `channel`, a scripted sidecar and
/// its backend, as far as held jobs allow: two round trips in a row, each
/// begun with the backend idle, found that the sidecar wrote nothing but
/// their answers. One is not enough: a callback that was running when the
/// first was queued may have queued a frame behind it.
[[nodiscard]] inline bool settled_over(adapters::python_pytorch::JobChannel& channel,
                                       ScriptedSidecar& sidecar, ScriptedBackend& backend,
                                       std::chrono::milliseconds patience) {
  const auto deadline = SettleClock::now() + patience;
  std::size_t written = sidecar.written_count();
  for (int quiet = 0; quiet < 2;) {
    if (!wait_until(deadline, [&] { return backend.idle(); })) {
      return false;
    }
    const auto answered = round_trip(channel, deadline);
    if (!answered.has_value()) {
      return settled_without_a_connection(channel, answered.error(), deadline);
    }
    const std::size_t now = sidecar.written_count();
    quiet = now == written + 1 ? quiet + 1 : 0;
    written = now;
  }
  return true;
}

/// True once `channel` owes nothing: no job without `released`, no session
/// release without its acknowledgement and no sink call still to return. For
/// a backend the test cannot see into and does not hold, whose jobs all end
/// by themselves.
[[nodiscard]] inline bool settled_by_backlog(adapters::python_pytorch::JobChannel& channel,
                                             std::chrono::milliseconds patience) {
  const auto deadline = SettleClock::now() + patience;
  for (;;) {
    const auto answered = round_trip(channel, deadline);
    if (!answered.has_value()) {
      return settled_without_a_connection(channel, answered.error(), deadline);
    }
    if (channel.backlog() == adapters::python_pytorch::JobChannelBacklog{}) {
      return true;
    }
    if (SettleClock::now() >= deadline) {
      return false;
    }
    std::this_thread::sleep_for(std::chrono::milliseconds{1});
  }
}

/// The session, its bridge and the channel under both, with a recording
/// sink registered through the bridge. A test's jobs go through the bridge.
class JobBridgeHarness : public JobBench {
 public:
  JobBridgeHarness(adapters::python_pytorch::PythonPytorchConfig config,
                   const adapters::python_pytorch::SidecarLauncher& launcher) {
    ExecutionSessionRuntimeHooks hooks{};
    hooks.buffer_manager = manager.get();
    auto built = adapters::python_pytorch::make_python_pytorch_session_with_bridge(
        hooks, std::move(config), launcher);
    session = std::move(built.session);
    bridge = std::move(built.bridge);
    channel = std::move(built.channel);
    bind(*channel);
    EXPECT_TRUE(bridge->set_event_sink(sink).has_value());
  }

  /// Destroys the session, which stops the channel, before the sink is read.
  ~JobBridgeHarness() {
    session.reset();
    check_sink();
  }

  JobBridgeHarness(const JobBridgeHarness&) = delete;
  JobBridgeHarness& operator=(const JobBridgeHarness&) = delete;
  JobBridgeHarness(JobBridgeHarness&&) = delete;
  JobBridgeHarness& operator=(JobBridgeHarness&&) = delete;

  [[nodiscard]] Result<void> submit(const JobRequest& request) {
    sink->expect_job(request);
    return bridge->submit(request);
  }

  [[nodiscard]] Result<void> cancel(std::uint64_t job_id) {
    sink->expect_cancel(job_id);
    return bridge->cancel(job_id);
  }

  [[nodiscard]] Result<void> release(std::uint64_t session_key, std::uint64_t generation = 1) {
    sink->expect_session_release(session_key);
    return bridge->release_session(session_key, generation);
  }

  std::unique_ptr<ExecutionSession> session;
  std::shared_ptr<BoundedJobBridge> bridge;
  std::shared_ptr<adapters::python_pytorch::JobChannel> channel;
};

}  // namespace tensorplate::testing
