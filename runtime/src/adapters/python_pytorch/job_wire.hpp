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
#include <optional>
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
///   A `message_id` is the adapter's own and well-formed UTF-8; an
///   ill-formed byte would be sent as U+FFFD.
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

/// Header of a `health_check`; its frame has no payload.
///
/// @return the header, or Error::Code::ConfigInvalid, "message_id_empty".
[[nodiscard]] Result<std::string> encode_health_check(std::string_view message_id);

// -- Negotiation. ------------------------------------------------------------

/// Most names a load response's `job_classes` may hold, whether this adapter
/// knows them or not: the bound the schema gives a capability list.
inline constexpr std::size_t kMaxDeclaredJobClasses = 16;

/// Whether a `ready_event` header lists kSpeechJobsCapability. A header
/// without `capabilities` lists nothing, and a capability this adapter does
/// not know is ignored.
///
/// @return Error::Code::InferenceFailed with reason "capabilities_malformed"
///   when the field is present and is not an array of strings.
[[nodiscard]] Result<bool> offers_speech_jobs(std::string_view ready_event_header);

/// The job classes a successful `load_model_response` header declares for a
/// load that enabled kSpeechJobsCapability, in the header's order. A class
/// name this adapter does not know is left out, so the list is empty when it
/// knows none of them.
///
/// @return Error::Code::InferenceFailed with reason "job_classes_malformed"
///   when the field is absent, empty, longer than kMaxDeclaredJobClasses,
///   not an array of strings or repeats a name.
[[nodiscard]] Result<std::vector<JobClass>> declared_job_classes(
    std::string_view load_model_response_header);

/// Error::context of an answer to a `health_check` that cannot be read as one.
inline constexpr std::string_view kHealthResponseMalformed = "health_response_malformed";

/// Whether a `health_check_response` header says the sidecar's backend is
/// ready.
///
/// @return Error::Code::InferenceFailed with kHealthResponseMalformed when the
///   header is not a `health_check_response` of this protocol whose status,
///   if it has one, is "ok" and whose `health.ready` is a boolean.
[[nodiscard]] Result<bool> reports_ready(std::string_view health_check_response_header);

// -- Sidecar to adapter. -----------------------------------------------------

/// Publishes the PCM of one `audio_chunk` into a buffer the receiver of the
/// event releases. Called at most once per frame, after every other check of
/// the message passed. The span is the frame's payload region: it is valid
/// during the call only.
///
/// A handle it returns that is released or smaller than the chunk faults the
/// job with Error::Code::Internal, "pcm_publisher_handle_unusable". No event
/// carries that handle, so it stays with the publisher's owner: a caller
/// that is handed a JobMessageFault after its publisher ran releases what
/// the publisher returned.
using PcmPublisher = std::function<Result<BufferRef>(std::span<const std::byte>)>;

/// A frame the job channel does not own, with a header it can read: a unary
/// response, a `ready_event`, a `metric_event`, or a `session_*` kind other
/// than `session_released` and `session_release`. The channel's health probe
/// or the unary exchange judges it.
struct NotJobMessage {
  /// The frame's `message_id`; empty when it has none that is a string.
  std::string message_id;
  friend bool operator==(const NotJobMessage&, const NotJobMessage&) noexcept = default;
};

/// A frame the adapter cannot attribute: its header is not a JSON object
/// with a string `kind`, or it is an `error_event` without a readable,
/// non-empty `message_id`, which no sidecar writes. Nothing says whom it was
/// for, so its reader ends the connection, as the unary exchange does for a
/// response it cannot parse.
struct UnreadableHeader {
  friend bool operator==(const UnreadableHeader&, const UnreadableHeader&) noexcept = default;
};

/// A `session_released`.
struct SessionReleased {
  std::uint64_t session_key = 0;
  std::uint64_t generation = 0;
  friend bool operator==(const SessionReleased&, const SessionReleased&) noexcept = default;
};

/// An `error_event` with a readable, non-empty `message_id`: the sidecar
/// refused the message of that id, one the adapter sent. The sidecar names no
/// job in it. Texts longer than kMaxJobErrorTextBytes are cut at a character
/// boundary. An event whose `schema_version` is not this protocol's, or whose
/// error object cannot be read, still names its message: `error` is then
/// Error::Code::Internal with kJobMessageMalformed, so the job behind the
/// message ends.
struct MessageRefused {
  std::string message_id;
  Error error;
  friend bool operator==(const MessageRefused&, const MessageRefused&) noexcept = default;
};

/// A job or session message that cannot be delivered as it stands, a `job_*`
/// kind the sidecar does not send and a `session_release` among them. `error`
/// is Error::Code::InferenceFailed with, in Error::context, the reason the
/// job value objects give or kJobMessageMalformed, except for the publisher
/// faults named at PcmPublisher and decode_sidecar_message; a publisher's own
/// error is passed on unchanged. Error::message names the message's kind and,
/// for kJobMessageMalformed, the field.
struct JobMessageFault {
  /// Zero when the message names no readable job.
  std::uint64_t job_id = 0;
  Error error;
  /// The session a faulty `session_released` names, when its key and
  /// generation are both readable and nonzero.
  std::optional<SessionReleased> session;
  friend bool operator==(const JobMessageFault&, const JobMessageFault&) noexcept = default;
};

/// Cuts `text` to kMaxJobErrorTextBytes at a character boundary: the longest
/// text the error of a `failed` event carries.
void cut_to_job_error_bound(std::string& text);

using SidecarJobMessage = std::variant<NotJobMessage, JobEvent, SessionReleased, MessageRefused,
                                       JobMessageFault, UnreadableHeader>;

/// Reads one frame from the sidecar. A `job_failed` error's texts are cut to
/// kMaxJobErrorTextBytes at a character boundary, and an error code this
/// adapter does not know reads as Error::Code::Internal.
///
/// `publish_pcm` receives the payload region of an `audio_chunk`; an empty
/// one faults such a message with Error::Code::NotReady, "backend_not_ready".
[[nodiscard]] SidecarJobMessage decode_sidecar_message(const ipc::SidecarFrame& frame,
                                                       const PcmPublisher& publish_pcm);

}  // namespace tensorplate::adapters::python_pytorch
