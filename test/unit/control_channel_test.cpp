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
#include <condition_variable>
#include <cstdint>
#include <limits>
#include <mutex>
#include <optional>
#include <string>
#include <thread>
#include <utility>
#include <vector>

#include "tensorplate/http/http_message.hpp"
#include "tensorplate/serving/router.hpp"

#include "control_socket_harness.hpp"
#include "recording_control_target.hpp"
#include "serving/member_control.hpp"

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

/// A member's target behind a channel, with a router that has no
/// dependencies: one that admits a request answers it 415 for its missing
/// content type, before it would reach a pipeline.
struct Member {
  explicit Member(std::chrono::milliseconds contact_timeout = 10s, bool bind = true)
      : under_test(target, contact_timeout) {
    if (bind) {
      target.bind_router(router);
    }
  }

  [[nodiscard]] ipc::WorkerControlResponse ask(ipc::WorkerControlOp op) const {
    return under_test.ask(request_for(op, "c-" + std::to_string(++asked)));
  }

  [[nodiscard]] ipc::WorkerControlResponse retire(std::uint64_t drain_timeout_ms) const {
    auto request = request_for(ipc::WorkerControlOp::Retire, "r-" + std::to_string(++asked));
    request.drain_timeout_ms = drain_timeout_ms;
    return under_test.ask(request);
  }

  /// The message a stream Open is refused with; empty when it is admitted.
  [[nodiscard]] std::string session_refusal() const {
    const auto admitted = target.admit_session(*under_test.channel);
    if (admitted) {
      return {};
    }
    EXPECT_EQ(admitted.error().code, Error::Code::NotReady);
    EXPECT_EQ(admitted.error().context, "admission_closed");
    return admitted.error().message;
  }

  /// What /infer answers: 415 once admitted, else the refusal's status and
  /// message.
  [[nodiscard]] std::pair<int, std::string> infer() {
    const auto response = router.handle_infer(http::Request{});
    return {response.status, response.status == 415 ? std::string{} : response.body};
  }

  /// The windows and the asked drains of the retires that reached the worker.
  [[nodiscard]] std::vector<std::pair<std::chrono::milliseconds, std::chrono::milliseconds>>
  retires() const {
    const std::lock_guard guard(mu);
    return retired;
  }

  mutable std::mutex mu;
  std::vector<std::pair<std::chrono::milliseconds, std::chrono::milliseconds>> retired;
  mutable int asked = 0;
  RequestRouter router{{}};
  MemberControlTarget target{
      [this](std::chrono::milliseconds window, std::chrono::milliseconds asked_for) {
        const std::lock_guard guard(mu);
        retired.emplace_back(window, asked_for);
      }};
  ChannelUnderTest under_test;
};

bool refused_because(const std::pair<int, std::string>& answer, const std::string& cause) {
  return answer.first == 503 && answer.second.find("\"admission_closed\"") != std::string::npos &&
         answer.second.find("serving worker admission closed: " + cause) != std::string::npos;
}

using Op = ipc::WorkerControlOp;
using Status = ipc::WorkerControlStatus;

TEST(MemberControl, AFenceClosesAdmissionAndAnActivateReopensIt) {
  Member member;
  member.target.set_session_ceiling(1);
  EXPECT_EQ(member.session_refusal(), "");
  EXPECT_EQ(member.infer().first, 415);

  EXPECT_EQ(member.ask(Op::AdmissionFence).status, Status::Ok);
  EXPECT_EQ(member.session_refusal(), "session admission is closed: fenced");
  EXPECT_TRUE(refused_because(member.infer(), "fenced")) << member.infer().second;

  const auto activated = member.ask(Op::Activate);
  EXPECT_EQ(activated.status, Status::Ok);
  EXPECT_TRUE(activated.quota.has_value());
  EXPECT_EQ(member.session_refusal(), "");
  EXPECT_EQ(member.infer().first, 415);
  EXPECT_TRUE(member.retires().empty());
}

TEST(MemberControl, RepeatedFencesAndActivatesAnswerOkAndChangeNothing) {
  Member member;
  member.target.set_session_ceiling(1);
  for (int round = 0; round < 2; ++round) {
    EXPECT_EQ(member.ask(Op::AdmissionFence).status, Status::Ok);
    EXPECT_EQ(member.ask(Op::AdmissionFence).status, Status::Ok);
    EXPECT_EQ(member.session_refusal(), "session admission is closed: fenced");
    const auto first = member.ask(Op::Activate);
    const auto second = member.ask(Op::Activate);
    EXPECT_EQ(first.status, Status::Ok);
    EXPECT_EQ(second.status, Status::Ok);
    EXPECT_EQ(first.quota, second.quota);
    EXPECT_EQ(member.session_refusal(), "");
  }
}

