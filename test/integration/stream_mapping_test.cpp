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

}  // namespace
