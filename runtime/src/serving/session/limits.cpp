// SPDX-License-Identifier: Apache-2.0
#include "tensorplate/serving/session.hpp"

namespace tensorplate::serving {
namespace {
using namespace std::chrono_literals;
constexpr std::uint32_t kProtocolSessionCeiling = 2048;
}  // namespace

SessionLimits SessionLimits::defaults() noexcept {
  return SessionLimits{60s, 10s, 30s, 60min, kProtocolSessionCeiling};
}

Result<SessionLimits> SessionLimits::create(std::chrono::milliseconds idle_timeout,
                                            std::chrono::milliseconds heartbeat_interval,
                                            std::chrono::milliseconds liveness_timeout,
                                            std::chrono::milliseconds max_duration,
                                            std::uint32_t max_sessions) {
  if (idle_timeout <= 0ms || heartbeat_interval <= 0ms || liveness_timeout <= 0ms ||
      max_duration <= 0ms || heartbeat_interval >= liveness_timeout || max_sessions == 0 ||
      max_sessions > kProtocolSessionCeiling) {
    return unexpected(Error::make(Error::Code::ConfigInvalid, "invalid session limits",
                                  "invalid_session_limits"));
  }
  return SessionLimits{idle_timeout, heartbeat_interval, liveness_timeout, max_duration,
                       max_sessions};
}
}  // namespace tensorplate::serving
