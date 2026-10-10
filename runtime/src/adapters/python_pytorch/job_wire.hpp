// SPDX-License-Identifier: Apache-2.0
//
// The `speech_jobs_v1` messages of the sidecar IPC
// (protocol/schemas/python_pytorch_ipc.json) as the adapter writes and reads
// them: headers built from the job value objects, and sidecar frames read
// back into job events. No socket, no thread and no buffer manager.
//
// The decoder reads the fields it uses by name and ignores any other, as the
// unary exchange does. What it reads it holds to the schema's types and to
// the job value objects' rules, so a job event it returns is one a
// JobEventSequence can judge.

#pragma once

#include <cstddef>
#include <cstdint>
#include <functional>
#include <span>
#include <string>
#include <string_view>
#include <variant>
#include <vector>

#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/job_event.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/result.hpp"
#include "tensorplate/ipc/sidecar_codec.hpp"

namespace tensorplate::adapters::python_pytorch {

/// The capability that enables the job and session messages.
inline constexpr std::string_view kSpeechJobsCapability = "speech_jobs_v1";

/// Largest integer a header field carries (2^53 - 1).
inline constexpr std::uint64_t kMaxWireInteger = (std::uint64_t{1} << 53U) - 1;

/// Error::context of a fault the job value objects have no reason for: a
/// field of the wrong type or out of the schema's range, or payload bytes
/// that do not match the header.
inline constexpr std::string_view kJobMessageMalformed = "job_message_malformed";

// -- Adapter to sidecar. -----------------------------------------------------

/// Bytes of the payload region a `job_submit` for `request` carries: its PCM
/// window, or its text.
[[nodiscard]] std::size_t job_submit_payload_size(const JobRequest& request) noexcept;

/// Header of the `job_submit` for `request`. The frame's payload region is
/// the request's PCM window or text, job_submit_payload_size() bytes.
///
/// @return the header, or Error::Code::ConfigInvalid with reason
///   "message_id_empty", or "wire_integer_out_of_range" when an identity
///   field, `start_sample` or `utterance_id` exceeds kMaxWireInteger.
[[nodiscard]] Result<std::string> encode_job_submit(const JobRequest& request,
                                                    std::string_view message_id);

/// Header of a `job_cancel`; its frame has no payload. Reasons as for
/// encode_job_submit, and "job_id_zero", "session_key_zero" or
/// "generation_zero".
[[nodiscard]] Result<std::string> encode_job_cancel(const JobIdentity& identity,
                                                    std::string_view message_id);

/// Header of a `session_release`; its frame has no payload. Reasons as for
/// encode_job_cancel, without the job id's.
[[nodiscard]] Result<std::string> encode_session_release(std::uint64_t session_key,
                                                         std::uint64_t generation,
                                                         std::string_view message_id);

// -- Negotiation. ------------------------------------------------------------

/// Whether a `ready_event` header lists kSpeechJobsCapability. A header
/// without `capabilities` lists nothing, and a capability this adapter does
/// not know is ignored.
///
/// @return Error::Code::InferenceFailed with reason "capabilities_malformed"
///   when the field is present and is not an array of strings.
[[nodiscard]] Result<bool> offers_speech_jobs(std::string_view ready_event_header);

/// The job classes a successful `load_model_response` header declares for a
/// load that enabled kSpeechJobsCapability, in the header's order. A class
/// name this adapter does not know is left out.
///
/// @return Error::Code::InferenceFailed with reason "job_classes_malformed"
///   when the field is absent, empty, not an array of strings or repeats a
///   name.
[[nodiscard]] Result<std::vector<JobClass>> declared_job_classes(
    std::string_view load_model_response_header);

// -- Sidecar to adapter. -----------------------------------------------------

/// Publishes the PCM of one `audio_chunk` into a buffer the receiver of the
/// event releases. Called at most once per frame, after every other check of
/// the message passed.
using PcmPublisher = std::function<Result<BufferRef>(std::span<const std::byte>)>;

/// A frame the job channel does not own: a unary response, a `ready_event`
/// or a `metric_event`, or a header that is not a JSON object with a `kind`.
struct NotJobMessage {
  friend bool operator==(const NotJobMessage&, const NotJobMessage&) noexcept = default;
};

/// A `session_released`.
struct SessionReleased {
  std::uint64_t session_key = 0;
  std::uint64_t generation = 0;
  friend bool operator==(const SessionReleased&, const SessionReleased&) noexcept = default;
};

/// An `error_event`: the sidecar refused the message named `message_id`,
/// one the adapter sent. The sidecar names no job in it. Texts longer than
/// kMaxJobErrorTextBytes are cut at a character boundary.
struct MessageRefused {
  std::string message_id;
  Error error;
  friend bool operator==(const MessageRefused&, const MessageRefused&) noexcept = default;
};

/// A job or session message that cannot be delivered as it stands. `error` is
/// Error::Code::InferenceFailed with, in Error::context, the reason the job
/// value objects give or kJobMessageMalformed; a publisher's error is passed
/// on unchanged. `job_id` is zero when the message names no readable job.
struct JobMessageFault {
  std::uint64_t job_id = 0;
  Error error;
  friend bool operator==(const JobMessageFault&, const JobMessageFault&) noexcept = default;
};

using SidecarJobMessage =
    std::variant<NotJobMessage, JobEvent, SessionReleased, MessageRefused, JobMessageFault>;

/// Reads one frame from the sidecar. A `job_failed` error's texts are cut to
/// kMaxJobErrorTextBytes at a character boundary, and an error code this
/// adapter does not know reads as Error::Code::Internal.
///
/// `publish_pcm` receives the payload region of an `audio_chunk`; an empty
/// one faults such a message with Error::Code::NotReady, "backend_not_ready".
[[nodiscard]] SidecarJobMessage decode_sidecar_message(const ipc::SidecarFrame& frame,
                                                       const PcmPublisher& publish_pcm);

}  // namespace tensorplate::adapters::python_pytorch
