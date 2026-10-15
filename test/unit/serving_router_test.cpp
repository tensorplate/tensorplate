// SPDX-License-Identifier: Apache-2.0
//
// The router's refusals of new requests before it reads them. A router with no
// dependencies answers an admitted request without a content type with 415,
// before anything of it reaches a pipeline.

#include <gtest/gtest.h>

#include <nlohmann/json.hpp>
#include <string>
#include <utility>

#include "tensorplate/serving/router.hpp"

namespace tensorplate::serving {
namespace {

using Json = nlohmann::json;

http::Request infer_request(std::string path) {
  http::Request request;
  request.method = "POST";
  request.path = std::move(path);
  request.correlation_id = "cid-under-test";
  return request;
}

std::string header_of(const http::Response& response, const std::string& name) {
  for (const auto& header : response.headers) {
    if (header.name == name) {
      return header.value;
    }
  }
  return {};
}

/// The error a refusal's body carries.
Json error_of(const http::Response& response) {
  const auto body = Json::parse(response.body, nullptr, false);
  EXPECT_TRUE(body.is_object()) << response.body;
  EXPECT_EQ(body.value("status", ""), "failure") << response.body;
  return body.is_object() ? body.value("error", Json::object()) : Json::object();
}

TEST(RequestRouter, AClosedRouterRefusesInferAndPolicyInferWith503) {
  RequestRouter router{{}};
  router.set_admission_closed("fenced");
  for (const auto& response : {router.handle_infer(infer_request("/infer")),
                               router.handle_policy_infer(infer_request("/policy/infer"))}) {
    EXPECT_EQ(response.status, 503);
    const auto error = error_of(response);
    EXPECT_EQ(error.value("code", ""), "not_ready");
    EXPECT_EQ(error.value("context", ""), "admission_closed");
    EXPECT_EQ(error.value("message", ""), "serving worker admission closed: fenced");
    EXPECT_EQ(header_of(response, "x-correlation-id"), "cid-under-test");
  }
}

TEST(RequestRouter, AReopenedRouterAdmitsAgainAndStoppingStillWins) {
  RequestRouter router{{}};
  router.set_admission_closed("retiring");
  ASSERT_EQ(router.handle_infer(infer_request("/infer")).status, 503);
  router.set_admission_closed(nullptr);
  EXPECT_EQ(router.handle_infer(infer_request("/infer")).status, 415);
  EXPECT_EQ(router.handle_policy_infer(infer_request("/policy/infer")).status, 415);

  router.set_admission_closed("fenced");
  router.set_stopping(true);
  for (const auto& response : {router.handle_infer(infer_request("/infer")),
                               router.handle_policy_infer(infer_request("/policy/infer"))}) {
    EXPECT_EQ(response.status, 503);
    const auto error = error_of(response);
    EXPECT_EQ(error.value("code", ""), "not_ready");
    EXPECT_FALSE(error.contains("context")) << response.body;
    EXPECT_NE(error.value("message", "").find("serving worker stopping"), std::string::npos)
        << response.body;
  }
}

}  // namespace
}  // namespace tensorplate::serving
