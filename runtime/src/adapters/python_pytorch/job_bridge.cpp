// SPDX-License-Identifier: Apache-2.0

#include "job_bridge.hpp"

#include <utility>

#include "tensorplate/core/error.hpp"

namespace tensorplate::adapters::python_pytorch {

JobBridge::JobBridge(std::shared_ptr<JobChannel> channel, std::chrono::milliseconds health_timeout)
    : channel_(std::move(channel)), health_timeout_(health_timeout) {}

Result<void> JobBridge::set_event_sink(std::shared_ptr<JobEventSink> sink) {
  return channel_->set_event_sink(std::move(sink));
}

Result<void> JobBridge::submit(JobRequest request) {
  return channel_->submit(request);
}

Result<void> JobBridge::cancel(std::uint64_t job_id) {
  return channel_->cancel(job_id);
}

Result<void> JobBridge::release_session(std::uint64_t session_key, std::uint64_t generation) {
  if (session_key == 0) {
    return unexpected(
        Error::make(Error::Code::ConfigInvalid, "session key is zero", "session_key_zero"));
  }
  if (generation == 0) {
    return unexpected(
        Error::make(Error::Code::ConfigInvalid, "generation is zero", "generation_zero"));
  }
  return channel_->release_session(session_key, generation);
}

Result<void> JobBridge::health() {
  return channel_->probe_health(JobChannel::TimePoint::clock::now() + health_timeout_);
}

}  // namespace tensorplate::adapters::python_pytorch
