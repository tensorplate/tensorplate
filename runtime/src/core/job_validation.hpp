// SPDX-License-Identifier: Apache-2.0
//
// Error helpers shared by the job value objects. Every failure carries a
// stable snake_case reason in Error::context.

#pragma once

#include <string>
#include <string_view>
#include <utility>

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate::internal {

/// ConfigInvalid error for a value-object construction failure.
[[nodiscard]] inline Error invalid_error(std::string_view reason, std::string message) {
  return Error::make(Error::Code::ConfigInvalid, std::move(message), std::string(reason));
}

[[nodiscard]] inline Unexpected invalid(std::string_view reason, std::string message) {
  return unexpected(invalid_error(reason, std::move(message)));
}

/// InferenceFailed error for an event a job's event order refuses.
[[nodiscard]] inline Unexpected refused(std::string_view reason, std::string message) {
  return unexpected(
      Error::make(Error::Code::InferenceFailed, std::move(message), std::string(reason)));
}

/// Nonzero checks shared by JobRequest and JobEvent; defined in
/// job_request.cpp.
[[nodiscard]] Result<void> validate_job_identity(const JobIdentity& identity);

/// Checks a PCM payload's format against `required` and its window against
/// its buffer, in the order JobRequest::create documents ("audio_format_
/// unsupported" through "pcm_misaligned"). `payload` names the payload type in
/// messages. Shared by the request and result payloads; defined in
/// job_request.cpp.
[[nodiscard]] Result<void> validate_pcm(const AudioFormat& format, const AudioFormat& required,
                                        const PcmWindow& pcm, std::string_view payload);

}  // namespace tensorplate::internal
