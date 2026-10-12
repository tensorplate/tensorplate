// SPDX-License-Identifier: Apache-2.0
//
// The BoundedJobBridge of the Python/PyTorch adapter: the job side of a
// session's JobChannel behind the public interface
// (tensorplate/backend/bounded_job_bridge.hpp). It refuses a zero session key
// and a zero generation ahead of everything the channel checks, gives
// health() its deadline, and passes every other call through: job_channel.hpp
// says what each one does on the socket.
//
// The bridge holds the channel, not the session, and may outlive the session.
// It starts, stops and waits for nothing of the session's lifecycle, and it is
// not told of an unload: an unload cancels no job. The sidecar fails the jobs
// that wait when the unload arrives ("session_unloaded") and lets the one it
// runs end by itself; the caller cancels what it does not want to wait for
// before it unloads (python_pytorch_session.hpp has the bound).
//
// Internal: nothing outside the adapter includes this header.

#pragma once

#include <chrono>
#include <cstdint>
#include <memory>

#include "tensorplate/backend/bounded_job_bridge.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/result.hpp"

#include "job_channel.hpp"

namespace tensorplate::adapters::python_pytorch {

class JobBridge final : public BoundedJobBridge {
 public:
  /// @param health_timeout  longest health() waits for the sidecar's answer.
  JobBridge(std::shared_ptr<JobChannel> channel, std::chrono::milliseconds health_timeout);

  [[nodiscard]] Result<void> set_event_sink(std::shared_ptr<JobEventSink> sink) override;

  /// JobChannel::submit(): the interface's refusals in its order, then the
  /// channel's own for a request the wire or the buffer manager cannot carry.
  [[nodiscard]] Result<void> submit(JobRequest request) override;

  [[nodiscard]] Result<void> cancel(std::uint64_t job_id) override;

  /// A zero key or generation is refused first, whatever the channel's state;
  /// then JobChannel::release_session(). After "backend_unavailable" the
  /// channel counts the key as released: a job for it is refused
  /// ("session_releasing") on every later load of the session. The one
  /// exception is a release made before the session's first load, which is
  /// not recorded.
  [[nodiscard]] Result<void> release_session(std::uint64_t session_key,
                                             std::uint64_t generation) override;

  /// JobChannel::probe_health() with `health_timeout` from now as its deadline.
  [[nodiscard]] Result<void> health() override;

 private:
  const std::shared_ptr<JobChannel> channel_;
  const std::chrono::milliseconds health_timeout_;
};

}  // namespace tensorplate::adapters::python_pytorch
