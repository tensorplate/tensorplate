// SPDX-License-Identifier: Apache-2.0
#include <gtest/gtest.h>

#include <array>
#include <fstream>
#include <iterator>
#include <nlohmann/json.hpp>
#include <string>
#include <string_view>
#include <vector>

#include "tensorplate/ipc/worker_control.hpp"

namespace tensorplate::ipc {
namespace {
constexpr std::array<std::string_view, 8> kFiles{
    "worker_control_activate.jsonl",        "worker_control_admission_fence.jsonl",
    "worker_control_error.jsonl",           "worker_control_ledger_status.jsonl",
    "worker_control_member_mismatch.jsonl", "worker_control_pressure_directive.jsonl",
    "worker_control_quota_assign.jsonl",    "worker_control_retire.jsonl"};

std::vector<std::string> fixture_frames(std::string_view name) {
  const std::string path =
      std::string(TP_SOURCE_DIR) + "/protocol/rust/tests/fixtures/" + std::string(name);
  std::ifstream stream(path, std::ios::binary);
  EXPECT_TRUE(stream.good()) << path;
  if (!stream.good()) {
    return {};
  }
  const std::string bytes{std::istreambuf_iterator<char>(stream), std::istreambuf_iterator<char>()};
  EXPECT_FALSE(bytes.empty());
  if (bytes.empty()) {
    return {};
  }
  EXPECT_EQ(bytes.back(), '\n');
  if (bytes.back() != '\n') {
    return {};
  }
  std::vector<std::string> frames;
  std::size_t first = 0;
  while (first < bytes.size()) {
    const auto end = bytes.find('\n', first);
    EXPECT_NE(end, std::string::npos);
    if (end == std::string::npos) {
      return {};
    }
    frames.push_back(bytes.substr(first, end + 1 - first));
    first = end + 1;
  }
  return frames;
}

TEST(WorkerControlCodec, RustGoldenFramesAreByteExact) {
  for (const auto name : kFiles) {
    SCOPED_TRACE(name);
    const auto frames = fixture_frames(name);
    ASSERT_FALSE(frames.empty());
    ASSERT_EQ(frames.size() % 2, 0U);
    for (std::size_t i = 0; i < frames.size(); i += 2) {
      const auto request = decode_worker_control_request(frames[i]);
      ASSERT_TRUE(request) << request.error().message;
      const auto encoded_request = encode_worker_control_request(*request);
      ASSERT_TRUE(encoded_request) << encoded_request.error().message;
      EXPECT_EQ(*encoded_request, frames[i]);
      const auto response = decode_worker_control_response(frames[i + 1]);
      ASSERT_TRUE(response) << response.error().message;
      const auto encoded_response = encode_worker_control_response(*response);
      ASSERT_TRUE(encoded_response) << encoded_response.error().message;
      EXPECT_EQ(*encoded_response, frames[i + 1]);
      EXPECT_TRUE(worker_control_answers(*response, *request));
    }
  }
}

TEST(WorkerControlCodec, RejectsMalformedFramesAndSemanticViolations) {
  struct Mutation {
    std::string_view fixture;
    std::size_t frame_index;
    std::string_view original;
    std::string_view replacement;
    Error::Code error_code = Error::Code::ConfigInvalid;
  };
  constexpr std::array<Mutation, 19> kMutations{{
      {"worker_control_retire.jsonl", 0, "\"0.1\"", "\"0.2\"", Error::Code::Unsupported},
      {"worker_control_retire.jsonl", 0, "\"correlation_id\"", "\"other\":1,\"correlation_id\""},
      {"worker_control_retire.jsonl", 0, "\"ctl-000003\"", "\"bad id\""},
      {"worker_control_retire.jsonl", 0, "\"tx-00000000-0000-4000-8000-000000000001\"",
       "\"runtime\""},
      {"worker_control_retire.jsonl", 0, "\"speech-tts\"", "\"../speech\""},
      {"worker_control_retire.jsonl", 0, "\"generation\":4", "\"generation\":0"},
      {"worker_control_retire.jsonl", 0, "\"generation\":4", "\"generation\":9007199254740992"},
      {"worker_control_retire.jsonl", 0, "\"drain_timeout_ms\":30000", "\"drain_timeout_ms\":0"},
      {"worker_control_retire.jsonl", 0, "\"retire\"", "\"prepare\""},
      {"worker_control_retire.jsonl", 0, "\"timeout_ms\":1000", "\"timeout_ms\":null"},
      {"worker_control_retire.jsonl", 0, "\"generation\":4", "\"generation\":4,\"generation\":5"},
      {"worker_control_quota_assign.jsonl", 0, "\"session_count\":1", "\"session_count\":2049"},
      {"worker_control_quota_assign.jsonl", 0, "\"guest_ram\":134217728",
       "\"shared_pool\":134217728"},
      {"worker_control_pressure_directive.jsonl", 2, "\"count\":1", "\"count\":0"},
      {"worker_control_pressure_directive.jsonl", 2, ",\"count\":1", ""},
      {"worker_control_pressure_directive.jsonl", 0, "\"shed_admission\"", "\"terminate_newest\""},
      {"worker_control_error.jsonl", 1, "\"not_ready\"", "\"unknown_code\""},
      {"worker_control_error.jsonl", 1, "\"message\"", "\"unexpected\":1,\"message\""},
      {"worker_control_retire.jsonl", 1, "\"status\":\"ok\"", "\"status\":\"error\""},
  }};
  for (const auto& mutation : kMutations) {
    SCOPED_TRACE(std::string(mutation.fixture) + " frame " + std::to_string(mutation.frame_index) +
                 " replace " + std::string(mutation.original));
    const auto frames = fixture_frames(mutation.fixture);
    ASSERT_GT(frames.size(), mutation.frame_index);
    if (mutation.frame_index % 2 == 0) {
      ASSERT_TRUE(decode_worker_control_request(frames[mutation.frame_index]));
    } else {
      ASSERT_TRUE(decode_worker_control_response(frames[mutation.frame_index]));
    }
    auto changed = frames[mutation.frame_index];
    const auto pos = changed.find(mutation.original);
    ASSERT_NE(pos, std::string::npos);
    changed.replace(pos, mutation.original.size(), mutation.replacement);
    if (mutation.frame_index % 2 == 0) {
      const auto result = decode_worker_control_request(changed);
      ASSERT_FALSE(result);
      EXPECT_EQ(result.error().code, mutation.error_code);
    } else {
      const auto result = decode_worker_control_response(changed);
      ASSERT_FALSE(result);
      EXPECT_EQ(result.error().code, mutation.error_code);
    }
  }
  const auto good_frames = fixture_frames("worker_control_retire.jsonl");
  ASSERT_FALSE(good_frames.empty());
  const auto& good = good_frames[0];
  EXPECT_FALSE(decode_worker_control_request(good.substr(0, good.size() - 1)));
  EXPECT_FALSE(decode_worker_control_request(good + "\n"));
  EXPECT_FALSE(decode_worker_control_request(std::string(kWorkerControlMaxFrameBytes, 'x') + "\n"));
}

TEST(WorkerControlCodec, RequiredAndForbiddenFieldsAreIndependent) {
  const auto request_frames = fixture_frames("worker_control_retire.jsonl");
  ASSERT_FALSE(request_frames.empty());
  auto request = nlohmann::ordered_json::parse(request_frames[0]);
  request.erase("correlation_id");
  EXPECT_FALSE(decode_worker_control_request(request.dump() + "\n"));

  const auto error_frames = fixture_frames("worker_control_error.jsonl");
  ASSERT_GT(error_frames.size(), 1U);
  auto error_response = nlohmann::ordered_json::parse(error_frames[1]);
  error_response["error"].erase("message");
  EXPECT_FALSE(decode_worker_control_response(error_response.dump() + "\n"));
  error_response = nlohmann::ordered_json::parse(error_frames[1]);
  error_response.erase("error");
  EXPECT_FALSE(decode_worker_control_response(error_response.dump() + "\n"));

  const auto ok_frames = fixture_frames("worker_control_activate.jsonl");
  ASSERT_GT(ok_frames.size(), 1U);
  auto ok_response = nlohmann::ordered_json::parse(ok_frames[1]);
  ok_response["error"] = {{"code", "not_ready"}, {"message", "failure"}};
  EXPECT_FALSE(decode_worker_control_response(ok_response.dump() + "\n"));
}

TEST(WorkerControlCodec, LongErrorIsBoundedWithoutBreakingUtf8) {
  const auto frames = fixture_frames("worker_control_error.jsonl");
  ASSERT_GT(frames.size(), 1U);
  auto response = decode_worker_control_response(frames[1]).value();
  const std::string fitting = std::string(1023, 'a') + "é" + std::string(2000, 'b');
  response.error->message = fitting;
  const auto unshortened = encode_worker_control_response(response);
  ASSERT_TRUE(unshortened) << unshortened.error().message;
  const auto decoded_unshortened = decode_worker_control_response(*unshortened);
  ASSERT_TRUE(decoded_unshortened) << decoded_unshortened.error().message;
  EXPECT_EQ(decoded_unshortened->error->message, fitting);

  response.error->message = std::string(63'000, 'a');
  for (int i = 0; i < 4000; ++i) {
    response.error->message += "é";
  }
  const auto encoded = encode_worker_control_response(response);
  ASSERT_TRUE(encoded) << encoded.error().message;
  EXPECT_LE(encoded->size(), kWorkerControlMaxFrameBytes);
  const auto decoded = decode_worker_control_response(*encoded);
  ASSERT_TRUE(decoded) << decoded.error().message;
  EXPECT_LT(decoded->error->message.size(), response.error->message.size());
  EXPECT_EQ(response.error->message.substr(0, decoded->error->message.size()),
            decoded->error->message);
  EXPECT_GT(decoded->error->message.size(), 63'000U);

  response.error->context = "ctx";
  const auto with_short_context = encode_worker_control_response(response);
  ASSERT_TRUE(with_short_context) << with_short_context.error().message;
  const auto decoded_with_short_context = decode_worker_control_response(*with_short_context);
  ASSERT_TRUE(decoded_with_short_context) << decoded_with_short_context.error().message;
  EXPECT_EQ(decoded_with_short_context->error->context, "ctx");
  EXPECT_LT(decoded_with_short_context->error->message.size(), response.error->message.size());

  response.error->message = fitting;
  response.error->context = std::string(70'000, 'c');
  const auto contextual = encode_worker_control_response(response);
  ASSERT_TRUE(contextual) << contextual.error().message;
  EXPECT_LE(contextual->size(), kWorkerControlMaxFrameBytes);
  const auto decoded_contextual = decode_worker_control_response(*contextual);
  ASSERT_TRUE(decoded_contextual) << decoded_contextual.error().message;
  EXPECT_EQ(decoded_contextual->error->message, fitting);
  ASSERT_TRUE(decoded_contextual->error->context);
  EXPECT_FALSE(decoded_contextual->error->context->empty());
  EXPECT_LT(decoded_contextual->error->context->size(), response.error->context->size());
}

TEST(WorkerControlCodec, RejectsMalformedResponseAndWrongAnswer) {
  const auto frames = fixture_frames("worker_control_ledger_status.jsonl");
  ASSERT_GT(frames.size(), 1U);
  auto response = frames[1];
  auto pos = response.find("\"active\":1");
  ASSERT_NE(pos, std::string::npos);
  response.replace(pos, std::string("\"active\":1").size(), "\"active\":2");
  EXPECT_FALSE(decode_worker_control_response(response));
  auto request = decode_worker_control_request(frames[0]).value();
  auto valid_response = decode_worker_control_response(frames[1]).value();
  valid_response.correlation_id = "other";
  EXPECT_FALSE(worker_control_answers(valid_response, request));
}
}  // namespace
}  // namespace tensorplate::ipc
