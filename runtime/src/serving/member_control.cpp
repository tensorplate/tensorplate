// SPDX-License-Identifier: Apache-2.0
#include "serving/member_control.hpp"

#include <algorithm>
#include <string>
#include <utility>

#include "tensorplate/serving/router.hpp"

namespace tensorplate::serving {
namespace {

/// Why admission is closed, as the router and a stream refusal name it; null
/// while it is open.
const char* closed_cause(MemberAdmission admission) noexcept {
  switch (admission) {
    case MemberAdmission::Fenced:
      return "fenced";
    case MemberAdmission::Retiring:
      return "retiring";
    case MemberAdmission::Open:
      break;
  }
  return nullptr;
}

/// The quota in force: the stream sessions, whose buffers no byte is
/// reserved for yet.
ipc::WorkerQuota quota_of(std::uint32_t sessions) {
  ipc::WorkerQuota quota;
  quota.session_count = sessions;
  if (sessions > 0) {
    quota.domain_bytes.guest_ram = 0;
  }
  return quota;
}

}  // namespace

MemberControlTarget::MemberControlTarget(Retire on_retire) : on_retire_(std::move(on_retire)) {}

ipc::WorkerLedger MemberControlTarget::ledger_status() {
  return unimplemented_.ledger_status();
}

Result<void> MemberControlTarget::admission_fence() {
  const std::lock_guard guard(mu_);
  if (admission_.load() == MemberAdmission::Open) {
    admission_.store(MemberAdmission::Fenced);
    publish_locked();
  }
  return {};
}

Result<ipc::WorkerQuota> MemberControlTarget::activate() {
  const std::lock_guard guard(mu_);
  if (admission_.load() == MemberAdmission::Retiring) {
    return unexpected(Error::make(Error::Code::Unavailable,
                                  "the member is retiring and admits nothing again",
                                  "member_retiring"));
  }
  const auto ceiling = ceiling_.load();
  if (ceiling < 0) {
    return unexpected(
        Error::make(Error::Code::NotReady, "the member is still loading", "member_loading"));
  }
  admission_.store(MemberAdmission::Open);
  publish_locked();
  return quota_of(static_cast<std::uint32_t>(ceiling));
}

Result<void> MemberControlTarget::retire(std::chrono::milliseconds drain_timeout) {
  const std::lock_guard guard(mu_);
  if (admission_.exchange(MemberAdmission::Retiring) == MemberAdmission::Retiring) {
    return {};
  }
  publish_locked();
  on_retire_(retirement_window(drain_timeout), drain_timeout);
  return {};
}

Result<ipc::WorkerQuota> MemberControlTarget::assign_quota(const ipc::WorkerQuota& quota) {
  return unimplemented_.assign_quota(quota);
}

Result<void> MemberControlTarget::apply_pressure(const ipc::WorkerPressure& pressure) {
  return unimplemented_.apply_pressure(pressure);
}

void MemberControlTarget::bind_router(RequestRouter& router) {
  const std::lock_guard guard(mu_);
  router_ = &router;
  publish_locked();
}

void MemberControlTarget::set_session_ceiling(std::uint32_t ceiling) noexcept {
  ceiling_.store(ceiling);
}

Result<void> MemberControlTarget::admit_session(const ControlChannel& channel) const {
  const char* cause = closed_cause(admission_.load());
  if (cause == nullptr && !channel.admission()) {
    cause = "lost contact with the agent";
  }
  if (cause == nullptr) {
    return {};
  }
  return unexpected(Error::make(Error::Code::NotReady,
                                std::string{"session admission is closed: "} + cause,
                                "admission_closed"));
}

std::chrono::milliseconds MemberControlTarget::retirement_window(
    std::chrono::milliseconds asked) noexcept {
  return std::min(asked, kMaxRetirementDrain);
}

void MemberControlTarget::publish_locked() noexcept {
  if (router_ != nullptr) {
    router_->set_admission_closed(closed_cause(admission_.load()));
  }
}

}  // namespace tensorplate::serving
