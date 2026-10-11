// SPDX-License-Identifier: Apache-2.0

#include "job_wire.hpp"

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <initializer_list>
#include <limits>
#include <nlohmann/json.hpp>
#include <optional>
#include <string>
#include <string_view>
#include <utility>
#include <variant>
#include <vector>

#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/job_event.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/job_result.hpp"
#include "tensorplate/core/result.hpp"
#include "tensorplate/ipc/sidecar_codec.hpp"

namespace tensorplate::adapters::python_pytorch {

namespace {

using json = nlohmann::json;

constexpr std::string_view kSchemaVersion = "0.1";
constexpr std::string_view kJobKindPrefix = "job_";

// -- Encoding. ---------------------------------------------------------------

Error refusal(std::string_view reason, std::string message) {
  return Error::make(Error::Code::ConfigInvalid, std::move(message), std::string(reason));
}

std::string dump(const json& header) {
  return header.dump(-1, ' ', false, json::error_handler_t::replace);
}

// Writes the envelope and the identity fields named in `fields` to `header`,
// or returns why not: an empty message id, else the first field that is zero
// or out of range.
std::optional<Error> write_envelope(
    json& header, std::string_view kind, std::string_view message_id,
    std::initializer_list<std::pair<const char*, std::uint64_t>> fields) {
  if (message_id.empty()) {
    return refusal("message_id_empty", "a message needs a message_id");
  }
  header = json{{"schema_version", std::string(kSchemaVersion)},
                {"message_id", std::string(message_id)},
                {"kind", std::string(kind)}};
  for (const auto& [name, value] : fields) {
    if (value == 0) {
      return refusal(std::string(name) + "_zero", std::string(name) + " is zero");
    }
    if (value > kMaxWireInteger) {
      return refusal("wire_integer_out_of_range", std::string(name) + " exceeds 2^53 - 1");
    }
    header[name] = value;
  }
  return std::nullopt;
}

json format_object(const AudioFormat& format) {
  return json{{"encoding", std::string(to_string(format.encoding))},
              {"sample_rate_hz", format.sample_rate_hz},
              {"channels", format.channels}};
}

// -- Decoding. ---------------------------------------------------------------

// What a fault is about: the message's kind and the job it names, zero for
// none.
struct Subject {
  std::string_view kind;
  std::uint64_t job_id = 0;
};

std::string about(std::string_view kind, std::string_view what) {
  return "sidecar " + std::string(kind) + ": " + std::string(what);
}

std::string unreadable_field(std::string_view field) {
  return "field '" + std::string(field) + "' is missing or malformed";
}

JobMessageFault fault(const Subject& subject, std::string_view reason, std::string_view what) {
  return JobMessageFault{
      subject.job_id,
      Error::make(Error::Code::InferenceFailed, about(subject.kind, what), std::string(reason)),
      std::nullopt};
}

JobMessageFault malformed(const Subject& subject, std::string_view field) {
  return fault(subject, kJobMessageMalformed, unreadable_field(field));
}

// The refusal of a job value object, as the fault of the job it would have
// reached.
JobMessageFault refused_by_seam(const Subject& subject, Error error) {
  error.code = Error::Code::InferenceFailed;
  error.message = about(subject.kind, error.message);
  return JobMessageFault{subject.job_id, std::move(error), std::nullopt};
}

// A non-negative JSON integer of at most `maximum`; never a boolean or a
// number with a fraction.
std::optional<std::uint64_t> wire_integer(const json& object, const char* name,
                                          std::uint64_t maximum = kMaxWireInteger) {
  const auto it = object.find(name);
  if (it == object.end() || !it->is_number_unsigned()) {
    return std::nullopt;
  }
  const auto value = it->get<std::uint64_t>();
  return value <= maximum ? std::optional<std::uint64_t>{value} : std::nullopt;
}

template <typename T>
std::optional<T> narrow_integer(const json& object, const char* name) {
  const auto value = wire_integer(object, name, std::numeric_limits<T>::max());
  return value.has_value() ? std::optional<T>{static_cast<T>(*value)} : std::nullopt;
}

std::optional<std::string> wire_text(const json& object, const char* name) {
  const auto it = object.find(name);
  if (it == object.end() || !it->is_string()) {
    return std::nullopt;
  }
  return it->get<std::string>();
}

const json* member(const json& object, const char* name, json::value_t type) {
  const auto it = object.find(name);
  return it != object.end() && it->type() == type ? &*it : nullptr;
}

void cut_to_bound(std::string& text) {
  if (text.size() <= kMaxJobErrorTextBytes) {
    return;
  }
  std::size_t end = kMaxJobErrorTextBytes;
  while (end > 0 && (static_cast<unsigned char>(text[end]) & 0xC0U) == 0x80U) {
    --end;
  }
  text.resize(end);
}

// The error object of a `job_failed` or an `error_event`.
std::optional<Error> read_error(const json& header) {
  const json* object = member(header, "error", json::value_t::object);
  if (object == nullptr) {
    return std::nullopt;
  }
  const auto code = wire_text(*object, "code");
  auto message = wire_text(*object, "message");
  if (!code.has_value() || !message.has_value()) {
    return std::nullopt;
  }
  Error error{error_code_from_string(*code).value_or(Error::Code::Internal), std::move(*message),
              std::nullopt};
  cut_to_bound(error.message);
  if (object->contains("context")) {
    error.context = wire_text(*object, "context");
    if (!error.context.has_value()) {
      return std::nullopt;
    }
    cut_to_bound(*error.context);
  }
  return error;
}

// Either the payload a `result` object describes or the name of the field
// that is not what the schema says.
using ReadResult = std::variant<JobResultPayload, const char*>;

ReadResult read_transcript(const json& result) {
  TranscriptResult transcript;
  auto text = wire_text(result, "text");
  const json* tokens = member(result, "tokens", json::value_t::array);
  const json* words = member(result, "words", json::value_t::array);
  if (!text.has_value()) {
    return "result.text";
  }
  if (tokens == nullptr) {
    return "result.tokens";
  }
  if (words == nullptr) {
    return "result.words";
  }
  transcript.text = std::move(*text);
  transcript.tokens.reserve(tokens->size());
  for (const json& token : *tokens) {
    if (!token.is_number_unsigned() ||
        token.get<std::uint64_t>() > std::numeric_limits<std::uint32_t>::max()) {
      return "result.tokens";
    }
    transcript.tokens.push_back(token.get<std::uint32_t>());
  }
  transcript.words.reserve(words->size());
  for (const json& word : *words) {
    auto word_text = wire_text(word, "text");
    const auto token_begin = narrow_integer<std::uint32_t>(word, "token_begin");
    const auto token_end = narrow_integer<std::uint32_t>(word, "token_end");
    const auto start_sample = wire_integer(word, "start_sample");
    const auto end_sample = wire_integer(word, "end_sample");
    if (!word_text || !token_begin || !token_end || !start_sample || !end_sample) {
      return "result.words";
    }
    transcript.words.push_back(
        WordUnit{std::move(*word_text), *token_begin, *token_end, *start_sample, *end_sample});
  }
  return transcript;
}

// An audio chunk whose window names `buffer`. An encoding this adapter does
// not know reads as one outside the enumeration, which the seam refuses.
ReadResult read_audio_chunk(const json& result, BufferRef buffer, std::size_t payload_bytes) {
  const json* format = member(result, "format", json::value_t::object);
  if (format == nullptr) {
    return "result.format";
  }
  const auto encoding = wire_text(*format, "encoding");
  const auto sample_rate_hz = narrow_integer<std::uint32_t>(*format, "sample_rate_hz");
  const auto channels = narrow_integer<std::uint16_t>(*format, "channels");
  if (!encoding || !sample_rate_hz || !channels) {
    return "result.format";
  }
  const auto clipped_samples = narrow_integer<std::uint32_t>(result, "clipped_samples");
  if (!clipped_samples) {
    return "result.clipped_samples";
  }
  if (wire_integer(result, "payload_length") != payload_bytes) {
    return "result.payload_length";
  }
  const auto known = audio_encoding_from_string(*encoding);
  return AudioChunkResult{
      AudioFormat{known.value_or(static_cast<AudioEncoding>(0xFF)), *sample_rate_hz, *channels},
      PcmWindow{buffer, 0, payload_bytes}, *clipped_samples};
}

ReadResult read_vad(const json& result) {
  const json* probabilities = member(result, "probabilities", json::value_t::array);
  if (probabilities == nullptr) {
    return "result.probabilities";
  }
  VadResult vad;
  vad.probabilities.reserve(probabilities->size());
  for (const json& probability : *probabilities) {
    if (!probability.is_number()) {
      return "result.probabilities";
    }
    // Judged as a double: narrowing first would round some values into
    // range, and is undefined for one no float holds.
    const double value = probability.get<double>();
    constexpr float kOutOfRange = 2.0F;
    vad.probabilities.push_back(value >= 0.0 && value <= 1.0 ? static_cast<float>(value)
                                                             : kOutOfRange);
  }
  return vad;
}

// The JobResult of a `job_progress` or `job_completed`, or the fault of its job.
std::variant<JobResult, JobMessageFault> read_result(const json& header,
                                                     const ipc::SidecarFrame& frame,
                                                     const Subject& subject,
                                                     const PcmPublisher& publish_pcm) {
  const std::uint64_t job_id = subject.job_id;
  const json* result = member(header, "result", json::value_t::object);
  if (result == nullptr) {
    return malformed(subject, "result");
  }
  const auto kind = job_result_kind_from_string(wire_text(*result, "type").value_or(""));
  if (!kind.has_value()) {
    return malformed(subject, "result.type");
  }
  const std::size_t payload_bytes = frame.payload.size();
  if (*kind != JobResultKind::AudioChunk && payload_bytes != 0) {
    return malformed(subject, "payload region");
  }
  const auto create = [&](ReadResult read) -> std::variant<JobResult, JobMessageFault> {
    if (const auto* field = std::get_if<const char*>(&read)) {
      return malformed(subject, *field);
    }
    auto created = JobResult::create(std::get<JobResultPayload>(std::move(read)));
    if (!created.has_value()) {
      return refused_by_seam(subject, std::move(created).error());
    }
    return std::move(created).value();
  };
  if (*kind == JobResultKind::Transcript) {
    return create(read_transcript(*result));
  }
  if (*kind == JobResultKind::Vad) {
    return create(read_vad(*result));
  }
  // Judged against a handle that names no buffer first, so a chunk the seam
  // refuses is never published.
  const auto probe =
      BufferRef::create(1, std::max<std::size_t>(payload_bytes, 1), BufferOwnership::Borrowed);
  if (!probe.has_value()) {
    return JobMessageFault{job_id, probe.error(), std::nullopt};
  }
  auto judged = create(read_audio_chunk(*result, probe.value(), payload_bytes));
  if (std::holds_alternative<JobMessageFault>(judged)) {
    return judged;
  }
  if (!publish_pcm) {
    return JobMessageFault{
        job_id,
        Error::make(Error::Code::NotReady, "no buffer manager to publish an audio chunk into",
                    "backend_not_ready"),
        std::nullopt};
  }
  auto published = publish_pcm(frame.payload);
  if (!published.has_value()) {
    return JobMessageFault{job_id, std::move(published).error(), std::nullopt};
  }
  // A released handle keeps its size, and an empty one has no bytes.
  if (!published.value().is_valid() || published.value().size_bytes() < payload_bytes) {
    return JobMessageFault{
        job_id,
        Error::make(Error::Code::Internal, "the published buffer cannot hold the chunk",
                    "pcm_publisher_handle_unusable"),
        std::nullopt};
  }
  return create(read_audio_chunk(*result, published.value(), payload_bytes));
}

SidecarJobMessage read_session_released(const json& header, const ipc::SidecarFrame& frame) {
  const Subject subject{"session_released"};
  const auto wire_key = wire_integer(header, "session_key");
  const auto wire_generation = wire_integer(header, "generation");
  const SessionReleased session{wire_key.value_or(0), wire_generation.value_or(0)};
  std::optional<JobMessageFault> failed;
  if (wire_text(header, "schema_version") != kSchemaVersion) {
    failed = malformed(subject, "schema_version");
  } else if (!wire_key.has_value()) {
    failed = malformed(subject, "session_key");
  } else if (!wire_generation.has_value()) {
    failed = malformed(subject, "generation");
  } else if (!frame.payload.empty()) {
    failed = malformed(subject, "payload region");
  } else if (session.session_key == 0) {
    failed = fault(subject, "session_key_zero", "session key is zero");
  } else if (session.generation == 0) {
    failed = fault(subject, "generation_zero", "generation is zero");
  }
  if (!failed.has_value()) {
    return session;
  }
  if (session.session_key != 0 && session.generation != 0) {
    failed->session = session;
  }
  return std::move(*failed);
}

// An `error_event` names the message it refuses; one that does not is no
// one's. One it cannot read otherwise still ends whatever waits behind the
// message it names.
SidecarJobMessage read_error_event(const json& header) {
  auto message_id = wire_text(header, "message_id");
  if (!message_id.has_value() || message_id->empty()) {
    return UnreadableHeader{};
  }
  const auto unreadable = [&](std::string_view field) {
    return MessageRefused{
        std::move(*message_id),
        Error::make(Error::Code::Internal, about("error_event", unreadable_field(field)),
                    std::string(kJobMessageMalformed))};
  };
  if (wire_text(header, "schema_version") != kSchemaVersion) {
    return unreadable("schema_version");
  }
  auto error = read_error(header);
  if (!error.has_value()) {
    return unreadable("error");
  }
  return MessageRefused{std::move(*message_id), std::move(*error)};
}

// The sequence of a `job_progress`, judged before the result is read so that
// no chunk is published for a message it faults.
std::variant<std::uint32_t, JobMessageFault> read_progress_sequence(const json& header,
                                                                    const Subject& subject) {
  const auto wide = wire_integer(header, "progress_sequence");
  if (!wide.has_value()) {
    return malformed(subject, "progress_sequence");
  }
  if (*wide == 0) {
    return fault(subject, "progress_sequence_zero", "the progress sequence is zero");
  }
  if (*wide > kMaxJobProgressEvents) {
    return fault(subject, "progress_sequence_too_large",
                 "the progress sequence exceeds the progress ceiling");
  }
  return static_cast<std::uint32_t>(*wide);
}

SidecarJobMessage read_job_message(const json& header, std::string_view kind,
                                   const ipc::SidecarFrame& frame,
                                   const PcmPublisher& publish_pcm) {
  const auto wire_job_id = wire_integer(header, "job_id");
  const Subject subject{kind, wire_job_id.value_or(0)};
  const auto event_kind = job_event_kind_from_string(kind.substr(kJobKindPrefix.size()));
  if (!event_kind.has_value()) {
    return malformed(subject, "kind");
  }
  if (wire_text(header, "schema_version") != kSchemaVersion) {
    return malformed(subject, "schema_version");
  }
  const auto session_key = wire_integer(header, "session_key");
  const auto generation = wire_integer(header, "generation");
  if (!wire_job_id.has_value()) {
    return malformed(subject, "job_id");
  }
  if (!session_key.has_value()) {
    return malformed(subject, "session_key");
  }
  if (!generation.has_value()) {
    return malformed(subject, "generation");
  }
  const JobIdentity identity{subject.job_id, *session_key, *generation};
  // Every event factory refuses a zero identity field with the same reason.
  if (auto named = JobEvent::accepted(identity); !named.has_value()) {
    return refused_by_seam(subject, std::move(named).error());
  }
  const bool carries_result =
      *event_kind == JobEventKind::Progress || *event_kind == JobEventKind::Completed;
  if (!carries_result && !frame.payload.empty()) {
    return malformed(subject, "payload region");
  }

  Result<JobEvent> event = unexpected(Error::Code::Internal, "unhandled job event kind");
  switch (*event_kind) {
    case JobEventKind::Accepted:
      event = JobEvent::accepted(identity);
      break;
    case JobEventKind::CancelAcknowledged:
      event = JobEvent::cancel_acknowledged(identity);
      break;
    case JobEventKind::Released:
      event = JobEvent::released(identity);
      break;
    case JobEventKind::Failed: {
      auto error = read_error(header);
      if (!error.has_value()) {
        return malformed(subject, "error");
      }
      event = JobEvent::failed(identity, std::move(*error));
      break;
    }
    case JobEventKind::Progress:
    case JobEventKind::Completed: {
      std::uint32_t sequence = 0;
      if (*event_kind == JobEventKind::Progress) {
        auto read = read_progress_sequence(header, subject);
        if (auto* failed = std::get_if<JobMessageFault>(&read)) {
          return std::move(*failed);
        }
        sequence = std::get<std::uint32_t>(read);
      }
      auto result = read_result(header, frame, subject, publish_pcm);
      if (auto* failed = std::get_if<JobMessageFault>(&result)) {
        return std::move(*failed);
      }
      event = *event_kind == JobEventKind::Progress
                  ? JobEvent::progress(identity, sequence, std::get<JobResult>(std::move(result)))
                  : JobEvent::completed(identity, std::get<JobResult>(std::move(result)));
      break;
    }
  }
  if (!event.has_value()) {
    return refused_by_seam(subject, std::move(event).error());
  }
  return std::move(event).value();
}

}  // namespace

std::size_t job_submit_payload_size(const JobRequest& request) noexcept {
  if (const auto* audio = std::get_if<AudioFrames>(&request.payload())) {
    return audio->pcm.byte_size;
  }
  if (const auto* vad = std::get_if<VadFrames>(&request.payload())) {
    return vad->pcm.byte_size;
  }
  const auto* text = std::get_if<TextSegment>(&request.payload());
  return text != nullptr ? text->text.size() : 0;
}

Result<std::string> encode_job_submit(const JobRequest& request, std::string_view message_id) {
  const JobIdentity& identity = request.identity();
  json header;
  if (auto refused = write_envelope(header, "job_submit", message_id,
                                    {{"job_id", identity.job_id},
                                     {"session_key", identity.session_key},
                                     {"generation", identity.generation}});
      refused.has_value()) {
    return unexpected(std::move(*refused));
  }
  header["job_class"] = std::string(to_string(request.job_class()));
  header["progress_limit"] = request.progress_limit();

  // An option is present exactly when the job class takes it.
  json options = json::object();
  if (request.job_class() != JobClass::VadFrames) {
    options["language"] = request.options().language;
  }
  if (request.job_class() == JobClass::TtsSynthesis) {
    options["voice"] = request.options().voice;
    options["speed_milli"] = request.options().speed_milli;
  }
  header["options"] = std::move(options);

  json input{{"payload_length", job_submit_payload_size(request)}};
  std::uint64_t widest = 0;
  if (const auto* audio = std::get_if<AudioFrames>(&request.payload())) {
    input["type"] = "audio_frames";
    input["format"] = format_object(audio->format);
    input["start_sample"] = widest = audio->start_sample;
  } else if (const auto* vad = std::get_if<VadFrames>(&request.payload())) {
    input["type"] = "vad_frames";
    input["format"] = format_object(vad->format);
    input["frame_samples"] = vad->frame_samples;
    input["frame_count"] = vad->frame_count;
    input["utterance_id"] = widest = vad->utterance_id;
  } else {
    input["type"] = "text_segment";
  }
  if (widest > kMaxWireInteger) {
    return unexpected(
        refusal("wire_integer_out_of_range", "a job_submit input field exceeds 2^53 - 1"));
  }
  header["input"] = std::move(input);
  return dump(header);
}

Result<std::string> encode_job_cancel(const JobIdentity& identity, std::string_view message_id) {
  json header;
  if (auto refused = write_envelope(header, "job_cancel", message_id,
                                    {{"job_id", identity.job_id},
                                     {"session_key", identity.session_key},
                                     {"generation", identity.generation}});
      refused.has_value()) {
    return unexpected(std::move(*refused));
  }
  return dump(header);
}

Result<std::string> encode_session_release(std::uint64_t session_key, std::uint64_t generation,
                                           std::string_view message_id) {
  json header;
  if (auto refused = write_envelope(header, "session_release", message_id,
                                    {{"session_key", session_key}, {"generation", generation}});
      refused.has_value()) {
    return unexpected(std::move(*refused));
  }
  return dump(header);
}

namespace {

// The strings of the array `name` in a header, none when it has no such
// field; std::nullopt when the field is anything but an array of strings.
std::optional<std::vector<std::string>> names_in(std::string_view header_text, const char* name) {
  const json header = json::parse(header_text, nullptr, /*allow_exceptions=*/false);
  if (!header.is_object()) {
    return std::nullopt;
  }
  std::vector<std::string> names;
  const auto it = header.find(name);
  if (it == header.end()) {
    return names;
  }
  if (!it->is_array()) {
    return std::nullopt;
  }
  for (const json& item : *it) {
    if (!item.is_string()) {
      return std::nullopt;
    }
    names.push_back(item.get<std::string>());
  }
  return names;
}

Unexpected violation(std::string_view reason, std::string message) {
  return unexpected(
      Error::make(Error::Code::InferenceFailed, std::move(message), std::string(reason)));
}

}  // namespace

Result<bool> offers_speech_jobs(std::string_view ready_event_header) {
  const auto listed = names_in(ready_event_header, "capabilities");
  if (!listed.has_value()) {
    return violation("capabilities_malformed",
                     "sidecar ready_event capabilities are not an array of strings");
  }
  return std::find(listed->begin(), listed->end(), kSpeechJobsCapability) != listed->end();
}

Result<std::vector<JobClass>> declared_job_classes(std::string_view load_model_response_header) {
  constexpr std::string_view kReason = "job_classes_malformed";
  const auto listed = names_in(load_model_response_header, "job_classes");
  if (!listed.has_value()) {
    return violation(kReason, "sidecar load response job_classes are not an array of strings");
  }
  if (listed->empty()) {
    return violation(kReason, "sidecar load response declares no job class");
  }
  if (listed->size() > kMaxDeclaredJobClasses) {
    return violation(kReason, "sidecar load response declares too many job classes");
  }
  std::vector<JobClass> classes;
  for (auto it = listed->begin(); it != listed->end(); ++it) {
    if (std::find(listed->begin(), it, *it) != it) {
      return violation(kReason, "sidecar load response repeats a job class");
    }
    if (const auto known = job_class_from_string(*it); known.has_value()) {
      classes.push_back(*known);
    }
  }
  return classes;
}

SidecarJobMessage decode_sidecar_message(const ipc::SidecarFrame& frame,
                                         const PcmPublisher& publish_pcm) {
  const json header = json::parse(frame.json_header, nullptr, /*allow_exceptions=*/false);
  // A header that is not a JSON object has no `kind` either.
  const auto kind = wire_text(header, "kind");
  if (!kind.has_value()) {
    return UnreadableHeader{};
  }
  if (*kind == "error_event") {
    return read_error_event(header);
  }
  if (*kind == "session_released") {
    return read_session_released(header, frame);
  }
  if (*kind == "session_release") {
    return malformed(Subject{*kind}, "kind");
  }
  if (kind->starts_with(kJobKindPrefix)) {
    return read_job_message(header, *kind, frame, publish_pcm);
  }
  return NotJobMessage{};
}

}  // namespace tensorplate::adapters::python_pytorch
