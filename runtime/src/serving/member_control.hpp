// SPDX-License-Identifier: Apache-2.0
//
// The control target of a worker that serves a deployment generation: the
// admission the agent's fence, activate and retire set, which the router and
// the stream listener read.

#pragma once

#include <atomic>
#include <chrono>
#include <cstdint>
#include <functional>
#include <mutex>

#include "tensorplate/core/result.hpp"
#include "tensorplate/ipc/worker_control.hpp"

#include "serving/control_channel.hpp"

namespace tensorplate::serving {

class RequestRouter;

/// Whether a member admits new requests and stream sessions.
enum class MemberAdmission : std::uint8_t { Open, Fenced, Retiring };

/// A fence closes admission and an activate opens it again; a retire closes
/// it for good and asks for the worker's stop once. Every operation is
/// idempotent. Quota and pressure stay unsupported and the ledger empty.
class MemberControlTarget final : public ControlTarget {
 public:
  /// The longest retirement drain a retire can ask for.
  static constexpr std::chrono::milliseconds kMaxRetirementDrain{30'000};

  /// Called by the first retire on the control thread, with the drain window
  /// and what the agent asked for. It must not wait.
  using Retire =
      std::function<void(std::chrono::milliseconds window, std::chrono::milliseconds asked)>;

  explicit MemberControlTarget(Retire on_retire);

  [[nodiscard]] ipc::WorkerLedger ledger_status() override;
  [[nodiscard]] Result<void> admission_fence() override;
  [[nodiscard]] Result<ipc::WorkerQuota> activate() override;
  [[nodiscard]] Result<void> retire(std::chrono::milliseconds drain_timeout) override;
  [[nodiscard]] Result<ipc::WorkerQuota> assign_quota(const ipc::WorkerQuota& quota) override;
  [[nodiscard]] Result<void> apply_pressure(const ipc::WorkerPressure& pressure) override;

  /// Has `router` refuse new requests while admission is closed, from now on.
  void bind_router(RequestRouter& router);

  /// The stream sessions the built worker serves at one time. Until it is
  /// set, an activate is answered as not ready.
  void set_session_ceiling(std::uint32_t ceiling) noexcept;

  /// Whether a stream Open may be admitted: not while fenced, retiring or
  /// out of contact with the agent. Reads atomics only.
  [[nodiscard]] Result<void> admit_session(const ControlChannel& channel) const;

  /// The drain a retire that asks for `asked` is given.
  [[nodiscard]] static std::chrono::milliseconds retirement_window(
      std::chrono::milliseconds asked) noexcept;

 private:
  void publish_locked() noexcept;

  /// Held by each operation and by bind_router().
  std::mutex mu_;
  std::atomic<MemberAdmission> admission_{MemberAdmission::Open};
  RequestRouter* router_ = nullptr;
  /// -1 until the worker is built.
  std::atomic<std::int64_t> ceiling_{-1};
  Retire on_retire_;
  UnimplementedControlTarget unimplemented_;
};

}  // namespace tensorplate::serving
