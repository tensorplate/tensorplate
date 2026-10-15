// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <grpcpp/support/status.h>

#include <cstdint>
#include <optional>
#include <string>
#include <string_view>

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/result.hpp"
#include "tensorplate/serving/session.hpp"
#include "tensorplate/stream/v1/session.pb.h"

#include "serving/session/dispatch.hpp"

namespace tensorplate::serving::stream {

namespace wire = ::tensorplate::stream::v1;

/// Largest sequence, generation or id the stream schema allows: 2^53 - 1.
inline constexpr std::uint64_t kMaxWireInteger = (std::uint64_t{1} << 53U) - 1U;

/// The deployment a listener serves. A worker without a generation serves no
/// generation and no descriptor digest: zero and empty.
struct ServedTarget {
  std::string deployment_id;
  std::uint64_t generation = 0;
  std::string descriptor_digest;
};

/// The reason an end cause reports for `error`: the reason its context names,
/// else the one its code implies.
[[nodiscard]] wire::FailureReason failure_reason_of(const Error& error);

/// The error code the failure reason taxonomy pairs with `reason`.
[[nodiscard]] wire::ErrorCode taxonomy_code(wire::FailureReason reason) noexcept;

/// `reason` with its taxonomy code and `detail` cut to the schema's 64 ASCII
/// characters.
[[nodiscard]] wire::EndCause end_cause(wire::FailureReason reason, std::string_view detail);
[[nodiscard]] wire::EndCause end_cause(const Error& error);

/// The gRPC status code for a call ended by `reason`: the stream's error
/// mapping, and for a reason it does not name, the class of the error code the
/// taxonomy pairs with it.
[[nodiscard]] grpc::StatusCode status_code(wire::FailureReason reason) noexcept;

/// The status that ends a call whose session closed: OK when it closed without
/// a cause, else the status code of the cause's reason, and never OK for a
/// failed session. The typed outcome is the SessionClosed the client has
/// already read.
[[nodiscard]] grpc::Status session_end_status(const wire::SessionClosed& closed);

/// The status that ends a call whose Open was refused. How the status carries
/// the OpenRefused is not decided yet: until then it carries the code and a
/// message, and no detail.
[[nodiscard]] grpc::Status refused_open_status(const wire::OpenRefused& refused);

/// The address the listener binds for `host`. "localhost" is the IPv4
/// loopback, as for the HTTP bind: it is never resolved. An IPv6 literal is
/// bracketed.
[[nodiscard]] std::string listen_address(std::string_view host, std::uint16_t port);

[[nodiscard]] wire::SessionState wire_state(LogicalSessionState state) noexcept;
[[nodiscard]] wire::AudioFormat wire_format(const StreamAudioFormat& format);

/// The input credit and the status a stream reports, from one transition the
/// manager's sink was shown.
[[nodiscard]] wire::InputCredit wire_credit(const LogicalSessionStatus& status,
                                            const LogicalSessionUsage& waiting_segments);
[[nodiscard]] wire::SessionStatus wire_status(const LogicalSessionStatus& status,
                                              const LogicalSessionUsage& waiting_segments);

/// What a call's session has accepted, as the checks of its next input need
/// it.
struct InputHistory {
  /// Audio samples accepted: the offset the next Audio carries.
  std::uint64_t samples = 0;
  /// The last frame accepted was shorter than the smallest granted, and no
  /// Finalize of the client's has followed it.
  bool short_frame = false;
  /// The highest segment id accepted.
  std::optional<std::uint64_t> last_segment_id;
  /// Text bytes accepted since the last Finalize the session started on.
  std::uint64_t synthesis_text_bytes = 0;
};

/// Whether `audio` may be offered to a session granted `terms`.
/// @return ConfigInvalid with context "empty_input" for a body without data,
///   else with context "invalid_event" and the failed check as its message:
///   `wrong_input_kind`, `frame_size_mismatch`, `frame_too_long`,
///   `sample_offset_mismatch` or `audio_after_short_frame`.
[[nodiscard]] Result<void> check_audio(const wire::Audio& audio, const SessionTerms& terms,
                                       const InputHistory& history);

/// Whether a frame of `sample_count` samples is shorter than the smallest
/// `terms` grant: valid only as the last before a Finalize or a half-close.
[[nodiscard]] bool is_short_frame(std::uint32_t sample_count, const SessionTerms& terms) noexcept;

/// Whether `segment` may be offered to a session granted `terms`.
/// @return ConfigInvalid with context "empty_input" for text that is empty or
///   only ASCII whitespace, else with context "invalid_event" and the failed
///   check as its message: `wrong_input_kind`, `segment_id_out_of_range`,
///   `segment_id_not_increasing`, `segment_too_long`,
///   `synthesis_text_limit_exceeded` or `turn_id_malformed`.
[[nodiscard]] Result<void> check_text_segment(const wire::TextSegment& segment,
                                              const SessionTerms& terms,
                                              const InputHistory& history);

/// The server event that carries `output`, without its sequence; `format` is
/// the session's audio format. None for an alternative this server does not
/// send.
[[nodiscard]] std::optional<wire::ServerEvent> task_event(const TaskOutput& output,
                                                          const StreamAudioFormat& format);

/// The request an Open makes, checked against the schema's bounds.
/// @return ConfigInvalid with context "invalid_event" for a malformed Open,
///   Unsupported with context "backend_unsupported_capability" for a mode or
///   encoding this server does not know.
[[nodiscard]] Result<SessionRequest> session_request(const wire::Open& open);

/// Whether an Open's target is the one served, compared by deployment id, then
/// generation, then descriptor digest.
/// @return Unsupported "target_unresolved" for a selector, ConfigInvalid
///   "invalid_event" for no target or a malformed deployment id, NotReady
///   "target_mismatch" for another deployment, NotReady "stale_generation" for
///   another generation, whatever digest it names, and NotReady
///   "target_mismatch" for another digest of the served generation.
[[nodiscard]] Result<void> check_target(const wire::Open& open, const ServedTarget& served);

}  // namespace tensorplate::serving::stream
