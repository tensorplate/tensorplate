// SPDX-License-Identifier: Apache-2.0
#include "serving/stream/stream_mapping.hpp"

#include <algorithm>
#include <cctype>
#include <string>
#include <utility>

namespace tensorplate::serving::stream {
namespace {

constexpr std::size_t kMaxDetail = 64;
constexpr std::size_t kMaxShortText = 64;
constexpr std::size_t kMaxDeploymentId = 128;
constexpr int kMaxCapabilities = 32;

bool printable_ascii(std::string_view text) noexcept {
  return std::all_of(text.begin(), text.end(), [](char c) { return c >= 0x20 && c <= 0x7e; });
}

bool short_text(std::string_view text, bool required) noexcept {
  return (!required || !text.empty()) && text.size() <= kMaxShortText && printable_ascii(text);
}

bool names_a_deployment(std::string_view id) noexcept {
  return !id.empty() && id.size() <= kMaxDeploymentId && id != "." && id != ".." &&
         std::all_of(id.begin(), id.end(), [](char c) {
           return std::isalnum(static_cast<unsigned char>(c)) != 0 || c == '.' || c == '_' ||
                  c == '-';
         });
}

Error invalid_event(std::string message) {
  return Error::make(Error::Code::ConfigInvalid, std::move(message), "invalid_event");
}

Error unsupported(std::string message) {
  return Error::make(Error::Code::Unsupported, std::move(message),
                     "backend_unsupported_capability");
}

wire::FailureReason reason_for_code(Error::Code code) noexcept {
  switch (code) {
    case Error::Code::ConfigInvalid:
      return wire::FAILURE_REASON_CONFIG_INVALID;
    case Error::Code::LoadFailed:
      return wire::FAILURE_REASON_BACKEND_UNAVAILABLE;
    case Error::Code::NotReady:
      return wire::FAILURE_REASON_WORKER_NOT_READY;
    case Error::Code::ShapeMismatch:
      return wire::FAILURE_REASON_SHAPE_MISMATCH;
    case Error::Code::Unsupported:
      return wire::FAILURE_REASON_BACKEND_UNSUPPORTED_CAPABILITY;
    case Error::Code::OOMError:
      return wire::FAILURE_REASON_OOM;
    case Error::Code::Timeout:
      return wire::FAILURE_REASON_TIMEOUT;
    case Error::Code::Cancelled:
      return wire::FAILURE_REASON_CLIENT_CANCELLED;
    case Error::Code::Unavailable:
      return wire::FAILURE_REASON_BACKEND_UNAVAILABLE;
    case Error::Code::ResourceExhausted:
    case Error::Code::InferenceFailed:
    case Error::Code::Internal:
      break;
  }
  return wire::FAILURE_REASON_INTERNAL;
}

}  // namespace

wire::FailureReason failure_reason_of(const Error& error) {
  if (error.context && !error.context->empty() && printable_ascii(*error.context)) {
    std::string name = "FAILURE_REASON_" + *error.context;
    std::transform(name.begin(), name.end(), name.begin(),
                   [](unsigned char c) { return static_cast<char>(std::toupper(c)); });
    wire::FailureReason reason = wire::FAILURE_REASON_UNSPECIFIED;
    if (wire::FailureReason_Parse(name, &reason) && reason != wire::FAILURE_REASON_UNSPECIFIED) {
      return reason;
    }
  }
  return reason_for_code(error.code);
}

// The table of docs/observability/failure-reasons.md; a unit test holds the two
// together.
wire::ErrorCode taxonomy_code(wire::FailureReason reason) noexcept {
  switch (reason) {
    case wire::FAILURE_REASON_CONFIG_INVALID:
    case wire::FAILURE_REASON_BUNDLE_SCHEMA_INVALID:
    case wire::FAILURE_REASON_BUNDLE_INTEGRITY_FAILED:
    case wire::FAILURE_REASON_EMPTY_INPUT:
    case wire::FAILURE_REASON_INVALID_EVENT:
      return wire::ERROR_CODE_CONFIG_INVALID;
    case wire::FAILURE_REASON_UNSUPPORTED_RUNTIME:
    case wire::FAILURE_REASON_UNSUPPORTED_HARDWARE:
    case wire::FAILURE_REASON_BACKEND_UNSUPPORTED_CAPABILITY:
    case wire::FAILURE_REASON_PERMISSION_DENIED:
    case wire::FAILURE_REASON_TARGET_UNRESOLVED:
      return wire::ERROR_CODE_UNSUPPORTED;
    case wire::FAILURE_REASON_BACKEND_UNAVAILABLE:
    case wire::FAILURE_REASON_SIDECAR_STARTUP_FAILED:
    case wire::FAILURE_REASON_SIDECAR_PROCESS_EXIT:
      return wire::ERROR_CODE_LOAD_FAILED;
    case wire::FAILURE_REASON_SHAPE_MISMATCH:
      return wire::ERROR_CODE_SHAPE_MISMATCH;
    case wire::FAILURE_REASON_OOM:
      return wire::ERROR_CODE_OOM_ERROR;
    case wire::FAILURE_REASON_TIMEOUT:
    case wire::FAILURE_REASON_DEADLINE_MISSED:
    case wire::FAILURE_REASON_IDLE_TIMEOUT:
    case wire::FAILURE_REASON_HEARTBEAT_TIMEOUT:
    case wire::FAILURE_REASON_MAX_DURATION:
    case wire::FAILURE_REASON_FINALIZE_TIMEOUT:
      return wire::ERROR_CODE_TIMEOUT;
    case wire::FAILURE_REASON_SIDECAR_MALFORMED_RESPONSE:
      return wire::ERROR_CODE_INFERENCE_FAILED;
    case wire::FAILURE_REASON_WORKER_NOT_READY:
    case wire::FAILURE_REASON_WORKER_EXIT:
    case wire::FAILURE_REASON_WORKER_CRASH_LOOP:
    case wire::FAILURE_REASON_NO_HEARTBEAT:
    case wire::FAILURE_REASON_STALE_GENERATION:
    case wire::FAILURE_REASON_ILLEGAL_TRANSITION:
    case wire::FAILURE_REASON_ADMISSION_CLOSED:
    case wire::FAILURE_REASON_TARGET_MISMATCH:
      return wire::ERROR_CODE_NOT_READY;
    case wire::FAILURE_REASON_INPUT_CREDIT_EXCEEDED:
    case wire::FAILURE_REASON_SLOW_CONSUMER:
    case wire::FAILURE_REASON_SESSION_COUNT_LIMIT:
      return wire::ERROR_CODE_RESOURCE_EXHAUSTED;
    case wire::FAILURE_REASON_BACKEND_RESET:
    case wire::FAILURE_REASON_DEPLOYMENT_RETIRED:
    case wire::FAILURE_REASON_WORKER_SHUTDOWN:
      return wire::ERROR_CODE_UNAVAILABLE;
    case wire::FAILURE_REASON_CLIENT_CANCELLED:
      return wire::ERROR_CODE_CANCELLED;
    default:
      break;
  }
  return wire::ERROR_CODE_INTERNAL;
}

wire::EndCause end_cause(wire::FailureReason reason, std::string_view detail) {
  wire::EndCause cause;
  cause.set_reason(reason);
  cause.set_code(taxonomy_code(reason));
  std::string kept;
  for (const char c : detail.substr(0, kMaxDetail)) {
    kept.push_back(printable_ascii(std::string_view{&c, 1}) ? c : '?');
  }
  cause.set_detail(std::move(kept));
  return cause;
}

wire::EndCause end_cause(const Error& error) {
  return end_cause(failure_reason_of(error), error.message);
}

grpc::StatusCode status_code(wire::FailureReason reason) noexcept {
  switch (reason) {
    case wire::FAILURE_REASON_INVALID_EVENT:
    case wire::FAILURE_REASON_CONFIG_INVALID:
    case wire::FAILURE_REASON_EMPTY_INPUT:
      return grpc::StatusCode::INVALID_ARGUMENT;
    case wire::FAILURE_REASON_BACKEND_UNSUPPORTED_CAPABILITY:
    case wire::FAILURE_REASON_STALE_GENERATION:
    case wire::FAILURE_REASON_TARGET_UNRESOLVED:
    case wire::FAILURE_REASON_TARGET_MISMATCH:
    case wire::FAILURE_REASON_ILLEGAL_TRANSITION:
      return grpc::StatusCode::FAILED_PRECONDITION;
    case wire::FAILURE_REASON_SESSION_COUNT_LIMIT:
    case wire::FAILURE_REASON_INPUT_CREDIT_EXCEEDED:
    case wire::FAILURE_REASON_SLOW_CONSUMER:
    case wire::FAILURE_REASON_OOM:
      return grpc::StatusCode::RESOURCE_EXHAUSTED;
    case wire::FAILURE_REASON_TIMEOUT:
    case wire::FAILURE_REASON_DEADLINE_MISSED:
    case wire::FAILURE_REASON_IDLE_TIMEOUT:
    case wire::FAILURE_REASON_HEARTBEAT_TIMEOUT:
    case wire::FAILURE_REASON_MAX_DURATION:
    case wire::FAILURE_REASON_FINALIZE_TIMEOUT:
      return grpc::StatusCode::DEADLINE_EXCEEDED;
    case wire::FAILURE_REASON_ADMISSION_CLOSED:
    case wire::FAILURE_REASON_WORKER_SHUTDOWN:
    case wire::FAILURE_REASON_DEPLOYMENT_RETIRED:
    case wire::FAILURE_REASON_WORKER_NOT_READY:
    case wire::FAILURE_REASON_BACKEND_UNAVAILABLE:
    case wire::FAILURE_REASON_BACKEND_RESET:
    case wire::FAILURE_REASON_SIDECAR_PROCESS_EXIT:
    case wire::FAILURE_REASON_SIDECAR_STARTUP_FAILED:
      return grpc::StatusCode::UNAVAILABLE;
    case wire::FAILURE_REASON_CLIENT_CANCELLED:
      return grpc::StatusCode::CANCELLED;
    default:
      break;
  }
  switch (taxonomy_code(reason)) {
    case wire::ERROR_CODE_CONFIG_INVALID:
    case wire::ERROR_CODE_SHAPE_MISMATCH:
      return grpc::StatusCode::INVALID_ARGUMENT;
    case wire::ERROR_CODE_UNSUPPORTED:
      return grpc::StatusCode::FAILED_PRECONDITION;
    case wire::ERROR_CODE_OOM_ERROR:
    case wire::ERROR_CODE_RESOURCE_EXHAUSTED:
      return grpc::StatusCode::RESOURCE_EXHAUSTED;
    case wire::ERROR_CODE_TIMEOUT:
      return grpc::StatusCode::DEADLINE_EXCEEDED;
    case wire::ERROR_CODE_LOAD_FAILED:
    case wire::ERROR_CODE_NOT_READY:
    case wire::ERROR_CODE_UNAVAILABLE:
      return grpc::StatusCode::UNAVAILABLE;
    case wire::ERROR_CODE_CANCELLED:
      return grpc::StatusCode::CANCELLED;
    default:
      break;
  }
  return grpc::StatusCode::INTERNAL;
}

grpc::Status session_end_status(const wire::SessionClosed& closed) {
  if (!closed.has_cause()) {
    if (closed.state() == wire::SESSION_STATE_FAILED) {
      return {grpc::StatusCode::INTERNAL, "session failed without a cause"};
    }
    return grpc::Status::OK;
  }
  const auto reason = closed.cause().reason();
  return {status_code(reason), "session ended: " + wire::FailureReason_Name(reason)};
}

std::string listen_address(std::string_view host, std::uint16_t port) {
  std::string address;
  if (host == "localhost") {
    address = "127.0.0.1";
  } else if (host == "::1") {
    address = "[::1]";
  } else {
    address = host;
  }
  return address + ":" + std::to_string(port);
}

grpc::Status refused_open_status(const wire::OpenRefused& refused) {
  const auto reason = refused.cause().reason();
  return {status_code(reason), "open refused: " + wire::FailureReason_Name(reason)};
}

wire::SessionState wire_state(LogicalSessionState state) noexcept {
  switch (state) {
    case LogicalSessionState::Opening:
      return wire::SESSION_STATE_OPENING;
    case LogicalSessionState::Active:
      return wire::SESSION_STATE_ACTIVE;
    case LogicalSessionState::Finalizing:
      return wire::SESSION_STATE_FINALIZING;
    case LogicalSessionState::Draining:
      return wire::SESSION_STATE_DRAINING;
    case LogicalSessionState::CancelRequested:
      return wire::SESSION_STATE_CANCEL_REQUESTED;
    case LogicalSessionState::Closed:
      return wire::SESSION_STATE_CLOSED;
    case LogicalSessionState::Failed:
      return wire::SESSION_STATE_FAILED;
  }
  return wire::SESSION_STATE_FAILED;
}

wire::AudioFormat wire_format(const StreamAudioFormat& format) {
  wire::AudioFormat out;
  out.set_encoding(format.encoding == StreamAudioEncoding::PcmS16Le ? wire::AUDIO_ENCODING_PCM_S16LE
                                                                    : wire::AUDIO_ENCODING_MULAW);
  out.set_sample_rate_hz(format.sample_rate_hz);
  out.set_channels(format.channels);
  return out;
}

Result<SessionRequest> session_request(const wire::Open& open) {
  SessionRequest request;
  switch (open.mode()) {
    case wire::SESSION_MODE_STT_STREAMING:
      request.input_kind = SessionInputKind::Audio;
      break;
    case wire::SESSION_MODE_TTS_STREAMING:
      request.input_kind = SessionInputKind::Text;
      break;
    case wire::SESSION_MODE_UNSPECIFIED:
      return unexpected(invalid_event("Open names no mode"));
    default:
      return unexpected(unsupported("Open names a mode this server does not know"));
  }
  if (!short_text(open.language(), true)) {
    return unexpected(invalid_event("Open language is not 1 to 64 ASCII characters"));
  }
  if (!short_text(open.trace_id(), false) || !short_text(open.turn_id(), false)) {
    return unexpected(invalid_event("Open trace or turn id is over 64 ASCII characters"));
  }
  if (open.capabilities_size() > kMaxCapabilities) {
    return unexpected(invalid_event("Open lists more than 32 capabilities"));
  }
  request.language = open.language();
  if (request.input_kind == SessionInputKind::Audio) {
    if (!open.voice().empty() || !open.has_input_format()) {
      return unexpected(invalid_event("a speech-to-text Open needs an input format and no voice"));
    }
    const auto& format = open.input_format();
    switch (format.encoding()) {
      case wire::AUDIO_ENCODING_PCM_S16LE:
        request.input_format.encoding = StreamAudioEncoding::PcmS16Le;
        break;
      case wire::AUDIO_ENCODING_MULAW:
        request.input_format.encoding = StreamAudioEncoding::Mulaw;
        break;
      case wire::AUDIO_ENCODING_UNSPECIFIED:
        return unexpected(invalid_event("Open input format names no encoding"));
      default:
        return unexpected(unsupported("Open input format names an unknown encoding"));
    }
    if (format.sample_rate_hz() == 0 || format.channels() == 0) {
      return unexpected(invalid_event("Open input format has no rate or channels"));
    }
    request.input_format.sample_rate_hz = format.sample_rate_hz();
    request.input_format.channels = format.channels();
    return request;
  }
  if (open.has_input_format() || !short_text(open.voice(), true)) {
    return unexpected(invalid_event("a text-to-speech Open needs a voice and no input format"));
  }
  request.voice = open.voice();
  return request;
}

Result<void> check_target(const wire::Open& open, const ServedTarget& served) {
  switch (open.target_case()) {
    case wire::Open::kSelector:
      return unexpected(Error::make(Error::Code::Unsupported,
                                    "Open names a model, not a resolved target",
                                    "target_unresolved"));
    case wire::Open::kResolved:
      break;
    case wire::Open::TARGET_NOT_SET:
      return unexpected(invalid_event("Open names no target"));
  }
  const auto& target = open.resolved();
  if (!names_a_deployment(target.deployment_id())) {
    return unexpected(invalid_event("Open deployment id is malformed"));
  }
  if (target.generation() > kMaxWireInteger) {
    return unexpected(invalid_event("Open generation is above 2^53 - 1"));
  }
  if (target.deployment_id() != served.deployment_id || !target.descriptor_digest().empty()) {
    return unexpected(Error::make(Error::Code::NotReady,
                                  "Open names a deployment or descriptor not served here",
                                  "target_mismatch"));
  }
  if (target.generation() != 0) {
    return unexpected(Error::make(Error::Code::NotReady,
                                  "Open names a generation; this worker serves none",
                                  "stale_generation"));
  }
  return {};
}

}  // namespace tensorplate::serving::stream
