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
/// generation and no descriptor digest.
struct ServedTarget {
  std::string deployment_id;
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

/// The gRPC status code for a call ended by `reason`.
[[nodiscard]] grpc::StatusCode status_code(wire::FailureReason reason) noexcept;

/// The status that ends a call whose Open was refused. How the status carries
/// the OpenRefused is not decided yet: until then it carries the code and a
/// message, and no detail.
[[nodiscard]] grpc::Status refused_open_status(const wire::OpenRefused& refused);

[[nodiscard]] wire::SessionState wire_state(LogicalSessionState state) noexcept;
[[nodiscard]] wire::AudioFormat wire_format(const StreamAudioFormat& format);

/// The request an Open makes, checked against the schema's bounds.
/// @return ConfigInvalid with context "invalid_event" for a malformed Open,
///   Unsupported with context "backend_unsupported_capability" for a mode or
///   encoding this server does not know.
[[nodiscard]] Result<SessionRequest> session_request(const wire::Open& open);

/// Whether an Open's target is the one served.
/// @return Unsupported "target_unresolved" for a selector, ConfigInvalid
///   "invalid_event" for no target or a malformed deployment id, NotReady
///   "target_mismatch" for another deployment or any descriptor digest, and
///   NotReady "stale_generation" for any generation: the worker serves none, and
///   is addressed by its deployment alone.
[[nodiscard]] Result<void> check_target(const wire::Open& open, const ServedTarget& served);

}  // namespace tensorplate::serving::stream
