// SPDX-License-Identifier: Apache-2.0
#include "serving/control_channel.hpp"

#include <arpa/inet.h>
#include <fcntl.h>
#include <gtest/gtest.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <unistd.h>

#include <atomic>
#include <chrono>
#include <cstdint>
#include <mutex>
#include <optional>
#include <string>
#include <thread>
#include <vector>

#include "control_socket_harness.hpp"
#include "recording_control_target.hpp"

namespace tensorplate::serving {
namespace {
using namespace std::chrono_literals;
using testing::ControlSocketPair;
using testing::FrameRead;
using testing::RecordingTarget;

const ipc::WorkerMember kMember{"speech-en", 7};

ipc::WorkerControlRequest request_for(ipc::WorkerControlOp op, const std::string& correlation,
                                      ipc::WorkerMember member = kMember) {
  ipc::WorkerControlRequest request;
  const bool transactional = op == ipc::WorkerControlOp::AdmissionFence ||
                             op == ipc::WorkerControlOp::Activate ||
                             op == ipc::WorkerControlOp::Retire;
  request.transaction_id = transactional ? "tx-41" : "runtime";
  request.correlation_id = correlation;
  request.op = op;
  request.member = std::move(member);
  if (op == ipc::WorkerControlOp::QuotaAssign) {
    request.quota = ipc::WorkerQuota{5, {std::nullopt, std::uint64_t{1} << 20U, std::nullopt}};
  }
  if (op == ipc::WorkerControlOp::PressureDirective) {
    request.pressure = ipc::WorkerPressure{ipc::WorkerPressureLevel::TerminateNewest, 2};
  }
  if (op == ipc::WorkerControlOp::Retire) {
    request.drain_timeout_ms = 30'000;
  }
  return request;
}

/// A channel over one end of a socket pair, and the agent's end.
struct ChannelUnderTest {
  explicit ChannelUnderTest(ControlTarget& target, std::chrono::milliseconds contact_timeout = 10s)
      : pair(ControlSocketPair::create()),
        channel(ControlChannel::start(ControlSocket{pair.worker.release()}, kMember, target,
                                      contact_timeout)
                    .value()) {}

  /// Sends `request` and returns the response that answers it.
  ipc::WorkerControlResponse ask(const ipc::WorkerControlRequest& request) const {
    const auto frame = ipc::encode_worker_control_request(request);
    EXPECT_TRUE(frame.has_value());
    EXPECT_TRUE(testing::send_frame(pair.agent.get(), frame.value_or("")));
    std::string reply;
    EXPECT_EQ(testing::read_frame(pair.agent.get(), 1s, reply), FrameRead::Frame);
    const auto response = ipc::decode_worker_control_response(reply);
    EXPECT_TRUE(response.has_value()) << reply;
    EXPECT_TRUE(response && ipc::worker_control_answers(*response, request).has_value());
    return response.value_or(ipc::WorkerControlResponse{});
  }

  /// Waits until admission answers with `reason` ("" for open).
  [[nodiscard]] bool admission_becomes(const std::string& reason) const {
    const auto give_up = std::chrono::steady_clock::now() + 5s;
    for (;;) {
      const auto admission = channel->admission();
      if ((admission ? std::string{} : admission.error().context.value_or("?")) == reason) {
        return true;
      }
      if (std::chrono::steady_clock::now() >= give_up) {
        return false;
      }
      std::this_thread::sleep_for(2ms);
    }
  }

