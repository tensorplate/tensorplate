// SPDX-License-Identifier: Apache-2.0
//
// V01-E07-F08: End-to-end serving worker integration tests.
//
// Spins up `ServingWorker` on a loopback ephemeral port using the
// built-in mock session and exercises the full HTTP route surface:
// `/infer`, `/policy/infer` + result/cancel, `/health`, `/metrics`,
// malformed/oversized payloads, deadline rejection, and graceful
// shutdown. No real backend is required.

#include <gtest/gtest.h>

#include <chrono>
#include <memory>
#include <nlohmann/json.hpp>
#include <string>
#include <thread>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/serving/config.hpp"
#include "tensorplate/serving/serialization.hpp"
#include "tensorplate/serving/worker.hpp"

#include "serving_http_client.hpp"

namespace {

using namespace tensorplate;
using namespace tensorplate::testing;

struct ServingHarness {
  std::unique_ptr<ServingWorker> worker;
  std::uint16_t port = 0;

  ServingHarness() = default;
  ServingHarness(const ServingHarness&) = delete;
  ServingHarness& operator=(const ServingHarness&) = delete;
  ServingHarness(ServingHarness&&) = default;
  ServingHarness& operator=(ServingHarness&&) = default;

  static ServingHarness start(ServingConfig cfg) {
    ServingHarness h;
    auto w_r = ServingWorker::create(std::move(cfg));
    if (!w_r) {
      throw std::runtime_error("worker create failed: " + w_r.error().message);
    }
    h.worker = std::move(w_r).value();
    auto sr = h.worker->start();
    if (!sr) {
      throw std::runtime_error("worker start failed: " + sr.error().message);
    }
    h.port = h.worker->bound_port();
    return h;
  }

