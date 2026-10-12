// SPDX-License-Identifier: Apache-2.0
//
// The BoundedJobBridge of the Python/PyTorch adapter: the job side of a
// session's JobChannel (job_channel.hpp) behind the public interface. It holds
// the channel, not the session, may outlive the session, and takes no part in
// the session's lifecycle: an unload cancels no job.
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

  /// JobChannel::submit(): the interface's refusals in its order. The last,
  /// for a request this backend cannot carry, is Error::Code::ConfigInvalid
  /// with "wire_integer_out_of_range" (an identity or input field above
  /// kMaxWireInteger) or "pcm_buffer_invalid" (a window the buffer manager
  /// does not hold). ipc::encode_frame()'s refusal, which has no reason, is
  /// beyond what a JobRequest can hold.
  [[nodiscard]] Result<void> submit(JobRequest request) override;

  [[nodiscard]] Result<void> cancel(std::uint64_t job_id) override;

  /// A zero key or generation is refused first, whatever the channel's state;
  /// then JobChannel::release_session(). After "backend_unavailable" the
  /// channel counts the key as released: a job for it is refused
  /// ("session_releasing") on every later load of the session. The one
  /// exception is a channel that was never started, which records nothing.
  /// Beyond the interface's list: Error::Code::ConfigInvalid,
  /// "wire_integer_out_of_range" for a key or generation above
  /// kMaxWireInteger, and Error::Code::ResourceExhausted,
  /// "session_release_capacity_exhausted" while kMaxPendingSessionReleases
  /// releases are unacknowledged.
  [[nodiscard]] Result<void> release_session(std::uint64_t session_key,
                                             std::uint64_t generation) override;

  /// JobChannel::probe_health() with `health_timeout` from now as its deadline.
  [[nodiscard]] Result<void> health() override;

 private:
  const std::shared_ptr<JobChannel> channel_;
  const std::chrono::milliseconds health_timeout_;
};

}  // namespace tensorplate::adapters::python_pytorch
