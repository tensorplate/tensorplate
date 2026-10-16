// SPDX-License-Identifier: Apache-2.0
#include "serving/stream/stream_mapping.hpp"

#include <algorithm>
#include <cctype>
#include <chrono>
#include <string>
#include <utility>
#include <variant>

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

Error empty_input(std::string message) {
  return Error::make(Error::Code::ConfigInvalid, std::move(message), "empty_input");
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

bool ascii_space(char c) noexcept {
  return c == ' ' || (c >= '\t' && c <= '\r');
}

wire::Usage usage(const LogicalSessionUsage& from) {
  wire::Usage out;
  out.set_used(from.used());
  out.set_limit(from.limit());
  return out;
}

/// Samples of `format` in `duration`.
std::uint64_t samples_in(std::chrono::milliseconds duration,
                         const StreamAudioFormat& format) noexcept {
  return std::uint64_t{format.sample_rate_hz} * static_cast<std::uint64_t>(duration.count()) /
         1000U;
}

wire::EndpointReason wire_reason(EndpointReason reason) noexcept {
  switch (reason) {
    case EndpointReason::Silence:
      return wire::ENDPOINT_REASON_SILENCE;
    case EndpointReason::ClientFinalize:
      return wire::ENDPOINT_REASON_CLIENT_FINALIZE;
    case EndpointReason::DurationLimit:
      return wire::ENDPOINT_REASON_DURATION_LIMIT;
    case EndpointReason::HalfClose:
      return wire::ENDPOINT_REASON_HALF_CLOSE;
  }
  return wire::ENDPOINT_REASON_UNSPECIFIED;
}

/// Encodes each kind of task output; an alternative added later has no
/// overload and yields no event.
struct TaskEvent {
  const StreamAudioFormat& format;

  std::optional<wire::ServerEvent> operator()(const PartialTranscriptOutput& from) const {
    wire::ServerEvent event;
    auto& out = *event.mutable_partial_transcript();
    out.set_utterance_id(from.utterance_id);
    out.set_revision(from.revision);
    out.set_text(from.text);
    return event;
  }
  std::optional<wire::ServerEvent> operator()(const EndpointDetectedOutput& from) const {
    wire::ServerEvent event;
    auto& out = *event.mutable_endpoint_detected();
    out.set_utterance_id(from.utterance_id);
    out.set_end_sample_offset(from.end_sample_offset);
    out.set_reason(wire_reason(from.reason));
    return event;
  }
  std::optional<wire::ServerEvent> operator()(const FinalTranscriptOutput& from) const {
    wire::ServerEvent event;
    auto& out = *event.mutable_final_transcript();
    out.set_utterance_id(from.utterance_id);
    for (const auto& segment : from.segments) {
      auto& to = *out.add_segments();
      to.set_start_us(segment.start_us);
      to.set_end_us(segment.end_us);
      to.set_text(segment.text);
    }
    out.set_end_sample_offset(from.end_sample_offset);
    out.set_finalize_sequence(from.finalize_sequence);
    return event;
  }
  std::optional<wire::ServerEvent> operator()(const AudioChunkOutput& from) const {
    wire::ServerEvent event;
    auto& out = *event.mutable_audio_chunk();
    out.set_segment_id(from.segment_id);
    out.set_chunk_index(from.chunk_index);
    out.set_segment_sample_offset(from.segment_sample_offset);
    *out.mutable_format() = wire_format(format);
    // NOLINTNEXTLINE(cppcoreguidelines-pro-type-reinterpret-cast): bytes as protobuf takes them.
    out.set_data(reinterpret_cast<const char*>(from.pcm.data()), from.pcm.size());
    return event;
  }
  std::optional<wire::ServerEvent> operator()(const SegmentCompletedOutput& from) const {
    wire::ServerEvent event;
    auto& out = *event.mutable_segment_completed();
    out.set_segment_id(from.segment_id);
    out.set_total_samples(from.total_samples);
    out.set_chunk_count(from.chunk_count);
    return event;
  }
  std::optional<wire::ServerEvent> operator()(const SynthesisCompletedOutput& from) const {
    wire::ServerEvent event;
    auto& out = *event.mutable_synthesis_completed();
    out.set_segment_count(from.segment_count);
    out.set_total_samples(from.total_samples);
    out.set_finalize_sequence(from.finalize_sequence);
    return event;
  }
  template <typename Other>
  std::optional<wire::ServerEvent> operator()(const Other& /*from*/) const {
    return std::nullopt;
  }
};

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
  } else if (host.find(':') != std::string_view::npos) {
    address = "[" + std::string{host} + "]";
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

wire::InputCredit wire_credit(const LogicalSessionStatus& status,
                              const LogicalSessionUsage& waiting_segments) {
  wire::InputCredit out;
  *out.mutable_bytes() = usage(status.input_credit_bytes);
  *out.mutable_waiting_segments() = usage(waiting_segments);
  return out;
}

wire::SessionStatus wire_status(const LogicalSessionStatus& status,
                                const LogicalSessionUsage& waiting_segments) {
  wire::SessionStatus out;
  out.set_state(wire_state(status.state));
  out.set_input_queue_depth(status.input_queue_depth);
  *out.mutable_input_credit() = wire_credit(status, waiting_segments);
  *out.mutable_output_pcm_bytes() = usage(status.output_pcm_bytes);
  *out.mutable_output_metadata_bytes() = usage(status.output_metadata_bytes);
  return out;
}

bool is_short_frame(std::uint32_t sample_count, const SessionTerms& terms) noexcept {
  return sample_count < samples_in(terms.min_frame, terms.audio_format);
}

Result<void> check_audio(const wire::Audio& audio, const SessionTerms& terms,
                         const InputHistory& history) {
  if (terms.input_kind != SessionInputKind::Audio) {
    return unexpected(invalid_event("wrong_input_kind"));
  }
  if (audio.data().empty()) {
    return unexpected(empty_input("audio body without data"));
  }
  const auto& format = terms.audio_format;
  const std::uint64_t frame_bytes =
      std::uint64_t{audio.sample_count()} * format.bytes_per_sample() * format.channels;
  if (audio.data().size() != frame_bytes) {
    return unexpected(invalid_event("frame_size_mismatch"));
  }
  if (audio.sample_count() > samples_in(terms.max_frame, format)) {
    return unexpected(invalid_event("frame_too_long"));
  }
  if (audio.sample_offset() != history.samples) {
    return unexpected(invalid_event("sample_offset_mismatch"));
  }
  // Only the client's Finalize or half-close may follow a short frame.
  if (history.short_frame) {
    return unexpected(invalid_event("audio_after_short_frame"));
  }
  return {};
}

Result<void> check_text_segment(const wire::TextSegment& segment, const SessionTerms& terms,
                                const InputHistory& history) {
  if (terms.input_kind != SessionInputKind::Text) {
    return unexpected(invalid_event("wrong_input_kind"));
  }
  const auto& text = segment.text();
  if (std::all_of(text.begin(), text.end(), ascii_space)) {
    return unexpected(empty_input("text segment without text"));
  }
  if (segment.segment_id() > kMaxWireInteger) {
    return unexpected(invalid_event("segment_id_out_of_range"));
  }
  if (history.last_segment_id && segment.segment_id() <= *history.last_segment_id) {
    return unexpected(invalid_event("segment_id_not_increasing"));
  }
  if (text.size() > terms.max_segment_text_bytes) {
    return unexpected(invalid_event("segment_too_long"));
  }
  if (history.synthesis_text_bytes + text.size() > terms.max_synthesis_text_bytes) {
    return unexpected(invalid_event("synthesis_text_limit_exceeded"));
  }
  if (!short_text(segment.turn_id(), false)) {
    return unexpected(invalid_event("turn_id_malformed"));
  }
  return {};
}

std::optional<wire::ServerEvent> task_event(const TaskOutput& output,
                                            const StreamAudioFormat& format) {
  return std::visit(TaskEvent{format}, output);
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
  if (target.deployment_id() != served.deployment_id) {
    return unexpected(Error::make(Error::Code::NotReady, "Open names a deployment not served here",
                                  "target_mismatch"));
  }
  if (target.generation() != served.generation) {
    return unexpected(Error::make(Error::Code::NotReady, "Open names a generation not served here",
                                  "stale_generation"));
  }
  if (target.descriptor_digest() != served.descriptor_digest) {
    return unexpected(Error::make(Error::Code::NotReady, "Open names a descriptor not served here",
                                  "target_mismatch"));
  }
  return {};
}

}  // namespace tensorplate::serving::stream