TEST(MemberControl, ARetireAsksForTheStopOnceAndARepeatOnlyAnswersOk) {
  Member member;
  EXPECT_EQ(member.ask(Op::Retire).status, Status::Ok);
  EXPECT_EQ(member.retires().size(), 1U);
  EXPECT_EQ(member.session_refusal(), "session admission is closed: retiring");
  EXPECT_TRUE(refused_because(member.infer(), "retiring")) << member.infer().second;

  EXPECT_EQ(member.retire(5'000).status, Status::Ok);
  // A fence does not turn a retiring member back into a fenced one.
  EXPECT_EQ(member.ask(Op::AdmissionFence).status, Status::Ok);
  EXPECT_EQ(member.retires().size(), 1U);
  EXPECT_EQ(member.session_refusal(), "session admission is closed: retiring");
  EXPECT_TRUE(refused_because(member.infer(), "retiring")) << member.infer().second;
}

TEST(MemberControl, AnActivateAfterARetireIsAnErrorAndAdmissionStaysClosed) {
  Member member;
  member.target.set_session_ceiling(1);
  EXPECT_EQ(member.ask(Op::Retire).status, Status::Ok);
  const auto activated = member.ask(Op::Activate);
  EXPECT_EQ(activated.status, Status::Error);
  EXPECT_FALSE(activated.quota.has_value());
  ASSERT_TRUE(activated.error.has_value());
  EXPECT_EQ(activated.error->code, Error::Code::Unavailable);
  EXPECT_EQ(activated.error->context, "member_retiring");
  EXPECT_EQ(member.session_refusal(), "session admission is closed: retiring");
  EXPECT_TRUE(refused_because(member.infer(), "retiring")) << member.infer().second;
}

TEST(MemberControl, ARetireDrainsForWhatItAsksUpToThirtySeconds) {
  const Member brief;
  EXPECT_EQ(brief.retire(300).status, Status::Ok);
  ASSERT_EQ(brief.retires().size(), 1U);
  EXPECT_EQ(brief.retires().front(), std::pair(300ms, 300ms));

  const Member patient;
  EXPECT_EQ(patient.retire(600'000).status, Status::Ok);
  ASSERT_EQ(patient.retires().size(), 1U);
  EXPECT_EQ(patient.retires().front(), std::pair(30'000ms, 600'000ms));
  EXPECT_EQ(MemberControlTarget::retirement_window(30'000ms), 30'000ms);
  EXPECT_EQ(MemberControlTarget::retirement_window(30'001ms), 30'000ms);
}

TEST(MemberControl, TheLargestDrainARetireCanNameWaitsThirtySeconds) {
  const Member member;
  EXPECT_EQ(member.retire(std::numeric_limits<std::uint64_t>::max()).status, Status::Ok);
  ASSERT_EQ(member.retires().size(), 1U);
  EXPECT_EQ(member.retires().front().first, 30'000ms);
  EXPECT_EQ(member.retires().front().second, std::chrono::milliseconds::max());
}

TEST(MemberControl, ARouterBoundAfterAFenceRefusesFromTheStart) {
  Member member{10s, false};
  EXPECT_EQ(member.ask(Op::AdmissionFence).status, Status::Ok);
  EXPECT_EQ(member.infer().first, 415) << "no router was bound";
  member.target.bind_router(member.router);
  EXPECT_TRUE(refused_because(member.infer(), "fenced")) << member.infer().second;
}

TEST(MemberControl, AnOpenIsRefusedWhileContactIsLostAndInferIsNot) {
  Member member{400ms};
  member.target.set_session_ceiling(1);
  ASSERT_TRUE(member.under_test.admission_becomes("control_contact_lost"));
  EXPECT_EQ(member.session_refusal(), "session admission is closed: lost contact with the agent");
  EXPECT_EQ(member.infer().first, 415);

  // The agent's next request is contact again.
  EXPECT_EQ(member.ask(Op::LedgerStatus).status, Status::Ok);
  EXPECT_EQ(member.session_refusal(), "");
  // A fenced member says so, whether or not the agent is in contact.
  EXPECT_EQ(member.ask(Op::AdmissionFence).status, Status::Ok);
  ASSERT_TRUE(member.under_test.admission_becomes("control_contact_lost"));
  EXPECT_EQ(member.session_refusal(), "session admission is closed: fenced");
  EXPECT_EQ(member.ask(Op::Activate).status, Status::Ok);
  EXPECT_EQ(member.session_refusal(), "");

  // A channel that ended refuses Opens for good.
  member.under_test.pair.agent.reset();
  ASSERT_TRUE(member.under_test.admission_becomes("control_channel_closed"));
  EXPECT_EQ(member.session_refusal(), "session admission is closed: lost contact with the agent");
  EXPECT_EQ(member.infer().first, 415);
}

TEST(MemberControl, ActivateBeforeTheWorkerIsBuiltIsNotReady) {
  Member member;
  EXPECT_EQ(member.ask(Op::AdmissionFence).status, Status::Ok);
  const auto early = member.ask(Op::Activate);
  EXPECT_EQ(early.status, Status::NotReady);
  ASSERT_TRUE(early.error.has_value());
  EXPECT_EQ(early.error->context, "member_loading");
  EXPECT_EQ(member.session_refusal(), "session admission is closed: fenced");

  member.target.set_session_ceiling(1);
  EXPECT_EQ(member.ask(Op::Activate).status, Status::Ok);
  EXPECT_EQ(member.session_refusal(), "");
}

// The answers come over the channel, which answers `error` for a quota the
// wire format refuses.
TEST(MemberControl, ActivateAnswersTheStreamSessionCeilingAsTheQuota) {
  Member unary;
  unary.target.set_session_ceiling(0);
  const auto none = unary.ask(Op::Activate);
  EXPECT_EQ(none.status, Status::Ok);
  EXPECT_EQ(none.quota, ipc::WorkerQuota{});

  Member streaming;
  streaming.target.set_session_ceiling(1);
  const auto one = streaming.ask(Op::Activate);
  EXPECT_EQ(one.status, Status::Ok);
  ipc::WorkerQuota expected;
  expected.session_count = 1;
  expected.domain_bytes.guest_ram = 0;
  EXPECT_EQ(one.quota, expected);
}

TEST(MemberControl, QuotaAndPressureStayUnsupportedAndTheLedgerEmpty) {
  const Member member;
  const auto ledger = member.ask(Op::LedgerStatus);
  EXPECT_EQ(ledger.status, Status::Ok);
  EXPECT_EQ(ledger.ledger, ipc::WorkerLedger{});
  for (const auto op : {Op::QuotaAssign, Op::PressureDirective}) {
    const auto response = member.ask(op);
    EXPECT_EQ(response.status, Status::Unsupported);
    ASSERT_TRUE(response.error.has_value());
    EXPECT_EQ(response.error->context, "control_operation_unsupported");
  }
}

// Every thread waits at one gate, so that ThreadSanitizer sees the reads of
// the admission and the operations that change it run together.
TEST(MemberControl, ControlCallsAndAdmissionReadsOnOtherThreadsDoNotRace) {
  Member member{10s, false};
  std::mutex gate_mu;
  std::condition_variable gate_cv;
  bool open = false;
  std::atomic<bool> done{false};
  const auto wait_for_gate = [&] {
    std::unique_lock lock(gate_mu);
    gate_cv.wait(lock, [&] { return open; });
  };
  std::vector<std::thread> readers;
  readers.emplace_back([&] {
    wait_for_gate();
    while (!done.load()) {
      (void)member.target.admit_session(*member.under_test.channel);
    }
  });
  readers.emplace_back([&] {
    wait_for_gate();
    while (!done.load()) {
      (void)member.router.handle_infer(http::Request{});
    }
  });
  readers.emplace_back([&] {
    wait_for_gate();
    member.target.set_session_ceiling(1);
    member.target.bind_router(member.router);
  });
  {
    const std::lock_guard guard(gate_mu);
    open = true;
  }
  gate_cv.notify_all();
  for (int round = 0; round < 20; ++round) {
    EXPECT_EQ(member.ask(Op::AdmissionFence).status, Status::Ok);
    (void)member.ask(Op::Activate);
  }
  EXPECT_EQ(member.ask(Op::Retire).status, Status::Ok);
  done.store(true);
  for (auto& reader : readers) {
    reader.join();
  }
  EXPECT_EQ(member.session_refusal(), "session admission is closed: retiring");
  EXPECT_TRUE(refused_because(member.infer(), "retiring")) << member.infer().second;
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