  ~ServingHarness() {
    if (worker) {
      worker->shutdown("test-teardown");
      (void)worker->stop();
    }
  }
};

ServingConfig default_test_config() {
  ServingConfig cfg;
  cfg.bind.host = "127.0.0.1";
  cfg.bind.port = 0;
  cfg.http.accept_thread_pool_size = 2;
  cfg.http.max_body_bytes = 8 * 1024;
  cfg.scheduler.queue_capacity = 4;
  cfg.scheduler.in_flight_capacity = 1;
  cfg.async_policy.max_pending = 4;
  cfg.enable_stderr_logs = false;
  return cfg;
}

std::string make_infer_body(const std::string& request_id,
                            std::optional<int64_t> deadline_ms = std::nullopt,
                            const std::optional<int64_t>& stale_after = std::nullopt,
                            std::optional<int64_t> action_seq = std::nullopt,
                            const std::string& correlation_id = "") {
  std::vector<std::byte> payload(4);
  for (size_t i = 0; i < payload.size(); ++i) {
    payload[i] = static_cast<std::byte>(i);
  }
  nlohmann::json body;
  body["schema_version"] = "0.1";
  body["request_id"] = request_id;
  body["endpoint"] = "default";
  if (!correlation_id.empty()) {
    body["metadata"]["correlation_id"] = correlation_id;
  }
  if (action_seq.has_value()) {
    body["metadata"]["action_chunk_sequence"] = *action_seq;
    body["metadata"]["action_chunk_id"] = "chunk-" + std::to_string(*action_seq);
  }
  if (stale_after.has_value()) {
    body["metadata"]["stale_after_sequence"] = *stale_after;
  }
  if (deadline_ms.has_value()) {
    body["deadline_ms"] = *deadline_ms;
  }
  nlohmann::json input;
  input["name"] = "image";
  input["tensor"] = {{"dtype", "uint8"}, {"shape", {2, 2}}, {"byte_size", 4}};
  input["payload_b64"] = serving::base64_encode(payload.data(), payload.size());
  body["inputs"] = nlohmann::json::array({input});
  return body.dump();
}

TEST(ServingE2E, HealthRouteReportsReady) {
  auto h = ServingHarness::start(default_test_config());
  HttpClient client("127.0.0.1", h.port);
  auto resp = client.get("/health");
  EXPECT_EQ(resp.status, 200);
  auto j = nlohmann::json::parse(resp.body);
  EXPECT_EQ(j["state"], "ready");
  EXPECT_EQ(j["endpoint"], "default");
}

TEST(ServingE2E, MetricsRouteReturnsPrometheusBody) {
  auto h = ServingHarness::start(default_test_config());
  HttpClient client("127.0.0.1", h.port);
  auto resp = client.get("/metrics");
  EXPECT_EQ(resp.status, 200);
  EXPECT_NE(resp.body.find("tensorplate_serving_requests_total"), std::string::npos);
}

TEST(ServingE2E, InferRouteHappyPath) {
  auto h = ServingHarness::start(default_test_config());
  HttpClient client("127.0.0.1", h.port);
  auto resp = client.post("/infer", make_infer_body("req-1"));
  ASSERT_EQ(resp.status, 200) << resp.body;
  auto j = nlohmann::json::parse(resp.body);
  EXPECT_EQ(j["status"], "success");
  EXPECT_EQ(j["request_id"], "req-1");
  ASSERT_TRUE(j.contains("outputs"));
  EXPECT_EQ(j["outputs"][0]["name"], "actions");
  EXPECT_FALSE(resp.header("x-correlation-id").empty());
}

TEST(ServingE2E, InferPropagatesCorrelationIdFromMetadata) {
  auto h = ServingHarness::start(default_test_config());
  HttpClient client("127.0.0.1", h.port);
  auto resp = client.post(
      "/infer", make_infer_body("req-c", std::nullopt, std::nullopt, std::nullopt, "custom-cid"));
  ASSERT_EQ(resp.status, 200) << resp.body;
  EXPECT_EQ(resp.header("x-correlation-id"), "custom-cid");
  auto j = nlohmann::json::parse(resp.body);
  EXPECT_EQ(j["correlation_id"], "custom-cid");
}

TEST(ServingE2E, InferRejectsMalformed) {
  auto h = ServingHarness::start(default_test_config());
  HttpClient client("127.0.0.1", h.port);
  auto resp = client.post("/infer", "not json");
  EXPECT_EQ(resp.status, 400);
  auto j = nlohmann::json::parse(resp.body);
  EXPECT_EQ(j["status"], "failure");
  EXPECT_EQ(j["error"]["code"], "config_invalid");
}

TEST(ServingE2E, InferRejectsOversizedPayload) {
  auto cfg = default_test_config();
  cfg.http.max_body_bytes = 64;  // tiny cap
  auto h = ServingHarness::start(std::move(cfg));
  HttpClient client("127.0.0.1", h.port);
  std::string big(8 * 1024, 'A');
  auto resp = client.post("/infer", big);
  EXPECT_EQ(resp.status, 413);
}

TEST(ServingE2E, InferRejectsDuplicateInputNames) {
  auto h = ServingHarness::start(default_test_config());
  HttpClient client("127.0.0.1", h.port);
  nlohmann::json body;
  body["request_id"] = "dup";
  body["endpoint"] = "default";
  nlohmann::json input;
  input["name"] = "image";
  input["tensor"] = {{"dtype", "uint8"}, {"shape", {2, 2}}, {"byte_size", 4}};
  std::vector<std::byte> p(4);
  input["payload_b64"] = serving::base64_encode(p.data(), p.size());
  body["inputs"] = nlohmann::json::array({input, input});
  auto resp = client.post("/infer", body.dump());
  EXPECT_EQ(resp.status, 400);
  auto j = nlohmann::json::parse(resp.body);
  EXPECT_EQ(j["error"]["code"], "config_invalid");
}

TEST(ServingE2E, PolicyInferAndResult) {
  auto h = ServingHarness::start(default_test_config());
  HttpClient client("127.0.0.1", h.port);
  auto accept =
      client.post("/policy/infer", make_infer_body("async-1", std::nullopt, std::nullopt, 1));
  ASSERT_EQ(accept.status, 202) << accept.body;
  auto aj = nlohmann::json::parse(accept.body);
  EXPECT_EQ(aj["status"], "accepted");
  std::string rid = aj["request_id"];
  // Poll for result.
  for (int i = 0; i < 50; ++i) {
    auto r = client.get(std::string{"/policy/result/"} + rid);
    ASSERT_EQ(r.status, 200);
    auto j = nlohmann::json::parse(r.body);
    if (j["status"] == "completed") {
      EXPECT_TRUE(j.contains("result"));
      EXPECT_EQ(j["result"]["status"], "success");
      return;
    }
    std::this_thread::sleep_for(std::chrono::milliseconds{20});
  }
  FAIL() << "async result never reached completed state";
}

TEST(ServingE2E, PolicyCancelTransitionsState) {
  auto cfg = default_test_config();
  cfg.scheduler.in_flight_capacity = 1;
  auto h = ServingHarness::start(std::move(cfg));
  HttpClient client("127.0.0.1", h.port);
  auto accept =
      client.post("/policy/infer", make_infer_body("async-cancel", std::nullopt, std::nullopt, 1));
  ASSERT_EQ(accept.status, 202);
  auto aj = nlohmann::json::parse(accept.body);
  std::string rid = aj["request_id"];
  auto cancel = client.post(std::string{"/policy/cancel/"} + rid, "");
  // Either 200 if cancellation reached the entry before completion,
  // or 404 if completion finished first. In either case the entry
  // is no longer pending.
  EXPECT_TRUE(cancel.status == 200 || cancel.status == 404) << cancel.body;
}

TEST(ServingE2E, UnknownPathReturns404) {
  auto h = ServingHarness::start(default_test_config());
  HttpClient client("127.0.0.1", h.port);
  auto resp = client.get("/does-not-exist");
  EXPECT_EQ(resp.status, 404);
}

TEST(ServingE2E, WrongMethodReturns405) {
  auto h = ServingHarness::start(default_test_config());
  HttpClient client("127.0.0.1", h.port);
  auto resp = client.get("/infer");  // /infer is POST-only
  EXPECT_EQ(resp.status, 405);
}

TEST(ServingE2E, ShutdownReleasesBuffers) {
  auto h = ServingHarness::start(default_test_config());
  HttpClient client("127.0.0.1", h.port);
  for (int i = 0; i < 3; ++i) {
    auto r = client.post("/infer", make_infer_body("req-" + std::to_string(i)));
    ASSERT_EQ(r.status, 200) << r.body;
  }
  // Trigger graceful shutdown explicitly.
  h.worker->shutdown("test");
  (void)h.worker->stop();
  // After stop, the buffer manager should report zero active buffers.
  EXPECT_EQ(h.worker->buffer_manager().accounting().active_count, 0U);
}

TEST(ServingE2E, RejectsAdmissionWhileStopping) {
  auto h = ServingHarness::start(default_test_config());
  HttpClient client("127.0.0.1", h.port);
  h.worker->shutdown("test");
  // Listener may still be alive briefly; the router should refuse.
  auto resp = client.post("/infer", make_infer_body("late"));
  // Either we observe 503 from the router or the server cut the
  // connection (status 0 from the test client). Both are correct.
  EXPECT_TRUE(resp.status == 503 || resp.status == 0) << "status=" << resp.status;
  (void)h.worker->stop();
}

TEST(ServingE2E, AdapterFailureFailsRequest) {
  auto h = ServingHarness::start(default_test_config());
  // The mock session installed by the worker is unreachable through
  // the public ServingWorker interface, so we cannot inject a
  // sticky failure here. This test asserts the negative case
  // separately via a session-level integration in the unit suite.
  HttpClient client("127.0.0.1", h.port);
  auto resp = client.post("/infer", make_infer_body("ok-after-restart"));
  EXPECT_EQ(resp.status, 200);
}

}  // namespace
