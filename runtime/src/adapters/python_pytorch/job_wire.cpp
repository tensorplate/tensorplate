// SPDX-License-Identifier: Apache-2.0

#include "job_wire.hpp"

#include <cstddef>
#include <cstdint>
#include <string>
#include <string_view>
#include <vector>

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/result.hpp"
#include "tensorplate/ipc/sidecar_codec.hpp"

namespace tensorplate::adapters::python_pytorch {

std::size_t job_submit_payload_size(const JobRequest& /*request*/) noexcept {
  return 0;
}

Result<std::string> encode_job_submit(const JobRequest& /*request*/,
                                      std::string_view /*message_id*/) {
  return unexpected(Error::Code::Unsupported, "not implemented");
}

Result<std::string> encode_job_cancel(const JobIdentity& /*identity*/,
                                      std::string_view /*message_id*/) {
  return unexpected(Error::Code::Unsupported, "not implemented");
}

Result<std::string> encode_session_release(std::uint64_t /*session_key*/,
                                           std::uint64_t /*generation*/,
                                           std::string_view /*message_id*/) {
  return unexpected(Error::Code::Unsupported, "not implemented");
}

Result<bool> offers_speech_jobs(std::string_view /*ready_event_header*/) {
  return unexpected(Error::Code::Unsupported, "not implemented");
}

Result<std::vector<JobClass>> declared_job_classes(
    std::string_view /*load_model_response_header*/) {
  return unexpected(Error::Code::Unsupported, "not implemented");
}

SidecarJobMessage decode_sidecar_message(const ipc::SidecarFrame& /*frame*/,
                                         const PcmPublisher& /*publish_pcm*/) {
  return NotJobMessage{};
}

}  // namespace tensorplate::adapters::python_pytorch
