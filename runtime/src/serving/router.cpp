// SPDX-License-Identifier: Apache-2.0

#include "tensorplate/serving/router.hpp"

#include <chrono>
#include <cstdint>
#include <mutex>
#include <nlohmann/json.hpp>
#include <random>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/cleanup.hpp"
#include "tensorplate/buffer/ingress.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/scheduler/scheduler.hpp"
#include "tensorplate/serving/async_policy.hpp"
#include "tensorplate/serving/health.hpp"
#include "tensorplate/serving/metrics.hpp"
#include "tensorplate/serving/pipeline.hpp"
#include "tensorplate/serving/serialization.hpp"

namespace tensorplate::serving {

namespace {

std::string generate_correlation_id() {
  // Small, low-entropy ID generator suitable for local-only serving
  // tracing. Not a cryptographic identifier.
  static thread_local std::mt19937_64 rng{
      static_cast<std::uint64_t>(std::chrono::steady_clock::now().time_since_epoch().count())};
  std::uniform_int_distribution<std::uint64_t> dist;
  std::uint64_t v = dist(rng);
  std::string out = "cid-";
  out.reserve(16 + 4);
  const char* hex = "0123456789abcdef";
  for (int i = 15; i >= 0; --i) {
    out.push_back(hex[(v >> (i * 4)) & 0xF]);
  }
  return out;
}

int http_status_for_error(Error::Code code) {
  switch (code) {
    case Error::Code::ConfigInvalid:
    case Error::Code::ShapeMismatch:
      return 400;
    case Error::Code::Unsupported:
      return 415;
    case Error::Code::OOMError:
      return 429;
    case Error::Code::Timeout:
      return 504;
    case Error::Code::NotReady:
      return 503;
    case Error::Code::LoadFailed:
    case Error::Code::InferenceFailed:
    case Error::Code::Internal:
    default:
      return 500;
  }
}

bool has_json_content_type(const http::Request& req) {
  auto value = req.header("content-type");
  if (!value.has_value()) {
    return false;
  }
  const std::string lower = http::lower_ascii(*value);
  return lower == "application/json" || lower.starts_with("application/json;");
}

bool has_binary_content_type(const http::Request& req) {
  auto value = req.header("content-type");
  if (!value.has_value()) {
    return false;
  }
  const std::string lower = http::lower_ascii(*value);
  const std::string binary_type{kBinaryInferContentType};
  return lower == binary_type || lower.starts_with(binary_type + ";");
}

}  // namespace

RequestRouter::RequestRouter(RequestRouterDeps deps) : deps_(std::move(deps)) {}
RequestRouter::~RequestRouter() = default;

void RequestRouter::set_stopping(bool stopping) noexcept {
  stopping_.store(stopping);
}
bool RequestRouter::is_stopping() const noexcept {
  return stopping_.load();
}
std::string_view RequestRouter::endpoint() const noexcept {
  return deps_.endpoint;
}

http::Response RequestRouter::make_error_response(int http_status, std::string_view request_id,
                                                  std::string_view correlation_id, Error::Code code,
                                                  std::string_view message,
                                                  std::optional<std::string_view> detail) const {
  Error err{code, std::string{message},
            detail.has_value() ? std::optional<std::string>{std::string{*detail}} : std::nullopt};
  std::optional<std::string_view> cid_opt;
  if (!correlation_id.empty()) {
    cid_opt = correlation_id;
  }
  auto body = render_error_response(request_id, cid_opt, err);
  auto resp = http::Response::json(http_status, std::move(body));
  if (!correlation_id.empty()) {
    resp.set_header("x-correlation-id", std::string{correlation_id});
  }
  if (deps_.metrics != nullptr) {
    deps_.metrics->record_rejection(code);
  }
  return resp;
}

http::Response RequestRouter::handle_infer(const http::Request& req) {
  const auto t0 = std::chrono::steady_clock::now();
  if (deps_.metrics != nullptr) {
    deps_.metrics->increment_requests_total();
  }
  std::string correlation_id =
      req.correlation_id.empty() ? generate_correlation_id() : req.correlation_id;
  if (stopping_.load()) {
    return make_error_response(503, "", correlation_id, Error::Code::NotReady,
                               "serving worker stopping; not accepting new requests");
  }
  const bool binary_request = has_binary_content_type(req);
  if (!binary_request && !has_json_content_type(req)) {
    return make_error_response(
        415, "", correlation_id, Error::Code::Unsupported,
        "content-type must be application/json or " + std::string{kBinaryInferContentType});
  }
  if (req.body.size() > deps_.max_body_bytes) {
    if (deps_.metrics != nullptr) {
      deps_.metrics->increment_rejected_oversize();
    }
    return make_error_response(413, "", correlation_id, Error::Code::Unsupported,
                               "payload exceeds configured max_body_bytes");
  }
  auto decoded_r =
      binary_request ? decode_binary_infer_request(req.body) : decode_infer_request(req.body);
  if (!decoded_r) {
    return make_error_response(http_status_for_error(decoded_r.error().code), "", correlation_id,
                               decoded_r.error().code, decoded_r.error().message);
  }
  auto decoded = std::move(decoded_r).value();
  if (decoded.metadata.correlation_id.has_value()) {
    correlation_id = *decoded.metadata.correlation_id;
  } else {
    decoded.metadata.correlation_id = correlation_id;
  }
  // Build NamedInputs via the buffer plane.
  auto ingress = as_ingress_inputs(decoded);
  IngressLimits limits;
  limits.max_total_bytes = deps_.max_body_bytes;
  auto inputs_r = build_named_inputs(*deps_.buffer_manager, ingress, limits);
  if (!inputs_r) {
    return make_error_response(http_status_for_error(inputs_r.error().code), decoded.request_id,
                               correlation_id, inputs_r.error().code, inputs_r.error().message);
  }
  auto request_r = InferRequest::create_with_relative_deadline(
      decoded.request_id, decoded.endpoint, std::move(inputs_r).value(), decoded.metadata,
      decoded.relative_deadline);
  if (!request_r) {
    return make_error_response(http_status_for_error(request_r.error().code), decoded.request_id,
                               correlation_id, request_r.error().code, request_r.error().message);
  }
  if (deps_.metrics != nullptr) {
    const auto t1 = std::chrono::steady_clock::now();
    deps_.metrics->observe_ingress_ms(
        static_cast<double>(std::chrono::duration_cast<std::chrono::nanoseconds>(t1 - t0).count()) /
        1e6);
  }
  auto outcome = deps_.pipeline->run_sync(std::move(request_r).value());
  if (!outcome.result) {
    return make_error_response(http_status_for_error(outcome.result.error().code),
                               decoded.request_id, correlation_id, outcome.result.error().code,
                               outcome.result.error().message);
  }
  InferResult result = std::move(outcome.result).value();
  if (result.is_failure() && deps_.metrics != nullptr) {
    deps_.metrics->record_rejection(result.error().code);
  }
  if (binary_request && result.is_success()) {
    auto body_r = render_binary_infer_response_checked(result, *deps_.buffer_manager,
                                                       std::string_view{correlation_id});
    (void)release_partial_outputs(*deps_.buffer_manager, result.outputs());
    if (!body_r) {
      return make_error_response(http_status_for_error(body_r.error().code), decoded.request_id,
                                 correlation_id, body_r.error().code,
                                 "response serialization failed", body_r.error().message);
    }
    http::Response resp;
    resp.status = 200;
    resp.set_header("content-type", std::string{kBinaryInferContentType});
    resp.set_header("x-correlation-id", correlation_id);
    resp.body = std::move(body_r).value();
    return resp;
  }

  auto body_r = render_infer_response_checked(result, *deps_.buffer_manager,
                                              std::string_view{correlation_id});
  (void)release_partial_outputs(*deps_.buffer_manager, result.outputs());
  if (!body_r) {
    return make_error_response(http_status_for_error(body_r.error().code), decoded.request_id,
                               correlation_id, body_r.error().code, "response serialization failed",
                               body_r.error().message);
  }
  auto body = std::move(body_r).value();
  auto resp = http::Response::ok_json(std::move(body));
  resp.set_header("x-correlation-id", correlation_id);
  return resp;
}

http::Response RequestRouter::handle_policy_infer(const http::Request& req) {
  const auto t0 = std::chrono::steady_clock::now();
  if (deps_.metrics != nullptr) {
    deps_.metrics->increment_requests_total();
  }
  std::string correlation_id =
      req.correlation_id.empty() ? generate_correlation_id() : req.correlation_id;
  if (stopping_.load()) {
    return make_error_response(503, "", correlation_id, Error::Code::NotReady,
                               "serving worker stopping; not accepting new async requests");
  }
  if (!deps_.async_policy_supported) {
    return make_error_response(
        501, "", correlation_id, Error::Code::Unsupported,
        "async policy routes require backend capability supports_async=true");
  }
  if (!has_json_content_type(req)) {
    return make_error_response(415, "", correlation_id, Error::Code::Unsupported,
                               "content-type must be application/json");
  }
  if (req.body.size() > deps_.max_body_bytes) {
    if (deps_.metrics != nullptr) {
      deps_.metrics->increment_rejected_oversize();
    }
    return make_error_response(413, "", correlation_id, Error::Code::Unsupported,
                               "payload exceeds configured max_body_bytes");
  }
  auto decoded_r = decode_infer_request(req.body);
  if (!decoded_r) {
    return make_error_response(http_status_for_error(decoded_r.error().code), "", correlation_id,
                               decoded_r.error().code, decoded_r.error().message);
  }
  auto decoded = std::move(decoded_r).value();
  if (decoded.metadata.correlation_id.has_value()) {
    correlation_id = *decoded.metadata.correlation_id;
  } else {
    decoded.metadata.correlation_id = correlation_id;
  }
  // If the request marks a stale_after_sequence, kick the store +
  // scheduler before accepting the new request.
  if (decoded.metadata.stale_after_sequence.has_value() && deps_.scheduler != nullptr) {
    auto staled =
        deps_.async_store->mark_stale_before_sequence(*decoded.metadata.stale_after_sequence);
    for (const auto& id : staled) {
      (void)deps_.scheduler->cancel(id, CancellationReason::StaleSequence);
    }
  }
  // Build NamedInputs via the buffer plane.
  auto ingress = as_ingress_inputs(decoded);
  IngressLimits limits;
  limits.max_total_bytes = deps_.max_body_bytes;
  auto inputs_r = build_named_inputs(*deps_.buffer_manager, ingress, limits);
  if (!inputs_r) {
    return make_error_response(http_status_for_error(inputs_r.error().code), decoded.request_id,
                               correlation_id, inputs_r.error().code, inputs_r.error().message);
  }
  auto request_r = InferRequest::create_with_relative_deadline(
      decoded.request_id, decoded.endpoint, std::move(inputs_r).value(), decoded.metadata,
      decoded.relative_deadline);
  if (!request_r) {
    return make_error_response(http_status_for_error(request_r.error().code), decoded.request_id,
                               correlation_id, request_r.error().code, request_r.error().message);
  }
  if (deps_.metrics != nullptr) {
    const auto t1 = std::chrono::steady_clock::now();
    deps_.metrics->observe_ingress_ms(
        static_cast<double>(std::chrono::duration_cast<std::chrono::nanoseconds>(t1 - t0).count()) /
        1e6);
  }
  auto accept_r = deps_.pipeline->run_async(std::move(request_r).value(), *deps_.async_store);
  if (!accept_r) {
    return make_error_response(http_status_for_error(accept_r.error().code), decoded.request_id,
                               correlation_id, accept_r.error().code, accept_r.error().message);
  }
  const auto accepted = std::move(accept_r).value();
  nlohmann::json j;
  j["schema_version"] = "0.1";
  j["status"] = "accepted";
  j["request_id"] = accepted.request_id;
  j["correlation_id"] = correlation_id;
  j["endpoint"] = decoded.endpoint;
  j["result_url"] = std::string{"/policy/result/"} + accepted.request_id;
  j["cancel_url"] = std::string{"/policy/cancel/"} + accepted.request_id;
  auto resp = http::Response::json(202, j.dump());
  resp.set_header("x-correlation-id", correlation_id);
  return resp;
}

http::Response RequestRouter::handle_policy_result(const http::Request& req,
                                                   std::string_view request_id) {
  std::string correlation_id =
      req.correlation_id.empty() ? generate_correlation_id() : req.correlation_id;
  if (!deps_.async_policy_supported) {
    return make_error_response(
        501, std::string{request_id}, correlation_id, Error::Code::Unsupported,
        "async policy routes require backend capability supports_async=true");
  }
  if (request_id.empty()) {
    return make_error_response(400, "", correlation_id, Error::Code::ConfigInvalid,
                               "result lookup: missing request_id");
  }
  auto snap = deps_.async_store->snapshot(request_id);
  if (!snap.has_value()) {
    return make_error_response(404, std::string{request_id}, correlation_id, Error::Code::NotReady,
                               "result lookup: request_id not found");
  }
  nlohmann::json j;
  j["schema_version"] = "0.1";
  j["request_id"] = snap->request_id;
  j["correlation_id"] = correlation_id;
  j["status"] = std::string{to_string(snap->status)};
  if (snap->action_chunk_id.has_value()) {
    j["action_chunk_id"] = *snap->action_chunk_id;
  }
  if (snap->action_chunk_sequence.has_value()) {
    j["action_chunk_sequence"] = *snap->action_chunk_sequence;
  }
  if (snap->status == AsyncStatus::Completed && snap->result.has_value()) {
    auto result = deps_.async_store->take_completed_result(request_id);
    if (!result.has_value()) {
      return make_error_response(404, snap->request_id, correlation_id, Error::Code::NotReady,
                                 "result lookup: completed result was already delivered");
    }
    auto body_r = render_infer_response_checked(*result, *deps_.buffer_manager,
                                                std::string_view{correlation_id});
    (void)release_partial_outputs(*deps_.buffer_manager, result->outputs());
    if (!body_r) {
      return make_error_response(http_status_for_error(body_r.error().code), snap->request_id,
                                 correlation_id, body_r.error().code,
                                 "response serialization failed", body_r.error().message);
    }
    j["result"] = nlohmann::json::parse(std::move(body_r).value());
  } else if (snap->status == AsyncStatus::Failed && snap->error.has_value()) {
    j["error"] = {
        {"schema_version", "0.1"},
        {"code", std::string{to_string(snap->error->code)}},
        {"message", snap->error->message},
    };
  }
  auto resp = http::Response::json(200, j.dump());
  resp.set_header("x-correlation-id", correlation_id);
  return resp;
}

http::Response RequestRouter::handle_policy_cancel(const http::Request& req,
                                                   std::string_view request_id) {
  std::string correlation_id =
      req.correlation_id.empty() ? generate_correlation_id() : req.correlation_id;
  if (!deps_.async_policy_supported) {
    return make_error_response(
        501, std::string{request_id}, correlation_id, Error::Code::Unsupported,
        "async policy routes require backend capability supports_async=true");
  }
  if (request_id.empty()) {
    return make_error_response(400, "", correlation_id, Error::Code::ConfigInvalid,
                               "cancel: missing request_id");
  }
  bool found = deps_.async_store->cancel(request_id);
  if (deps_.scheduler != nullptr) {
    (void)deps_.scheduler->cancel(request_id, CancellationReason::ClientRequest);
  }
  nlohmann::json j;
  j["schema_version"] = "0.1";
  j["request_id"] = std::string{request_id};
  j["correlation_id"] = correlation_id;
  j["cancelled"] = found;
  auto resp = http::Response::json(found ? 200 : 404, j.dump());
  resp.set_header("x-correlation-id", correlation_id);
  return resp;
}

}  // namespace tensorplate::serving
