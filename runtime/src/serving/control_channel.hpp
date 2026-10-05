// SPDX-License-Identifier: Apache-2.0
//
// The worker's end of the agent-to-worker control channel: the socket the
// agent passes as fd 0, and the thread that answers its requests.

#pragma once

#include <atomic>
#include <chrono>
#include <cstdint>
#include <memory>
#include <string>
#include <string_view>
#include <thread>

#include "tensorplate/core/result.hpp"
#include "tensorplate/ipc/worker_control.hpp"

namespace tensorplate::serving {

/// Owns the worker's end of the control channel and closes it.
class ControlSocket {
 public:
  ControlSocket() noexcept = default;
  explicit ControlSocket(int fd) noexcept : fd_(fd) {}
  ControlSocket(ControlSocket&& other) noexcept;
  ControlSocket& operator=(ControlSocket&& other) noexcept;
  ControlSocket(const ControlSocket&) = delete;
  ControlSocket& operator=(const ControlSocket&) = delete;
  ~ControlSocket();

  [[nodiscard]] int fd() const noexcept { return fd_; }

 private:
  int fd_ = -1;
};

/// Takes the control socket the agent passed as fd 0: moves it to a private
/// close-on-exec descriptor and reopens fd 0 on /dev/null, so that no child
/// of this process can inherit an end of the channel. Call it before the
/// process starts a thread or a child.
///
/// @return the socket, or Error::Code::Unavailable with context
///   "control_descriptor_absent" if fd 0 is not a connected stream socket of
///   the local family (it is left as it was) or "control_peer_closed" if the agent's
///   end is already closed, or Error::Code::Internal if a descriptor
///   operation fails.
[[nodiscard]] Result<ControlSocket> take_inherited_control_socket();

/// What the control channel asks of its worker. Called on the control
/// thread only. An implementation answers at once and never waits for a
/// backend job: every request is answered within a second.
class ControlTarget {
 public:
  ControlTarget() = default;
  ControlTarget(const ControlTarget&) = delete;
  ControlTarget& operator=(const ControlTarget&) = delete;
  virtual ~ControlTarget() = default;

  /// Reservations held for the member's logical sessions.
  [[nodiscard]] virtual ipc::WorkerLedger ledger_status() = 0;
  /// Stops admitting sessions ahead of a transaction's commit.
  [[nodiscard]] virtual Result<void> admission_fence() = 0;
  /// Starts serving; returns the effective quota.
  [[nodiscard]] virtual Result<ipc::WorkerQuota> activate() = 0;
  /// Starts the retirement drain.
  [[nodiscard]] virtual Result<void> retire(std::chrono::milliseconds drain_timeout) = 0;
  /// Replaces the quota; returns the effective one.
  [[nodiscard]] virtual Result<ipc::WorkerQuota> assign_quota(const ipc::WorkerQuota& quota) = 0;
  /// Applies a pressure directive.
  [[nodiscard]] virtual Result<void> apply_pressure(const ipc::WorkerPressure& pressure) = 0;
};

/// The target of a worker that implements no runtime operation yet: it
/// reports an empty ledger and refuses every other request as unsupported,
/// so that the agent is never told an operation took effect.
class UnimplementedControlTarget final : public ControlTarget {
 public:
  [[nodiscard]] ipc::WorkerLedger ledger_status() override;
  [[nodiscard]] Result<void> admission_fence() override;
  [[nodiscard]] Result<ipc::WorkerQuota> activate() override;
  [[nodiscard]] Result<void> retire(std::chrono::milliseconds drain_timeout) override;
  [[nodiscard]] Result<ipc::WorkerQuota> assign_quota(const ipc::WorkerQuota& quota) override;
  [[nodiscard]] Result<void> apply_pressure(const ipc::WorkerPressure& pressure) override;
};

/// Reads newline-delimited requests from the control socket and answers each
/// one on a thread of its own, which never calls into an execution session.
///
/// A request naming another member is answered with this worker's member and
/// reaches no target. A complete frame that does not decode is skipped; a
/// line longer than the frame limit ends the channel, as does a response the
/// agent does not take within the write limit.
///
/// The agent is in contact from the start. Contact is lost when it sends
/// nothing for the contact timeout and returns with its next request; it is
/// lost for good when either side closes the channel.
class ControlChannel {
 public:
  static constexpr std::chrono::milliseconds kContactTimeout{3000};
  static constexpr std::chrono::milliseconds kWriteTimeout{1000};

  /// Starts the control thread. `target` must outlive the channel.
  /// @return Error::Code::ConfigInvalid if `member` is not one the wire
  ///   format can name, or Error::Code::Internal if the thread cannot start.
  [[nodiscard]] static Result<std::unique_ptr<ControlChannel>> start(
      ControlSocket socket, ipc::WorkerMember member, ControlTarget& target,
      std::chrono::milliseconds contact_timeout = kContactTimeout);

  /// Ends the channel and joins the thread.
  ~ControlChannel();
  ControlChannel(const ControlChannel&) = delete;
  ControlChannel& operator=(const ControlChannel&) = delete;

  /// Whether the worker may admit new sessions: success while the agent is
  /// in contact, else Error::Code::Unavailable with context
  /// "control_contact_lost" or, once the channel has ended,
  /// "control_channel_closed". Sessions already admitted are not affected.
  [[nodiscard]] Result<void> admission() const;

  /// True once the channel has ended, by either side.
  [[nodiscard]] bool closed() const noexcept;

 private:
  enum class Contact : std::uint8_t { Present, Lost, Closed };
  enum class Input : std::uint8_t { Ready, Idle, Failed };

  ControlChannel(ControlSocket socket, ipc::WorkerMember member, ControlTarget& target,
                 std::chrono::milliseconds contact_timeout) noexcept;

  void run();
  /// Waits for input or the contact deadline, which it records as lost.
  [[nodiscard]] Input wait_for_input(std::chrono::steady_clock::time_point contact_deadline);
  /// Answers every complete frame in `pending`. False once the channel
  /// must end.
  [[nodiscard]] bool answer_frames(std::string& pending,
                                   std::chrono::steady_clock::time_point& contact_deadline);
  /// False once the channel must end.
  [[nodiscard]] bool answer(const ipc::WorkerControlRequest& request);
  void perform(const ipc::WorkerControlRequest& request, ipc::WorkerControlResponse& response);
  [[nodiscard]] bool write_frame(std::string_view frame) const;

  ControlSocket socket_;
  ipc::WorkerMember member_;
  ControlTarget& target_;
  std::chrono::milliseconds contact_timeout_;
  std::atomic<Contact> contact_{Contact::Present};
  std::atomic<bool> stopping_{false};
  std::thread thread_;
};
}  // namespace tensorplate::serving