  ControlSocketPair pair;
  std::unique_ptr<ControlChannel> channel;
};

TEST(ControlChannel, LedgerStatusIsAnsweredFromTheTarget) {
  RecordingTarget target;
  const ChannelUnderTest under_test(target);
  const auto response = under_test.ask(request_for(ipc::WorkerControlOp::LedgerStatus, "poll-1"));
  EXPECT_EQ(response.status, ipc::WorkerControlStatus::Ok);
  EXPECT_EQ(response.member, kMember);
  EXPECT_EQ(response.ledger, target.ledger);
  EXPECT_FALSE(response.error.has_value());
  EXPECT_EQ(target.calls(), std::vector<std::string>{"ledger_status"});
}

TEST(ControlChannel, EveryOperationReachesItsTargetMethodWithItsPayload) {
  RecordingTarget target;
  const ChannelUnderTest under_test(target);
  using Op = ipc::WorkerControlOp;
  const auto fenced = under_test.ask(request_for(Op::AdmissionFence, "c1"));
  EXPECT_EQ(fenced.status, ipc::WorkerControlStatus::Ok);
  const auto activated = under_test.ask(request_for(Op::Activate, "c2"));
  EXPECT_EQ(activated.status, ipc::WorkerControlStatus::Ok);
  EXPECT_EQ(activated.quota, target.quota);
  const auto retired = under_test.ask(request_for(Op::Retire, "c3"));
  EXPECT_EQ(retired.status, ipc::WorkerControlStatus::NotReady);
  ASSERT_TRUE(retired.error.has_value());
  EXPECT_EQ(retired.error->code, Error::Code::NotReady);
  const auto assigned = under_test.ask(request_for(Op::QuotaAssign, "c4"));
  EXPECT_EQ(assigned.status, ipc::WorkerControlStatus::Ok);
  ASSERT_TRUE(assigned.quota.has_value());
  EXPECT_EQ(assigned.quota->session_count, 5U);
  const auto pressed = under_test.ask(request_for(Op::PressureDirective, "c5"));
  EXPECT_EQ(pressed.status, ipc::WorkerControlStatus::Timeout);
  EXPECT_EQ(target.calls(), (std::vector<std::string>{"admission_fence", "activate", "retire 30000",
                                                      "assign_quota 5", "apply_pressure 2"}));
}

TEST(ControlChannel, RequestForAnotherMemberIsAnsweredWithThisOneAndReachesNoTarget) {
  RecordingTarget target;
  const ChannelUnderTest under_test(target);
  for (const auto& other : {ipc::WorkerMember{"speech-en", 6}, ipc::WorkerMember{"speech-en", 8},
                            ipc::WorkerMember{"speech-ar", 7}}) {
    const auto response = under_test.ask(request_for(ipc::WorkerControlOp::Retire, "stale", other));
    EXPECT_EQ(response.status, ipc::WorkerControlStatus::MemberMismatch);
    EXPECT_EQ(response.member, kMember);
    EXPECT_FALSE(response.error.has_value());
  }
  EXPECT_TRUE(target.calls().empty());
}

TEST(ControlChannel, UnimplementedTargetReportsAnEmptyLedgerAndRefusesTheRest) {
  UnimplementedControlTarget target;
  const ChannelUnderTest under_test(target);
  using Op = ipc::WorkerControlOp;
  const auto ledger = under_test.ask(request_for(Op::LedgerStatus, "l"));
  EXPECT_EQ(ledger.status, ipc::WorkerControlStatus::Ok);
  EXPECT_EQ(ledger.ledger, ipc::WorkerLedger{});
  for (const auto op :
       {Op::AdmissionFence, Op::Activate, Op::Retire, Op::QuotaAssign, Op::PressureDirective}) {
    const auto response = under_test.ask(request_for(op, "u"));
    EXPECT_EQ(response.status, ipc::WorkerControlStatus::Unsupported);
    ASSERT_TRUE(response.error.has_value());
    EXPECT_EQ(response.error->code, Error::Code::Unsupported);
    EXPECT_FALSE(response.quota.has_value());
  }
}

TEST(ControlChannel, TargetAnswerTheWireFormatRefusesIsReportedAsAnError) {
  RecordingTarget target;
  target.ledger = ipc::WorkerLedger{5, 5, 0, 8, {}};
  const ChannelUnderTest under_test(target);
  const auto response = under_test.ask(request_for(ipc::WorkerControlOp::LedgerStatus, "bad"));
  EXPECT_EQ(response.status, ipc::WorkerControlStatus::Error);
  EXPECT_FALSE(response.ledger.has_value());
  ASSERT_TRUE(response.error.has_value());
  EXPECT_EQ(response.error->code, Error::Code::Internal);
  target.ledger = ipc::WorkerLedger{};
  EXPECT_EQ(under_test.ask(request_for(ipc::WorkerControlOp::LedgerStatus, "good")).status,
            ipc::WorkerControlStatus::Ok);
}

TEST(ControlChannel, FrameThatDoesNotDecodeIsSkippedAndTheNextIsAnswered) {
  RecordingTarget target;
  const ChannelUnderTest under_test(target);
  ASSERT_TRUE(testing::send_frame(under_test.pair.agent.get(), "{\"not\":\"a request\"}\n"));
  ASSERT_TRUE(testing::send_frame(under_test.pair.agent.get(), "\n"));
  const auto response =
      under_test.ask(request_for(ipc::WorkerControlOp::LedgerStatus, "after-garbage"));
  EXPECT_EQ(response.status, ipc::WorkerControlStatus::Ok);
  EXPECT_EQ(target.calls().size(), 1U);
}

TEST(ControlChannel, RequestsSentTogetherAndInPiecesAreEachAnswered) {
  RecordingTarget target;
  const ChannelUnderTest under_test(target);
  const auto first = request_for(ipc::WorkerControlOp::LedgerStatus, "a");
  const auto second = request_for(ipc::WorkerControlOp::LedgerStatus, "b");
  const std::string both = ipc::encode_worker_control_request(first).value() +
                           ipc::encode_worker_control_request(second).value();
  ASSERT_TRUE(testing::send_frame(under_test.pair.agent.get(), both.substr(0, 9)));
  std::this_thread::sleep_for(20ms);
  ASSERT_TRUE(testing::send_frame(under_test.pair.agent.get(), both.substr(9)));
  for (const auto* request : {&first, &second}) {
    std::string reply;
    ASSERT_EQ(testing::read_frame(under_test.pair.agent.get(), 1s, reply), FrameRead::Frame);
    const auto response = ipc::decode_worker_control_response(reply);
    ASSERT_TRUE(response.has_value());
    EXPECT_TRUE(ipc::worker_control_answers(*response, *request).has_value());
  }
}

TEST(ControlChannel, InputPastTheFrameLimitWithoutANewlineEndsTheChannel) {
  RecordingTarget target;
  const ChannelUnderTest under_test(target);
  const std::string endless(ipc::kWorkerControlMaxFrameBytes, 'x');
  (void)testing::send_frame(under_test.pair.agent.get(), endless);
  std::string reply;
  EXPECT_EQ(testing::read_frame(under_test.pair.agent.get(), 5s, reply), FrameRead::Closed);
  EXPECT_TRUE(under_test.admission_becomes("control_channel_closed"));
  EXPECT_TRUE(target.calls().empty());
}

TEST(ControlChannel, ResponseTheAgentDoesNotTakeWithinTheWriteLimitEndsTheChannel) {
  RecordingTarget target;
  const ChannelUnderTest under_test(target);
  const auto frame =
      ipc::encode_worker_control_request(request_for(ipc::WorkerControlOp::LedgerStatus, "x"))
          .value();
  // Requests go in and no response is read, until the channel can neither
  // write a response nor read another request.
  const auto give_up = std::chrono::steady_clock::now() + 10s;
  while (under_test.channel->admission().has_value() &&
         std::chrono::steady_clock::now() < give_up) {
    if (::send(under_test.pair.agent.get(), frame.data(), frame.size(),
               testing::kSendWithoutSigpipe | MSG_DONTWAIT) < 0) {
      std::this_thread::sleep_for(5ms);
    }
  }
  EXPECT_TRUE(under_test.admission_becomes("control_channel_closed"));
}

TEST(ControlChannel, LineLongerThanTheFrameLimitEndsTheChannelEvenWithItsNewline) {
  RecordingTarget target;
  const ChannelUnderTest under_test(target);
  std::string line(ipc::kWorkerControlMaxFrameBytes + 1'000, 'x');
  line.back() = '\n';
  // The newline arrives in the piece that crosses the limit.
  ASSERT_TRUE(testing::send_frame(under_test.pair.agent.get(), line.substr(0, 65'000)));
  std::this_thread::sleep_for(20ms);
  (void)testing::send_frame(under_test.pair.agent.get(), line.substr(65'000));
  std::string reply;
  EXPECT_EQ(testing::read_frame(under_test.pair.agent.get(), 5s, reply), FrameRead::Closed);
  EXPECT_TRUE(under_test.admission_becomes("control_channel_closed"));
}

TEST(ControlChannel, InputThatNeverDecodesDoesNotPostponeLossOfContact) {
  RecordingTarget target;
  const ChannelUnderTest under_test(target, 100ms);
  // Lines arrive without a pause, so the channel never finds its socket idle.
  std::atomic<bool> stop{false};
  std::thread flood([&] {
    const std::string noise = "noise\n";
    while (!stop.load()) {
      (void)::send(under_test.pair.agent.get(), noise.data(), noise.size(),
                   testing::kSendWithoutSigpipe | MSG_DONTWAIT);
    }
  });
  const bool lost = under_test.admission_becomes("control_contact_lost");
  stop.store(true);
  flood.join();
  EXPECT_TRUE(lost);
}

TEST(ControlChannel, AgentClosingItsEndIsLossOfContactForGood) {
  RecordingTarget target;
  ChannelUnderTest under_test(target);
  EXPECT_TRUE(under_test.channel->admission().has_value());
  under_test.pair.agent.reset();
  EXPECT_TRUE(under_test.admission_becomes("control_channel_closed"));
  const auto admission = under_test.channel->admission();
  ASSERT_FALSE(admission.has_value());
  EXPECT_EQ(admission.error().code, Error::Code::Unavailable);
}

TEST(ControlChannel, SilencePastTheContactTimeoutClosesAdmissionUntilTheNextRequest) {
  RecordingTarget target;
  const ChannelUnderTest under_test(target, 60ms);
  EXPECT_TRUE(under_test.channel->admission().has_value());
  EXPECT_TRUE(under_test.admission_becomes("control_contact_lost"));
  const auto lost = under_test.channel->admission();
  ASSERT_FALSE(lost.has_value());
  EXPECT_EQ(lost.error().code, Error::Code::Unavailable);

  // A frame that does not decode is not contact.
  ASSERT_TRUE(testing::send_frame(under_test.pair.agent.get(), "noise\n"));
  std::this_thread::sleep_for(20ms);
  EXPECT_FALSE(under_test.channel->admission().has_value());

  // A request for another member is still the agent speaking.
  (void)under_test.ask(
      request_for(ipc::WorkerControlOp::LedgerStatus, "back", ipc::WorkerMember{"speech-en", 6}));
  EXPECT_TRUE(under_test.channel->admission().has_value());
  EXPECT_TRUE(under_test.admission_becomes("control_contact_lost"));
}

TEST(ControlChannel, RegularRequestsKeepContactPastTheTimeout) {
  RecordingTarget target;
  const ChannelUnderTest under_test(target, 150ms);
  for (int poll = 0; poll < 8; ++poll) {
    (void)under_test.ask(request_for(ipc::WorkerControlOp::LedgerStatus, "keep"));
    EXPECT_TRUE(under_test.channel->admission().has_value());
    std::this_thread::sleep_for(40ms);
  }
}

TEST(ControlChannel, StoppingTheChannelClosesTheAgentsEnd) {
  RecordingTarget target;
  ChannelUnderTest under_test(target);
  under_test.channel.reset();
  std::string reply;
  EXPECT_EQ(testing::read_frame(under_test.pair.agent.get(), 1s, reply), FrameRead::Closed);
}

TEST(ControlChannel, MemberTheProtocolCannotNameIsRefusedAtStart) {
  RecordingTarget target;
  for (const auto& member : {ipc::WorkerMember{"speech en", 7}, ipc::WorkerMember{"speech-en", 0},
                             ipc::WorkerMember{"", 7}}) {
    auto pair = ControlSocketPair::create();
    const auto channel =
        ControlChannel::start(ControlSocket{pair.worker.release()}, member, target);
    ASSERT_FALSE(channel.has_value());
    EXPECT_EQ(channel.error().code, Error::Code::ConfigInvalid);
    EXPECT_EQ(channel.error().context, "invalid_control_member");
  }
}

using testing::StdinReplaced;

TEST(InheritedControlSocket, IsMovedToAPrivateDescriptorAndFdZeroBecomesDevNull) {
  auto pair = ControlSocketPair::create();
  const auto worker_inode = testing::inode_of(pair.worker.get());
  const StdinReplaced replaced(pair.worker.get());
  pair.worker.reset();

  const auto taken = take_inherited_control_socket();
  ASSERT_TRUE(taken.has_value()) << taken.error().message;
  EXPECT_GT(taken->fd(), STDERR_FILENO);
  EXPECT_EQ(testing::inode_of(taken->fd()), worker_inode);
  EXPECT_NE(::fcntl(taken->fd(), F_GETFD) & FD_CLOEXEC, 0);

  struct stat input {};
  struct stat null_device {};
  ASSERT_EQ(::fstat(STDIN_FILENO, &input), 0);
  ASSERT_EQ(::stat("/dev/null", &null_device), 0);
  EXPECT_TRUE(S_ISCHR(input.st_mode));
  EXPECT_EQ(input.st_rdev, null_device.st_rdev);
  EXPECT_EQ(::fcntl(STDIN_FILENO, F_GETFD) & FD_CLOEXEC, 0);

  ASSERT_TRUE(testing::send_frame(pair.agent.get(), "ping\n"));
  std::string received;
  EXPECT_EQ(testing::read_frame(taken->fd(), 1s, received), FrameRead::Frame);
  EXPECT_EQ(received, "ping\n");
}

TEST(InheritedControlSocket, AnythingButAConnectedLocalStreamSocketIsAbsentAndLeftAlone) {
  int pipe_ends[2] = {-1, -1};
  ASSERT_EQ(::pipe(pipe_ends), 0);
  const testing::OwnedFd pipe_read{pipe_ends[0]};
  const testing::OwnedFd pipe_write{pipe_ends[1]};
  const testing::OwnedFd null_input{::open("/dev/null", O_RDONLY)};
  // Connected and local, but not a stream.
  int datagrams[2] = {-1, -1};
  ASSERT_EQ(::socketpair(AF_UNIX, SOCK_DGRAM, 0, datagrams), 0);
  const testing::OwnedFd datagram{datagrams[0]};
  const testing::OwnedFd datagram_peer{datagrams[1]};
  const testing::OwnedFd unconnected{::socket(AF_UNIX, SOCK_STREAM, 0)};
  // A listening TCP socket, and a connected one: streams, but not local.
  const testing::OwnedFd listener{::socket(AF_INET, SOCK_STREAM, 0)};
  sockaddr_in address{};
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  socklen_t size = sizeof(address);
  // NOLINTBEGIN(cppcoreguidelines-pro-type-reinterpret-cast)
  auto* generic = reinterpret_cast<sockaddr*>(&address);
  ASSERT_EQ(::bind(listener.get(), generic, size), 0);
  ASSERT_EQ(::listen(listener.get(), 1), 0);
  ASSERT_EQ(::getsockname(listener.get(), generic, &size), 0);
  const testing::OwnedFd tcp_client{::socket(AF_INET, SOCK_STREAM, 0)};
  ASSERT_EQ(::connect(tcp_client.get(), generic, size), 0);
  // NOLINTEND(cppcoreguidelines-pro-type-reinterpret-cast)
  const testing::OwnedFd tcp_accepted{::accept(listener.get(), nullptr, nullptr)};
  ASSERT_GE(tcp_accepted.get(), 0);
  for (const int fd : {pipe_read.get(), null_input.get(), datagram.get(), unconnected.get(),
                       listener.get(), tcp_accepted.get()}) {
    const auto before = testing::inode_of(fd);
    const StdinReplaced replaced(fd);
    const auto taken = take_inherited_control_socket();
    ASSERT_FALSE(taken.has_value());
    EXPECT_EQ(taken.error().code, Error::Code::Unavailable);
    EXPECT_EQ(taken.error().context, "control_descriptor_absent");
    EXPECT_EQ(testing::inode_of(STDIN_FILENO), before);
  }
}

TEST(InheritedControlSocket, SocketWhoseAgentEndIsClosedIsRefused) {
  auto pair = ControlSocketPair::create();
  const StdinReplaced replaced(pair.worker.get());
  pair.worker.reset();
  pair.agent.reset();
  const auto taken = take_inherited_control_socket();
  ASSERT_FALSE(taken.has_value());
  EXPECT_EQ(taken.error().code, Error::Code::Unavailable);
  EXPECT_EQ(taken.error().context, "control_peer_closed");
}
}  // namespace
}  // namespace tensorplate::serving
