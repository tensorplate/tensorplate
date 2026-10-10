// SPDX-License-Identifier: Apache-2.0
//
// The stream listener's mapping from the session layer to the wire.

#include "serving/stream/stream_mapping.hpp"

#include <google/protobuf/descriptor.h>
#include <gtest/gtest.h>

#include <fstream>
#include <map>
#include <regex>
#include <sstream>
#include <string>

namespace {

namespace stream = tensorplate::serving::stream;
namespace wire = tensorplate::stream::v1;
using tensorplate::Error;

std::string upper(std::string text) {
  for (auto& c : text) {
    c = static_cast<char>(std::toupper(static_cast<unsigned char>(c)));
  }
  return text;
}

TEST(StreamMapping, EveryReasonCarriesTheCodeTheTaxonomyPairsItWith) {
  std::ifstream input{std::string{TP_SOURCE_DIR} + "/docs/observability/failure-reasons.md"};
  ASSERT_TRUE(input.is_open());
  std::stringstream text;
  text << input.rdbuf();
  const std::regex row{R"(^\|\s*`([a-z_]+)`\s*\|.*\|\s*`([a-z_]+)`\s*\|\s*$)"};
  std::map<std::string, std::string> codes;
  std::string line;
  while (std::getline(text, line)) {
    std::smatch match;
    if (std::regex_match(line, match, row)) {
      codes.emplace(match[1].str(), match[2].str());
    }
  }
  const auto* reasons = wire::FailureReason_descriptor();
  ASSERT_GT(reasons->value_count(), 1);
  for (int index = 1; index < reasons->value_count(); ++index) {
    const auto* value = reasons->value(index);
    const auto reason = static_cast<wire::FailureReason>(value->number());
    const std::string name =
        std::string{value->name()}.substr(std::string{"FAILURE_REASON_"}.size());
    SCOPED_TRACE(name);
    std::string lower = name;
    for (auto& c : lower) {
      c = static_cast<char>(std::tolower(static_cast<unsigned char>(c)));
    }
    const auto paired = codes.find(lower);
    ASSERT_NE(paired, codes.end()) << "failure-reasons.md has no row";
    EXPECT_EQ(wire::ErrorCode_Name(stream::taxonomy_code(reason)),
              "ERROR_CODE_" + upper(paired->second));
    const auto cause = stream::end_cause(Error::make(Error::Code::Internal, "m", lower));
    EXPECT_EQ(cause.reason(), reason) << "a context naming a reason maps to it";
  }
}

TEST(StreamMapping, AContextThatNamesNoReasonMapsByItsCode) {
  EXPECT_EQ(stream::failure_reason_of(Error::make(Error::Code::NotReady, "m", "session_ended")),
            wire::FAILURE_REASON_WORKER_NOT_READY);
  EXPECT_EQ(stream::failure_reason_of(
                Error::make(Error::Code::ConfigInvalid, "m", "invalid_output_item")),
            wire::FAILURE_REASON_CONFIG_INVALID);
  EXPECT_EQ(stream::failure_reason_of(
                Error::make(Error::Code::ResourceExhausted, "m", "session_key_exhausted")),
            wire::FAILURE_REASON_INTERNAL);
  EXPECT_EQ(stream::failure_reason_of(Error::make(Error::Code::InferenceFailed, "m")),
            wire::FAILURE_REASON_INTERNAL);
  EXPECT_EQ(stream::failure_reason_of(Error::make(Error::Code::Timeout, "m", "UNSPECIFIED")),
            wire::FAILURE_REASON_TIMEOUT);
}

TEST(StreamMapping, DetailIsCutToSixtyFourPrintableCharacters) {
  const auto cause = stream::end_cause(wire::FAILURE_REASON_INTERNAL, std::string(100, 'a') + "\n");
  EXPECT_EQ(cause.detail(), std::string(64, 'a'));
  EXPECT_EQ(stream::end_cause(wire::FAILURE_REASON_INTERNAL, "a\tb").detail(), "a?b");
}

TEST(StreamMapping, RefusalStatusCodesFollowTheContractTable) {
  const std::map<wire::FailureReason, grpc::StatusCode> expected{
      {wire::FAILURE_REASON_INVALID_EVENT, grpc::StatusCode::INVALID_ARGUMENT},
      {wire::FAILURE_REASON_BACKEND_UNSUPPORTED_CAPABILITY, grpc::StatusCode::FAILED_PRECONDITION},
      {wire::FAILURE_REASON_STALE_GENERATION, grpc::StatusCode::FAILED_PRECONDITION},
      {wire::FAILURE_REASON_SESSION_COUNT_LIMIT, grpc::StatusCode::RESOURCE_EXHAUSTED},
      {wire::FAILURE_REASON_ADMISSION_CLOSED, grpc::StatusCode::UNAVAILABLE},
      {wire::FAILURE_REASON_IDLE_TIMEOUT, grpc::StatusCode::DEADLINE_EXCEEDED},
      {wire::FAILURE_REASON_CLIENT_CANCELLED, grpc::StatusCode::CANCELLED},
      {wire::FAILURE_REASON_INTERNAL, grpc::StatusCode::INTERNAL},
  };
  for (const auto& [reason, code] : expected) {
    SCOPED_TRACE(wire::FailureReason_Name(reason));
    wire::OpenRefused refused;
    *refused.mutable_cause() = stream::end_cause(reason, "");
    refused.set_admission(wire::ADMISSION_NOT_ADMITTED);
    EXPECT_EQ(stream::refused_open_status(refused).error_code(), code);
  }
}

TEST(StreamMapping, ASessionEndsItsCallWithTheStatusOfItsCause) {
  // The stream's error mapping, then the class of the reason's error code.
  const std::map<std::string, grpc::StatusCode> expected{
      {"CONFIG_INVALID", grpc::StatusCode::INVALID_ARGUMENT},
      {"BUNDLE_SCHEMA_INVALID", grpc::StatusCode::INVALID_ARGUMENT},
      {"BUNDLE_INTEGRITY_FAILED", grpc::StatusCode::INVALID_ARGUMENT},
      {"EMPTY_INPUT", grpc::StatusCode::INVALID_ARGUMENT},
      {"INVALID_EVENT", grpc::StatusCode::INVALID_ARGUMENT},
      {"SHAPE_MISMATCH", grpc::StatusCode::INVALID_ARGUMENT},
      {"UNSUPPORTED_RUNTIME", grpc::StatusCode::FAILED_PRECONDITION},
      {"UNSUPPORTED_HARDWARE", grpc::StatusCode::FAILED_PRECONDITION},
      {"BACKEND_UNSUPPORTED_CAPABILITY", grpc::StatusCode::FAILED_PRECONDITION},
      {"PERMISSION_DENIED", grpc::StatusCode::FAILED_PRECONDITION},
      {"TARGET_UNRESOLVED", grpc::StatusCode::FAILED_PRECONDITION},
      {"TARGET_MISMATCH", grpc::StatusCode::FAILED_PRECONDITION},
      {"STALE_GENERATION", grpc::StatusCode::FAILED_PRECONDITION},
      {"ILLEGAL_TRANSITION", grpc::StatusCode::FAILED_PRECONDITION},
      {"SESSION_COUNT_LIMIT", grpc::StatusCode::RESOURCE_EXHAUSTED},
      {"INPUT_CREDIT_EXCEEDED", grpc::StatusCode::RESOURCE_EXHAUSTED},
      {"SLOW_CONSUMER", grpc::StatusCode::RESOURCE_EXHAUSTED},
      {"OOM", grpc::StatusCode::RESOURCE_EXHAUSTED},
      {"TIMEOUT", grpc::StatusCode::DEADLINE_EXCEEDED},
      {"DEADLINE_MISSED", grpc::StatusCode::DEADLINE_EXCEEDED},
      {"IDLE_TIMEOUT", grpc::StatusCode::DEADLINE_EXCEEDED},
      {"HEARTBEAT_TIMEOUT", grpc::StatusCode::DEADLINE_EXCEEDED},
      {"MAX_DURATION", grpc::StatusCode::DEADLINE_EXCEEDED},
      {"FINALIZE_TIMEOUT", grpc::StatusCode::DEADLINE_EXCEEDED},
      {"ADMISSION_CLOSED", grpc::StatusCode::UNAVAILABLE},
      {"WORKER_SHUTDOWN", grpc::StatusCode::UNAVAILABLE},
      {"DEPLOYMENT_RETIRED", grpc::StatusCode::UNAVAILABLE},
      {"WORKER_NOT_READY", grpc::StatusCode::UNAVAILABLE},
      {"WORKER_EXIT", grpc::StatusCode::UNAVAILABLE},
      {"WORKER_CRASH_LOOP", grpc::StatusCode::UNAVAILABLE},
      {"NO_HEARTBEAT", grpc::StatusCode::UNAVAILABLE},
      {"BACKEND_UNAVAILABLE", grpc::StatusCode::UNAVAILABLE},
      {"BACKEND_RESET", grpc::StatusCode::UNAVAILABLE},
      {"SIDECAR_PROCESS_EXIT", grpc::StatusCode::UNAVAILABLE},
      {"SIDECAR_STARTUP_FAILED", grpc::StatusCode::UNAVAILABLE},
      {"CLIENT_CANCELLED", grpc::StatusCode::CANCELLED},
      {"SIDECAR_MALFORMED_RESPONSE", grpc::StatusCode::INTERNAL},
      {"INTERNAL", grpc::StatusCode::INTERNAL},
  };
  const auto* reasons = wire::FailureReason_descriptor();
  for (int index = 1; index < reasons->value_count(); ++index) {
    const auto* value = reasons->value(index);
    const std::string name =
        std::string{value->name()}.substr(std::string{"FAILURE_REASON_"}.size());
    SCOPED_TRACE(name);
    const auto row = expected.find(name);
    ASSERT_NE(row, expected.end()) << "a reason with no status in this table";
    wire::SessionClosed closed;
    closed.set_state(wire::SESSION_STATE_FAILED);
    *closed.mutable_cause() =
        stream::end_cause(static_cast<wire::FailureReason>(value->number()), "");
    const auto status = stream::session_end_status(closed);
    EXPECT_EQ(status.error_code(), row->second);
    EXPECT_NE(status.error_message().find(value->name()), std::string::npos);
  }
  EXPECT_EQ(expected.size(), static_cast<std::size_t>(reasons->value_count() - 1));
}

TEST(StreamMapping, OnlyASessionClosedWithoutACauseEndsItsCallOk) {
  wire::SessionClosed closed;
  closed.set_state(wire::SESSION_STATE_CLOSED);
  EXPECT_TRUE(stream::session_end_status(closed).ok());
  closed.set_state(wire::SESSION_STATE_FAILED);
  EXPECT_EQ(stream::session_end_status(closed).error_code(), grpc::StatusCode::INTERNAL);
  closed.set_state(wire::SESSION_STATE_CLOSED);
  *closed.mutable_cause() = stream::end_cause(wire::FAILURE_REASON_WORKER_SHUTDOWN, "");
  EXPECT_EQ(stream::session_end_status(closed).error_code(), grpc::StatusCode::UNAVAILABLE);
}

TEST(StreamMapping, LocalhostIsTheIpv4LoopbackAndIpv6IsBracketed) {
  EXPECT_EQ(stream::listen_address("localhost", 7001), "127.0.0.1:7001");
  EXPECT_EQ(stream::listen_address("127.0.0.1", 0), "127.0.0.1:0");
  EXPECT_EQ(stream::listen_address("::1", 7001), "[::1]:7001");
}

}  // namespace
