// SPDX-License-Identifier: Apache-2.0
#include "serving/control_channel.hpp"

#include <fcntl.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <unistd.h>

#include <array>
#include <cerrno>
#include <string>
#include <system_error>
#include <utility>

namespace tensorplate::serving {
namespace {
using Clock = std::chrono::steady_clock;

#ifdef MSG_NOSIGNAL
constexpr int kSendFlags = MSG_NOSIGNAL | MSG_DONTWAIT;
#else
constexpr int kSendFlags = MSG_DONTWAIT;
#endif

Error unavailable(std::string message, std::string reason) {
  return Error::make(Error::Code::Unavailable, std::move(message), std::move(reason));
}

Error descriptor_failure(const char* step) {
  return Error::make(Error::Code::Internal,
                     std::string{"control descriptor: "} + step +
                         " failed: " + std::generic_category().message(errno),
                     "control_descriptor_failure");
}

/// A stream socket of the local family with a peer: what one end of the
/// agent's socket pair is.
bool is_connected_local_stream_socket(int fd) noexcept {
  struct stat info {};
  int type = 0;
  socklen_t type_size = sizeof(type);
  sockaddr_storage local{};
  socklen_t local_size = sizeof(local);
  sockaddr_storage peer{};
  socklen_t peer_size = sizeof(peer);
  // NOLINTBEGIN(cppcoreguidelines-pro-type-reinterpret-cast)
  return ::fstat(fd, &info) == 0 && S_ISSOCK(info.st_mode) &&
         ::getsockopt(fd, SOL_SOCKET, SO_TYPE, &type, &type_size) == 0 && type == SOCK_STREAM &&
         ::getsockname(fd, reinterpret_cast<sockaddr*>(&local), &local_size) == 0 &&
         local.ss_family == AF_UNIX &&
         ::getpeername(fd, reinterpret_cast<sockaddr*>(&peer), &peer_size) == 0;
  // NOLINTEND(cppcoreguidelines-pro-type-reinterpret-cast)
}

/// Milliseconds until `deadline`, at least zero, for poll().
int millis_until(Clock::time_point deadline) noexcept {
  const auto left = std::chrono::ceil<std::chrono::milliseconds>(deadline - Clock::now()).count();
  return left > 0 ? static_cast<int>(left) : 0;
}

Error unsupported(const char* operation) {
  return Error::make(Error::Code::Unsupported,
                     std::string{"this worker does not implement "} + operation,
                     "control_operation_unsupported");
}

ipc::WorkerControlStatus status_of(const Error& error) noexcept {
  switch (error.code) {
    case Error::Code::Unsupported:
      return ipc::WorkerControlStatus::Unsupported;
    case Error::Code::NotReady:
      return ipc::WorkerControlStatus::NotReady;
    case Error::Code::Timeout:
      return ipc::WorkerControlStatus::Timeout;
    default:
      return ipc::WorkerControlStatus::Error;
  }
}

void refuse(ipc::WorkerControlResponse& response, const Error& error) {
  response.status = status_of(error);
  response.error = error;
}
}  // namespace

ControlSocket::ControlSocket(ControlSocket&& other) noexcept : fd_(std::exchange(other.fd_, -1)) {}

ControlSocket& ControlSocket::operator=(ControlSocket&& other) noexcept {
  if (this != &other) {
    if (fd_ >= 0) {
      ::close(fd_);
    }
    fd_ = std::exchange(other.fd_, -1);
  }
  return *this;
}

ControlSocket::~ControlSocket() {
  if (fd_ >= 0) {
    ::close(fd_);
  }
}

Result<ControlSocket> take_inherited_control_socket() {
  if (!is_connected_local_stream_socket(STDIN_FILENO)) {
    return unexpected(unavailable(
        "no control socket on fd 0: a worker configured with a deployment generation is "
        "started by the agent, which passes one",
        "control_descriptor_absent"));
  }
  // NOLINTNEXTLINE(cppcoreguidelines-pro-type-vararg)
  const int moved = ::fcntl(STDIN_FILENO, F_DUPFD_CLOEXEC, STDERR_FILENO + 1);
  if (moved < 0) {
    return unexpected(descriptor_failure("moving fd 0"));
  }
  ControlSocket socket{moved};
#ifdef SO_NOSIGPIPE
  const int on = 1;
  (void)::setsockopt(moved, SOL_SOCKET, SO_NOSIGPIPE, &on, sizeof(on));
#endif
  // dup2 leaves fd 0 without close-on-exec, which is what a child expects.
  // NOLINTNEXTLINE(cppcoreguidelines-pro-type-vararg)
  const int null_input = ::open("/dev/null", O_RDONLY | O_CLOEXEC);
  if (null_input < 0) {
    return unexpected(descriptor_failure("opening /dev/null"));
  }
  const int replaced = ::dup2(null_input, STDIN_FILENO);
  ::close(null_input);
  if (replaced < 0) {
    return unexpected(descriptor_failure("replacing fd 0"));
  }
  char byte = 0;
  if (::recv(moved, &byte, 1, MSG_PEEK | MSG_DONTWAIT) == 0) {
    return unexpected(
        unavailable("the agent's end of the control socket is closed", "control_peer_closed"));
  }
  return socket;
}

ipc::WorkerLedger UnimplementedControlTarget::ledger_status() {
  return {};
}

Result<void> UnimplementedControlTarget::admission_fence() {
  return unexpected(unsupported("admission_fence"));
}

Result<ipc::WorkerQuota> UnimplementedControlTarget::activate() {
  return unexpected(unsupported("activate"));
}

Result<void> UnimplementedControlTarget::retire(std::chrono::milliseconds /*drain_timeout*/) {
  return unexpected(unsupported("retire"));
}

Result<ipc::WorkerQuota> UnimplementedControlTarget::assign_quota(
    const ipc::WorkerQuota& /*quota*/) {
  return unexpected(unsupported("quota_assign"));
}

Result<void> UnimplementedControlTarget::apply_pressure(const ipc::WorkerPressure& /*pressure*/) {
  return unexpected(unsupported("pressure_directive"));
}

ControlChannel::ControlChannel(ControlSocket socket, ipc::WorkerMember member,
                               ControlTarget& target,
                               std::chrono::milliseconds contact_timeout) noexcept
    : socket_(std::move(socket)),
      member_(std::move(member)),
      target_(target),
      contact_timeout_(contact_timeout) {}

Result<std::unique_ptr<ControlChannel>> ControlChannel::start(
    ControlSocket socket, ipc::WorkerMember member, ControlTarget& target,
    std::chrono::milliseconds contact_timeout) {
  // A member the wire format cannot name could never answer a request.
  ipc::WorkerControlResponse probe;
  probe.transaction_id = "runtime";
  probe.correlation_id = "probe";
  probe.member = member;
  probe.ledger = ipc::WorkerLedger{};
  if (socket.fd() < 0 || contact_timeout.count() <= 0 ||
      !ipc::encode_worker_control_response(probe)) {
    return unexpected(Error::make(Error::Code::ConfigInvalid,
                                  "the deployment endpoint and generation do not name a member "
                                  "of the control protocol",
                                  "invalid_control_member"));
  }
  auto channel = std::unique_ptr<ControlChannel>(
      new ControlChannel(std::move(socket), std::move(member), target, contact_timeout));
  try {
    channel->thread_ = std::thread([owner = channel.get()] { owner->run(); });
  } catch (const std::system_error&) {
    return unexpected(Error::make(Error::Code::Internal, "control thread could not start",
                                  "control_thread_unavailable"));
  }
  return channel;
}

ControlChannel::~ControlChannel() {
  stopping_.store(true);
  // Wakes the thread out of poll(); the agent sees the channel end.
  ::shutdown(socket_.fd(), SHUT_RDWR);
  if (thread_.joinable()) {
    thread_.join();
  }
}

bool ControlChannel::closed() const noexcept {
  return contact_.load() == Contact::Closed;
}

Result<void> ControlChannel::admission() const {
  switch (contact_.load()) {
    case Contact::Present:
      return {};
    case Contact::Lost:
      return unexpected(unavailable("no control request arrived within the contact limit",
                                    "control_contact_lost"));
    case Contact::Closed:
      break;
  }
  return unexpected(unavailable("the control channel has ended", "control_channel_closed"));
}

ControlChannel::Input ControlChannel::wait_for_input(
    std::chrono::steady_clock::time_point contact_deadline) {
  // Checked on every pass: input that never decodes must not postpone it.
  if (contact_.load() == Contact::Present && Clock::now() >= contact_deadline) {
    contact_.store(Contact::Lost);
  }
  const bool present = contact_.load() == Contact::Present;
  pollfd waiting{socket_.fd(), POLLIN, 0};
  const int ready = ::poll(&waiting, 1, present ? millis_until(contact_deadline) : -1);
  if (ready > 0) {
    return Input::Ready;
  }
  if (ready < 0 && errno != EINTR) {
    return Input::Failed;
  }
  return Input::Idle;
}

bool ControlChannel::answer_frames(std::string& pending,
                                   std::chrono::steady_clock::time_point& contact_deadline) {
  for (auto end = pending.find('\n'); end != std::string::npos; end = pending.find('\n')) {
    if (end >= ipc::kWorkerControlMaxFrameBytes) {
      return false;
    }
    const auto request = ipc::decode_worker_control_request({pending.data(), end + 1});
    pending.erase(0, end + 1);
    if (!request) {
      continue;
    }
    contact_deadline = Clock::now() + contact_timeout_;
    contact_.store(Contact::Present);
    if (!answer(*request)) {
      return false;
    }
  }
  return pending.size() < ipc::kWorkerControlMaxFrameBytes;
}

void ControlChannel::run() {
  std::string pending;
  std::array<char, 4096> chunk{};
  auto contact_deadline = Clock::now() + contact_timeout_;
  while (!stopping_.load()) {
    const auto input = wait_for_input(contact_deadline);
    if (input == Input::Failed) {
      break;
    }
    if (input == Input::Idle) {
      continue;
    }
    const auto received = ::recv(socket_.fd(), chunk.data(), chunk.size(), 0);
    if (received < 0 && (errno == EINTR || errno == EAGAIN)) {
      continue;
    }
    if (received <= 0) {
      break;
    }
    pending.append(chunk.data(), static_cast<std::size_t>(received));
    if (!answer_frames(pending, contact_deadline)) {
      break;
    }
  }
  contact_.store(Contact::Closed);
  ::shutdown(socket_.fd(), SHUT_RDWR);
}

bool ControlChannel::answer(const ipc::WorkerControlRequest& request) {
  ipc::WorkerControlResponse response;
  response.transaction_id = request.transaction_id;
  response.correlation_id = request.correlation_id;
  response.op = request.op;
  response.member = member_;
  if (request.member == member_) {
    perform(request, response);
  } else {
    response.status = ipc::WorkerControlStatus::MemberMismatch;
  }
  auto frame = ipc::encode_worker_control_response(response);
  if (!frame) {
    // The target answered with something the wire format refuses.
    response.ledger.reset();
    response.quota.reset();
    refuse(response,
           Error::make(Error::Code::Internal, frame.error().message, "control_response_invalid"));
    frame = ipc::encode_worker_control_response(response);
  }
  return frame && write_frame(*frame);
}

void ControlChannel::perform(const ipc::WorkerControlRequest& request,
                             ipc::WorkerControlResponse& response) {
  const auto finish = [&response](const Result<void>& done) {
    if (!done) {
      refuse(response, done.error());
    }
  };
  const auto finish_quota = [&response](const Result<ipc::WorkerQuota>& quota) {
    if (quota) {
      response.quota = *quota;
    } else {
      refuse(response, quota.error());
    }
  };
  switch (request.op) {
    case ipc::WorkerControlOp::LedgerStatus:
      response.ledger = target_.ledger_status();
      break;
    case ipc::WorkerControlOp::AdmissionFence:
      finish(target_.admission_fence());
      break;
    case ipc::WorkerControlOp::Activate:
      finish_quota(target_.activate());
      break;
    case ipc::WorkerControlOp::Retire:
      finish(target_.retire(std::chrono::milliseconds{request.drain_timeout_ms.value_or(0)}));
      break;
    case ipc::WorkerControlOp::QuotaAssign:
      finish_quota(target_.assign_quota(request.quota.value_or(ipc::WorkerQuota{})));
      break;
    case ipc::WorkerControlOp::PressureDirective:
      finish(target_.apply_pressure(request.pressure.value_or(ipc::WorkerPressure{})));
      break;
  }
}

bool ControlChannel::write_frame(std::string_view frame) const {
  const auto give_up = Clock::now() + kWriteTimeout;
  while (!frame.empty()) {
    const auto sent = ::send(socket_.fd(), frame.data(), frame.size(), kSendFlags);
    if (sent > 0) {
      frame.remove_prefix(static_cast<std::size_t>(sent));
      continue;
    }
    if (errno == EINTR) {
      continue;
    }
    if (errno != EAGAIN && errno != EWOULDBLOCK) {
      return false;
    }
    pollfd waiting{socket_.fd(), POLLOUT, 0};
    if (Clock::now() >= give_up ||
        (::poll(&waiting, 1, millis_until(give_up)) < 0 && errno != EINTR)) {
      return false;
    }
  }
  return true;
}
}  // namespace tensorplate::serving
