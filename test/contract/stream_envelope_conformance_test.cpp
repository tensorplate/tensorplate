// SPDX-License-Identifier: Apache-2.0
//
// Conformance of the stream/v1 session envelope: its golden frames against the
// bindings generated in this build, the order and unit rules of the recorded
// sessions, the schema's numbering, and the enums it mirrors.

#include <google/protobuf/descriptor.h>
#include <google/protobuf/descriptor.pb.h>
#include <google/protobuf/message.h>
#include <google/protobuf/text_format.h>
#include <google/protobuf/unknown_field_set.h>
#include <google/protobuf/util/message_differencer.h>
#include <gtest/gtest.h>

#include <algorithm>
#include <cctype>
#include <cstddef>
#include <cstdint>
#include <filesystem>
#include <fstream>
#include <functional>
#include <iterator>
#include <memory>
#include <nlohmann/json.hpp>
#include <optional>
#include <set>
#include <sstream>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include "tensorplate/serving/session.hpp"
#include "tensorplate/stream/v1/session.grpc.pb.h"
#include "tensorplate/stream/v1/session.pb.h"

namespace {

namespace pb = google::protobuf;
namespace v1 = tensorplate::stream::v1;

struct Frame {
  std::string file;
  std::string name;
  std::string type;
  std::string bytes;
  std::string text;
};

std::string fixture_dir() {
  return std::string{TP_SOURCE_DIR} + "/test/contract/fixtures/stream/v1/";
}

std::string read_file(const std::string& path) {
  std::ifstream in(path, std::ios::binary);
  EXPECT_TRUE(in.is_open()) << path;
  return {std::istreambuf_iterator<char>(in), std::istreambuf_iterator<char>()};
}

// Lowercase contiguous hex only: the one form record.sh writes.
std::optional<std::string> decode_hex(std::string_view hex) {
  const auto digit = [](char c) {
    return c >= '0' && c <= '9' ? c - '0' : c >= 'a' && c <= 'f' ? c - 'a' + 10 : -1;
  };
  if (hex.size() % 2 != 0) {
    return std::nullopt;
  }
  std::string bytes;
  for (std::size_t i = 0; i < hex.size(); i += 2) {
    const int high = digit(hex[i]);
    const int low = digit(hex[i + 1]);
    if (high < 0 || low < 0) {
      return std::nullopt;
    }
    bytes.push_back(static_cast<char>(high * 16 + low));
  }
  return bytes;
}

std::string to_hex(std::string_view bytes) {
  constexpr std::string_view digits = "0123456789abcdef";
  std::string hex;
  for (const char c : bytes) {
    const auto byte = static_cast<unsigned char>(c);
    hex.push_back(digits[byte >> 4]);
    hex.push_back(digits[byte & 0x0F]);
  }
  return hex;
}

std::vector<Frame> load_frames(const std::string& file) {
  const std::string content = read_file(fixture_dir() + file);
  EXPECT_TRUE(!content.empty() && content.back() == '\n') << file << " must end with a newline";
  std::vector<Frame> frames;
  std::istringstream lines(content);
  std::string line;
  for (std::size_t number = 1; std::getline(lines, line); ++number) {
    if (line.find('\r') != std::string::npos) {
      ADD_FAILURE() << file << ":" << number << ": a carriage return; lines end with a line feed";
      continue;
    }
    if (line.empty() || line.front() == '#') {
      continue;
    }
    std::vector<std::string> columns;
    std::istringstream cells(line);
    for (std::string cell; std::getline(cells, cell, '\t');) {
      columns.push_back(cell);
    }
    const auto bytes = columns.size() == 4 ? decode_hex(columns[2]) : std::nullopt;
    if (!bytes) {
      ADD_FAILURE() << file << ":" << number
                    << ": expected a name, a type, lowercase hex and text, separated by tabs";
      continue;
    }
    frames.push_back(Frame{file, columns[0], columns[1], *bytes, columns[3]});
  }
  return frames;
}

// Every *.frames file, so one added later is held to the same checks.
std::vector<Frame> all_frames() {
  std::vector<std::string> files;
  for (const auto& entry : std::filesystem::directory_iterator(fixture_dir())) {
    if (entry.path().extension() == ".frames") {
      files.push_back(entry.path().filename().string());
    }
  }
  std::sort(files.begin(), files.end());
  std::vector<Frame> frames;
  for (const auto& file : files) {
    auto loaded = load_frames(file);
    std::move(loaded.begin(), loaded.end(), std::back_inserter(frames));
  }
  return frames;
}

const pb::FileDescriptor* schema() {
  return v1::ClientEvent::descriptor()->file();
}

// The frame's message type when stream/v1 defines it; extension.proto's types are not.
const pb::Descriptor* schema_type(const Frame& frame) {
  const auto* type = schema()->pool()->FindMessageTypeByName(frame.type);
  return type != nullptr && type->file() == schema() ? type : nullptr;
}

// The stream/v1 type that reads the frame: its own, or the one of the same name that a frame
// of extension.proto is sent to.
const pb::Descriptor* reader_type(const Frame& frame) {
  return schema()->FindMessageTypeByName(frame.type.substr(frame.type.rfind('.') + 1));
}

std::unique_ptr<pb::Message> new_message(const pb::Descriptor* type) {
  return std::unique_ptr<pb::Message>(
      pb::MessageFactory::generated_factory()->GetPrototype(type)->New());
}

// Every frame as stream/v1 reads it, in file order.
std::vector<std::pair<Frame, std::unique_ptr<pb::Message>>> golden_messages() {
  std::vector<std::pair<Frame, std::unique_ptr<pb::Message>>> messages;
  for (auto& frame : all_frames()) {
    if (const auto* type = reader_type(frame)) {
      auto message = new_message(type);
      EXPECT_TRUE(message->ParseFromString(frame.bytes)) << frame.file << ": " << frame.name;
      messages.emplace_back(std::move(frame), std::move(message));
    }
  }
  return messages;
}

// Calls `visit` for `message` and every message nested in it.
void for_each_message(const pb::Message& message,
                      const std::function<void(const pb::Message&)>& visit) {
  visit(message);
  const auto* reflection = message.GetReflection();
  std::vector<const pb::FieldDescriptor*> fields;
  reflection->ListFields(message, &fields);
  for (const auto* field : fields) {
    if (field->cpp_type() != pb::FieldDescriptor::CPPTYPE_MESSAGE) {
      continue;
    }
    if (!field->is_repeated()) {
      for_each_message(reflection->GetMessage(message, field), visit);
      continue;
    }
    for (int i = 0; i < reflection->FieldSize(message, field); ++i) {
      for_each_message(reflection->GetRepeatedMessage(message, field, i), visit);
    }
  }
}

int unknown_field_count(const pb::Message& message) {
  int count = 0;
  for_each_message(message, [&count](const pb::Message& part) {
    count += part.GetReflection()->GetUnknownFields(part).field_count();
  });
  return count;
}

// What `message` itself kept without knowing it: "4=5 " for a varint, "10='text' " for bytes.
std::string unknown_fields(const pb::Message& message) {
  const pb::UnknownFieldSet& unknown = message.GetReflection()->GetUnknownFields(message);
  std::string kept;
  for (int i = 0; i < unknown.field_count(); ++i) {
    const pb::UnknownField& field = unknown.field(i);
    kept += std::to_string(field.number()) + "=";
    if (field.type() == pb::UnknownField::TYPE_VARINT) {
      kept += std::to_string(field.varint()) + " ";
    } else if (field.type() == pb::UnknownField::TYPE_LENGTH_DELIMITED) {
      kept += "'" + std::string{field.length_delimited()} + "' ";
    } else {
      kept += "? ";
    }
  }
  return kept;
}

// Every sequence and every 64-bit id, which the schema bounds by kLargest.
constexpr std::uint64_t kLargest = (1ULL << 53) - 1;
bool is_sequence_or_id(const pb::FieldDescriptor* field) {
  const std::string_view name = field->name();
  return field->cpp_type() == pb::FieldDescriptor::CPPTYPE_UINT64 &&
         (name == "generation" || name.ends_with("sequence") || name.ends_with("_id"));
}

// proto3 keeps an enum number the schema does not name in the field itself,
// not among the unknown fields.
std::vector<std::string> unnamed_enum_values(const pb::Message& message) {
  std::vector<std::string> found;
  for_each_message(message, [&found](const pb::Message& part) {
    const auto* reflection = part.GetReflection();
    std::vector<const pb::FieldDescriptor*> fields;
    reflection->ListFields(part, &fields);
    for (const auto* field : fields) {
      if (field->cpp_type() != pb::FieldDescriptor::CPPTYPE_ENUM) {
        continue;
      }
      const int count = field->is_repeated() ? reflection->FieldSize(part, field) : 1;
      for (int i = 0; i < count; ++i) {
        const int number = field->is_repeated() ? reflection->GetRepeatedEnumValue(part, field, i)
                                                : reflection->GetEnumValue(part, field);
        if (field->enum_type()->FindValueByNumber(number) == nullptr) {
          found.push_back(std::string{field->full_name()} + " = " + std::to_string(number));
        }
      }
    }
  });
  return found;
}

// Every message of the schema file, nested ones behind their parent.
std::vector<const pb::Descriptor*> schema_messages() {
  std::vector<const pb::Descriptor*> messages;
  for (int i = 0; i < schema()->message_type_count(); ++i) {
    messages.push_back(schema()->message_type(i));
  }
  // Grows while it is walked.
  for (std::size_t i = 0; i < messages.size(); ++i) {
    for (int nested = 0; nested < messages[i]->nested_type_count(); ++nested) {
      messages.push_back(messages[i]->nested_type(nested));
    }
  }
  return messages;
}

bool same(const pb::Message& left, const pb::Message& right) {
  return pb::util::MessageDifferencer::Equals(left, right);
}

struct Event {
  std::string name;
  std::optional<v1::ClientEvent> client;
  std::optional<v1::ServerEvent> server;
};

// One recorded session: client and server events in the order they were sent.
std::vector<Event> load_script(const std::string& file) {
  std::vector<Event> script;
  for (const Frame& frame : load_frames(file)) {
    Event event{frame.name, std::nullopt, std::nullopt};
    if (frame.type == v1::ClientEvent::descriptor()->full_name()) {
      EXPECT_TRUE(event.client.emplace().ParseFromString(frame.bytes)) << frame.name;
    } else if (frame.type == v1::ServerEvent::descriptor()->full_name()) {
      EXPECT_TRUE(event.server.emplace().ParseFromString(frame.bytes)) << frame.name;
    } else {
      ADD_FAILURE() << file << ": " << frame.name << " is not an event of the session";
    }
    script.push_back(std::move(event));
  }
  return script;
}

bool opens_and_is_ready(const std::vector<Event>& script) {
  return script.size() >= 2 && script[0].client && script[0].client->has_open() &&
         script[1].server && script[1].server->has_ready();
}

std::uint64_t bytes_per_sample(const v1::AudioFormat& format) {
  if (format.encoding() == v1::AUDIO_ENCODING_PCM_S16LE) {
    return 2ULL * format.channels();
  }
  return format.encoding() == v1::AUDIO_ENCODING_MULAW ? format.channels() : 0;
}

// A sample boundary between two microseconds is rounded outward: a start down, an end up.
constexpr std::uint64_t start_us_of(std::uint64_t sample, std::uint64_t rate) {
  return sample * 1'000'000 / rate;
}

constexpr std::uint64_t end_us_of(std::uint64_t sample, std::uint64_t rate) {
  return (sample * 1'000'000 + rate - 1) / rate;
}

bool is_snake_case(std::string_view text) {
  return !text.empty() && text.front() >= 'a' && text.front() <= 'z' &&
         std::all_of(text.begin(), text.end(), [](char c) {
           return (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '_';
         });
}

std::string upper(std::string_view text) {
  std::string result{text};
  std::transform(result.begin(), result.end(), result.begin(),
                 [](unsigned char c) { return static_cast<char>(std::toupper(c)); });
  return result;
}

// What a mirror of `source` is: the zero value, then each name at its index plus one.
std::vector<std::string> mirror_of(const std::string& prefix,
                                   const std::vector<std::string>& source) {
  std::vector<std::string> names{prefix + "UNSPECIFIED"};
  for (const auto& name : source) {
    names.push_back(prefix + upper(name));
  }
  return names;
}

std::vector<std::string> names_by_number(const pb::EnumDescriptor* values) {
  std::vector<std::string> names;
  for (int number = 0; number < values->value_count(); ++number) {
    const auto* value = values->FindValueByNumber(number);
    names.emplace_back(value != nullptr ? value->name() : "(no value has this number)");
  }
  return names;
}

// A field as the schema declares it: "uint64 Audio.sample_offset = 3",
// "repeated TranscriptSegment FinalTranscript.segments = 2".
std::string declaration(const pb::FieldDescriptor* field) {
  const std::string package = std::string{schema()->package()} + ".";
  const auto local = [&package](std::string_view name) {
    return std::string{name.substr(0, package.size()) == package ? name.substr(package.size())
                                                                 : name};
  };
  std::string type{field->type_name()};
  if (field->message_type() != nullptr) {
    type = local(field->message_type()->full_name());
  } else if (field->enum_type() != nullptr) {
    type = local(field->enum_type()->full_name());
  }
  std::string text =
      type + " " + local(field->full_name()) + " = " + std::to_string(field->number());
  if (field->is_repeated()) {
    return "repeated " + text;
  }
  if (const auto* oneof = field->real_containing_oneof()) {
    return text + " (oneof " + std::string{oneof->name()} + ")";
  }
  return field->message_type() == nullptr && field->has_presence() ? "optional " + text : text;
}

// Every field of the schema. A type, a label or a message's name can change
// without changing a byte of the golden frames; this list is what holds them.
constexpr std::string_view kFields = R"(
AudioEncoding AudioFormat.encoding = 1
uint32 AudioFormat.sample_rate_hz = 2
uint32 AudioFormat.channels = 3
uint64 Usage.used = 1
uint64 Usage.limit = 2
Usage InputCredit.bytes = 1
Usage InputCredit.waiting_segments = 2
uint32 SpeechToTextLimits.min_frame_ms = 1
uint32 SpeechToTextLimits.max_frame_ms = 2
uint32 SpeechToTextLimits.max_utterance_ms = 3
uint32 TextToSpeechLimits.max_segment_text_bytes = 1
uint32 TextToSpeechLimits.max_segment_audio_ms = 2
uint32 TextToSpeechLimits.max_synthesis_text_bytes = 3
uint32 TextToSpeechLimits.max_synthesis_audio_ms = 4
uint32 SessionLimits.idle_timeout_ms = 1
uint32 SessionLimits.heartbeat_interval_ms = 2
uint32 SessionLimits.liveness_timeout_ms = 3
uint32 SessionLimits.max_duration_ms = 4
uint32 SessionLimits.finalize_deadline_ms = 5
uint32 SessionLimits.output_stall_timeout_ms = 6
SpeechToTextLimits SessionLimits.speech_to_text = 7
TextToSpeechLimits SessionLimits.text_to_speech = 8
FailureReason EndCause.reason = 1
ErrorCode EndCause.code = 2
string EndCause.detail = 3
EndCause OpenRefused.cause = 1
Admission OpenRefused.admission = 2
uint64 ClientEvent.sequence = 1
string ClientEvent.session_id = 2
Open ClientEvent.open = 10 (oneof body)
Finalize ClientEvent.finalize = 11 (oneof body)
Cancel ClientEvent.cancel = 12 (oneof body)
Ping ClientEvent.ping = 13 (oneof body)
StatusRequest ClientEvent.status_request = 14 (oneof body)
Audio ClientEvent.audio = 20 (oneof body)
TextSegment ClientEvent.text_segment = 21 (oneof body)
string ResolvedTarget.deployment_id = 1
uint64 ResolvedTarget.generation = 2
string ResolvedTarget.descriptor_digest = 3
string Selector.model = 1
string Selector.version = 2
ResolvedTarget Open.resolved = 1 (oneof target)
Selector Open.selector = 2 (oneof target)
SessionMode Open.mode = 3
string Open.language = 4
AudioFormat Open.input_format = 5
string Open.voice = 6
string Open.trace_id = 7
string Open.turn_id = 8
repeated Capability Open.capabilities = 9
bytes Audio.data = 1
uint32 Audio.sample_count = 2
uint64 Audio.sample_offset = 3
uint64 TextSegment.segment_id = 1
string TextSegment.text = 2
string TextSegment.turn_id = 3
uint64 ServerEvent.sequence = 1
Ready ServerEvent.ready = 10 (oneof body)
Accepted ServerEvent.accepted = 11 (oneof body)
SessionStatus ServerEvent.status = 12 (oneof body)
Pong ServerEvent.pong = 13 (oneof body)
CancelAccepted ServerEvent.cancel_accepted = 14 (oneof body)
SessionClosed ServerEvent.session_closed = 15 (oneof body)
PartialTranscript ServerEvent.partial_transcript = 20 (oneof body)
EndpointDetected ServerEvent.endpoint_detected = 21 (oneof body)
FinalTranscript ServerEvent.final_transcript = 22 (oneof body)
AudioChunk ServerEvent.audio_chunk = 23 (oneof body)
SegmentCompleted ServerEvent.segment_completed = 24 (oneof body)
SynthesisCompleted ServerEvent.synthesis_completed = 25 (oneof body)
string Ready.session_id = 1
uint64 Ready.generation = 2
string Ready.descriptor_digest = 3
SessionLimits Ready.limits = 4
uint32 Ready.remaining_duration_ms = 5
AudioFormat Ready.input_format = 6
AudioFormat Ready.output_format = 7
SessionStatus Ready.status = 8
repeated Capability Ready.capabilities = 9
uint64 Accepted.accepted_sequence = 1
InputCredit Accepted.input_credit = 2
SessionState SessionStatus.state = 1
uint32 SessionStatus.input_queue_depth = 2
InputCredit SessionStatus.input_credit = 3
Usage SessionStatus.output_pcm_bytes = 4
Usage SessionStatus.output_metadata_bytes = 5
SessionState SessionClosed.state = 1
EndCause SessionClosed.cause = 2
Admission SessionClosed.admission = 3
uint64 SessionClosed.last_accepted_sequence = 4
SessionTotals SessionClosed.totals = 5
uint64 SessionTotals.input_audio_samples = 1
uint64 SessionTotals.input_text_bytes = 2
uint64 SessionTotals.output_audio_samples = 3
uint32 SessionTotals.utterances_completed = 4
uint32 SessionTotals.segments_completed = 5
uint64 PartialTranscript.utterance_id = 1
uint32 PartialTranscript.revision = 2
string PartialTranscript.text = 3
uint64 EndpointDetected.utterance_id = 1
uint64 EndpointDetected.end_sample_offset = 2
EndpointReason EndpointDetected.reason = 3
uint64 TranscriptSegment.start_us = 1
uint64 TranscriptSegment.end_us = 2
string TranscriptSegment.text = 3
uint64 FinalTranscript.utterance_id = 1
repeated TranscriptSegment FinalTranscript.segments = 2
uint64 FinalTranscript.end_sample_offset = 3
uint64 FinalTranscript.finalize_sequence = 4
uint64 AudioChunk.segment_id = 1
uint32 AudioChunk.chunk_index = 2
uint64 AudioChunk.segment_sample_offset = 3
AudioFormat AudioChunk.format = 4
bytes AudioChunk.data = 5
uint64 SegmentCompleted.segment_id = 1
uint64 SegmentCompleted.total_samples = 2
uint32 SegmentCompleted.chunk_count = 3
uint32 SynthesisCompleted.segment_count = 1
uint64 SynthesisCompleted.total_samples = 2
uint64 SynthesisCompleted.finalize_sequence = 3
)";

// The ends with a cause that the frames show. The code is the one
// docs/observability/failure-reasons.md pairs with the reason. The outcome is
// a property of the recordings, the one the session layer gives that end: an
// expired timer of an admitted session takes the abort path and so closes it
// (runtime/src/serving/session/session_manager.cpp); the others are in
// docs/architecture/serving-worker.md ("How sessions end").
struct End {
  v1::FailureReason reason;
  v1::ErrorCode code;
  v1::SessionState state;
};
constexpr End kEnds[] = {
    {v1::FAILURE_REASON_CLIENT_CANCELLED, v1::ERROR_CODE_CANCELLED, v1::SESSION_STATE_CLOSED},
    {v1::FAILURE_REASON_INPUT_CREDIT_EXCEEDED, v1::ERROR_CODE_RESOURCE_EXHAUSTED,
     v1::SESSION_STATE_FAILED},
    {v1::FAILURE_REASON_DEPLOYMENT_RETIRED, v1::ERROR_CODE_UNAVAILABLE, v1::SESSION_STATE_CLOSED},
    {v1::FAILURE_REASON_IDLE_TIMEOUT, v1::ERROR_CODE_TIMEOUT, v1::SESSION_STATE_CLOSED},
    {v1::FAILURE_REASON_INTERNAL, v1::ERROR_CODE_INTERNAL, v1::SESSION_STATE_FAILED},
};

// The reasons the schema's comment on OpenRefused lists as preceding admission,
// each with the code failure-reasons.md pairs with it.
struct Refusal {
  v1::FailureReason reason;
  v1::ErrorCode code;
};
constexpr Refusal kRefusals[] = {
    {v1::FAILURE_REASON_INVALID_EVENT, v1::ERROR_CODE_CONFIG_INVALID},
    {v1::FAILURE_REASON_BACKEND_UNSUPPORTED_CAPABILITY, v1::ERROR_CODE_UNSUPPORTED},
    {v1::FAILURE_REASON_TARGET_UNRESOLVED, v1::ERROR_CODE_UNSUPPORTED},
    {v1::FAILURE_REASON_TARGET_MISMATCH, v1::ERROR_CODE_NOT_READY},
    {v1::FAILURE_REASON_STALE_GENERATION, v1::ERROR_CODE_NOT_READY},
    {v1::FAILURE_REASON_ADMISSION_CLOSED, v1::ERROR_CODE_NOT_READY},
    {v1::FAILURE_REASON_SESSION_COUNT_LIMIT, v1::ERROR_CODE_RESOURCE_EXHAUSTED},
};

// No default, and -Wswitch is an error here whatever the build's flags: a
// state added to the header stops this file compiling until it is listed.
#pragma GCC diagnostic push
#pragma GCC diagnostic error "-Wswitch"
bool is_session_state(tensorplate::serving::LogicalSessionState state) {
  using State = tensorplate::serving::LogicalSessionState;
  switch (state) {
    case State::Opening:
    case State::Active:
    case State::Finalizing:
    case State::Draining:
    case State::CancelRequested:
    case State::Closed:
    case State::Failed:
      return true;
  }
  return false;
}
#pragma GCC diagnostic pop

std::vector<std::string> schema_enum(const std::string& file, const std::string& property) {
  std::ifstream in(std::string{TP_SOURCE_DIR} + "/protocol/schemas/" + file);
  EXPECT_TRUE(in.is_open()) << file;
  const auto schema_json = nlohmann::json::parse(in, nullptr, false);
  if (schema_json.is_discarded()) {
    ADD_FAILURE() << file << " is not JSON";
    return {};
  }
  return schema_json.at("properties").at(property).at("enum").get<std::vector<std::string>>();
}

TEST(StreamEnvelope, FrameNamesAreUniqueSnakeCase) {
  std::set<std::string> names;
  for (const Frame& frame : all_frames()) {
    EXPECT_TRUE(is_snake_case(frame.name)) << frame.file << ": " << frame.name;
    EXPECT_TRUE(names.insert(frame.name).second) << frame.file << " repeats " << frame.name;
  }
}

TEST(StreamEnvelope, GoldenFramesDecodeAndReencodeByteExact) {
  std::size_t checked = 0;
  for (const Frame& frame : all_frames()) {
    const auto* type = schema_type(frame);
    if (type == nullptr) {
      continue;
    }
    SCOPED_TRACE(frame.file + ": " + frame.name);
    ++checked;
    const auto decoded = new_message(type);
    ASSERT_TRUE(decoded->ParseFromString(frame.bytes));
    EXPECT_EQ(unknown_field_count(*decoded), 0);
    EXPECT_EQ(unnamed_enum_values(*decoded), std::vector<std::string>{});

    const auto described = new_message(type);
    ASSERT_TRUE(pb::TextFormat::ParseFromString(frame.text, described.get()));
    pb::util::MessageDifferencer differencer;
    std::string difference;
    differencer.ReportDifferencesToString(&difference);
    EXPECT_TRUE(differencer.Compare(*described, *decoded)) << difference;

    EXPECT_EQ(to_hex(decoded->SerializeAsString()), to_hex(frame.bytes));
  }
  EXPECT_GT(checked, 0U);
}

TEST(StreamEnvelope, EveryFieldAndEnumValueHasAGoldenFrame) {
  std::set<std::string> used;
  for (const auto& [frame, message] : golden_messages()) {
    // Only a frame of stream/v1's own type is written with the names of its values.
    const bool names_values = schema_type(frame) != nullptr;
    for_each_message(*message, [&used, names_values](const pb::Message& part) {
      std::vector<const pb::FieldDescriptor*> fields;
      part.GetReflection()->ListFields(part, &fields);
      for (const auto* field : fields) {
        used.emplace(field->full_name());
        if (names_values && field->cpp_type() == pb::FieldDescriptor::CPPTYPE_ENUM &&
            !field->is_repeated()) {
          used.emplace(part.GetReflection()->GetEnum(part, field)->full_name());
        }
      }
    });
  }

  std::vector<const pb::EnumDescriptor*> enums;
  for (int i = 0; i < schema()->enum_type_count(); ++i) {
    enums.push_back(schema()->enum_type(i));
  }
  for (const auto* message : schema_messages()) {
    for (int nested = 0; nested < message->enum_type_count(); ++nested) {
      enums.push_back(message->enum_type(nested));
    }
    for (int field = 0; field < message->field_count(); ++field) {
      const std::string name{message->field(field)->full_name()};
      EXPECT_TRUE(used.contains(name)) << "no golden frame sets " << name;
    }
  }

  // A frame is what ties a value's name to its number. The three mirrors are
  // tied to their sources instead, and zero is never on the wire.
  const std::set<const pb::EnumDescriptor*> mirrors{
      v1::ErrorCode_descriptor(), v1::FailureReason_descriptor(), v1::SessionState_descriptor()};
  for (const auto* values : enums) {
    if (mirrors.contains(values)) {
      continue;
    }
    for (int i = 0; i < values->value_count(); ++i) {
      const std::string name{values->value(i)->full_name()};
      EXPECT_TRUE(values->value(i)->number() == 0 || used.contains(name))
          << "no golden frame uses " << name;
    }
  }
}

TEST(StreamEnvelope, GoldenFramesKeepTheStatedValueRules) {
  for (const auto& [frame, message] : golden_messages()) {
    SCOPED_TRACE(frame.file + ": " + frame.name);
    for_each_message(*message, [](const pb::Message& part) {
      std::vector<const pb::FieldDescriptor*> fields;
      part.GetReflection()->ListFields(part, &fields);
      for (const auto* field : fields) {
        if (is_sequence_or_id(field)) {
          EXPECT_LE(part.GetReflection()->GetUInt64(part, field), kLargest) << field->full_name();
        }
      }
      if (const auto* usage = dynamic_cast<const v1::Usage*>(&part)) {
        EXPECT_LE(usage->used(), usage->limit());
      }
      if (const auto* ready = dynamic_cast<const v1::Ready*>(&part)) {
        EXPECT_LE(ready->remaining_duration_ms(), ready->limits().max_duration_ms());
      }
      if (const auto* closed = dynamic_cast<const v1::SessionClosed*>(&part)) {
        EXPECT_TRUE(closed->state() == v1::SESSION_STATE_CLOSED ||
                    closed->state() == v1::SESSION_STATE_FAILED);
        EXPECT_TRUE(closed->state() != v1::SESSION_STATE_FAILED || closed->has_cause())
            << "a failed session names its cause";
        EXPECT_NE(closed->admission(), v1::ADMISSION_UNSPECIFIED) << "admission is always set";
        if (closed->has_cause()) {
          const auto* end = std::find_if(
              std::begin(kEnds), std::end(kEnds),
              [closed](const End& known) { return known.reason == closed->cause().reason(); });
          ASSERT_NE(end, std::end(kEnds)) << "list this end in kEnds with its code and outcome";
          EXPECT_EQ(closed->cause().code(), end->code);
          EXPECT_EQ(closed->state(), end->state);
        }
      }
      if (const auto* refused = dynamic_cast<const v1::OpenRefused*>(&part)) {
        EXPECT_EQ(refused->admission(), v1::ADMISSION_NOT_ADMITTED);
        const auto* refusal = std::find_if(
            std::begin(kRefusals), std::end(kRefusals),
            [refused](const Refusal& known) { return known.reason == refused->cause().reason(); });
        ASSERT_NE(refusal, std::end(kRefusals)) << "not a reason that precedes admission";
        EXPECT_EQ(refused->cause().code(), refusal->code);
      }
      if (const auto* cause = dynamic_cast<const v1::EndCause*>(&part)) {
        EXPECT_NE(cause->reason(), v1::FAILURE_REASON_UNSPECIFIED);
        EXPECT_NE(cause->code(), v1::ERROR_CODE_UNSPECIFIED);
        const std::string& detail = cause->detail();
        const auto printable = [](char c) { return c >= ' ' && c <= '~'; };
        EXPECT_TRUE(detail.size() <= 64 && std::all_of(detail.begin(), detail.end(), printable))
            << "not at most 64 ASCII characters: " << detail;
      }
    });
  }
}

TEST(StreamEnvelope, SessionScriptsAreSequencedAndAddressed) {
  for (const char* file : {"stt_session.frames", "tts_session.frames"}) {
    SCOPED_TRACE(file);
    const auto script = load_script(file);
    ASSERT_TRUE(opens_and_is_ready(script));
    const v1::ClientEvent& open = *script[0].client;
    const v1::ResolvedTarget& target = open.open().resolved();
    const v1::Ready& ready = script[1].server->ready();
    EXPECT_TRUE(open.session_id().empty());
    EXPECT_TRUE(open.open().has_resolved()) << "a server refuses an Open that carries a selector";
    EXPECT_NE(target.generation(), 0U);
    EXPECT_FALSE(ready.session_id().empty());
    EXPECT_EQ(ready.generation(), target.generation());
    // An Open whose digest is not the served descriptor's is refused.
    EXPECT_EQ(ready.descriptor_digest(), target.descriptor_digest());
    // The active state is the one in which Ready has been sent (session.hpp).
    EXPECT_EQ(ready.status().state(), v1::SESSION_STATE_ACTIVE);
    // The deployment's bounds for the session's mode, never the other's.
    const bool speech = open.open().mode() == v1::SESSION_MODE_STT_STREAMING;
    EXPECT_EQ(ready.limits().has_speech_to_text(), speech);
    EXPECT_EQ(ready.limits().has_text_to_speech(), !speech);
    // A property of the recordings: neither side declares a capability; none is defined yet.
    EXPECT_TRUE(open.open().capabilities().empty() && ready.capabilities().empty());

    std::uint64_t client_sequence = 0;
    std::uint64_t server_sequence = 0;
    std::set<std::uint64_t> inputs;  // sequences of the Audio and TextSegments sent
    v1::Accepted accepted;           // the latest one
    bool segment_credit_returned = false;
    std::uint64_t audio_samples = 0;  // what the events add up to
    std::uint64_t text_bytes = 0;
    std::uint64_t output_bytes = 0;
    std::uint32_t finals = 0;
    std::uint32_t segments = 0;
    for (std::size_t i = 0; i < script.size(); ++i) {
      SCOPED_TRACE(script[i].name);
      if (script[i].client) {
        const v1::ClientEvent& event = *script[i].client;
        EXPECT_EQ(event.sequence(), ++client_sequence);
        if (i > 0) {
          EXPECT_FALSE(event.has_open());
          EXPECT_EQ(event.session_id(), ready.session_id());
        }
        if (event.has_audio() || event.has_text_segment()) {
          inputs.insert(event.sequence());
          audio_samples += event.audio().sample_count();
          text_bytes += event.text_segment().text().size();
        }
      } else if (script[i].server) {
        const v1::ServerEvent& event = *script[i].server;
        EXPECT_EQ(event.sequence(), ++server_sequence);
        EXPECT_EQ(event.has_ready(), i == 1);
        EXPECT_EQ(event.has_session_closed(), i + 1 == script.size());
        output_bytes += event.audio_chunk().data().size();
        finals += event.has_final_transcript() ? 1 : 0;
        segments += event.has_segment_completed() ? 1 : 0;
        if (event.has_accepted()) {
          const v1::Accepted before = accepted;
          accepted = event.accepted();
          const v1::InputCredit& credit = accepted.input_credit();
          EXPECT_TRUE(inputs.contains(accepted.accepted_sequence()))
              << "not the sequence of an Audio or a TextSegment already sent";
          EXPECT_GE(accepted.accepted_sequence(), before.accepted_sequence()) << "it went back";
          // With no input newly accepted, it is sent because credit returned.
          if (accepted.accepted_sequence() == before.accepted_sequence()) {
            const std::uint64_t waiting = credit.waiting_segments().used();
            const std::uint64_t waiting_before = before.input_credit().waiting_segments().used();
            EXPECT_LE(credit.bytes().used(), before.input_credit().bytes().used());
            EXPECT_LE(waiting, waiting_before);
            EXPECT_FALSE(same(credit, before.input_credit())) << "no credit returned";
            segment_credit_returned = segment_credit_returned || waiting < waiting_before;
          }
          // A property of the recordings: a session keeps the budgets its Ready reported.
          const v1::InputCredit& budgets = ready.status().input_credit();
          EXPECT_EQ(credit.bytes().limit(), budgets.bytes().limit());
          EXPECT_EQ(credit.waiting_segments().limit(), budgets.waiting_segments().limit());
        }
      }
    }
    // A property of the recordings: the text-to-speech one shows a waiting segment's credit return.
    EXPECT_TRUE(speech || segment_credit_returned);

    ASSERT_TRUE(script.back().server && script.back().server->has_session_closed());
    const v1::SessionClosed& closed = script.back().server->session_closed();
    EXPECT_EQ(closed.state(), v1::SESSION_STATE_CLOSED);
    EXPECT_FALSE(closed.has_cause());
    EXPECT_EQ(closed.admission(), v1::ADMISSION_ADMITTED);
    EXPECT_EQ(closed.last_accepted_sequence(), accepted.accepted_sequence());
    // A property of the recordings: all the input sent was accepted, so the totals count all of it.
    ASSERT_FALSE(inputs.empty());
    EXPECT_EQ(accepted.accepted_sequence(), *inputs.rbegin());
    EXPECT_EQ(closed.totals().input_audio_samples(), audio_samples);
    EXPECT_EQ(closed.totals().input_text_bytes(), text_bytes);
    const std::uint64_t sample_bytes = bytes_per_sample(ready.output_format());
    EXPECT_EQ(closed.totals().output_audio_samples(),
              sample_bytes == 0 ? 0 : output_bytes / sample_bytes);
    EXPECT_EQ(closed.totals().utterances_completed(), finals);
    EXPECT_EQ(closed.totals().segments_completed(), segments);
  }
}

TEST(StreamEnvelope, SpeechToTextScriptKeepsSampleAndTimeUnits) {
  const auto script = load_script("stt_session.frames");
  ASSERT_TRUE(opens_and_is_ready(script));
  const v1::Open& open = script[0].client->open();
  const v1::AudioFormat& format = open.input_format();
  EXPECT_EQ(open.mode(), v1::SESSION_MODE_STT_STREAMING);
  EXPECT_TRUE(same(script[1].server->ready().input_format(), format));
  const std::uint64_t rate = format.sample_rate_hz();
  const std::uint64_t sample_bytes = bytes_per_sample(format);
  ASSERT_NE(rate, 0U);
  ASSERT_NE(sample_bytes, 0U);
  // The frame lengths the deployment admits, which are never outside 20 to 320 ms.
  const v1::SpeechToTextLimits& limits = script[1].server->ready().limits().speech_to_text();
  EXPECT_GE(limits.min_frame_ms(), 20U);
  EXPECT_LE(limits.min_frame_ms(), limits.max_frame_ms());
  EXPECT_LE(limits.max_frame_ms(), 320U);

  // Events the client has still to send: after the last, it closes its sending half.
  auto client_events = std::count_if(script.begin(), script.end(),
                                     [](const Event& event) { return event.client.has_value(); });
  std::uint64_t sent = 0;          // samples the client has sent
  std::uint64_t covered = 0;       // where the last finished utterance ended, in samples
  std::uint64_t ended = 0;         // id of the last utterance with an endpoint
  std::uint64_t finished = 0;      // id of the last finished utterance
  std::uint64_t revised = 0;       // utterance of the last partial
  std::uint32_t revision = 0;      // and its revision
  std::string hypothesis;          // text of the open utterance's last partial
  std::uint64_t finalize = 0;      // sequence of a Finalize that no FinalTranscript has answered
  bool audio_after_final = false;  // without it nothing shows that offsets are not reset
  bool short_body = false;         // the last Audio was shorter than a frame
  bool short_bodies = false;       // without one nothing shows that a last body may be
  std::optional<v1::EndpointDetected> endpoint;
  v1::EndpointReason ended_by = v1::ENDPOINT_REASON_UNSPECIFIED;  // the last endpoint's reason
  for (const Event& event : script) {
    SCOPED_TRACE(event.name);
    if (event.client) {
      --client_events;
      EXPECT_TRUE(!short_body || event.client->has_finalize())
          << "only the last body before a Finalize or the half-close may be shorter than a frame";
      short_body = false;
    }
    if (event.client && event.client->has_audio()) {
      const v1::Audio& audio = event.client->audio();
      EXPECT_EQ(finalize, 0U) << "input before the FinalTranscript that answers Finalize";
      EXPECT_EQ(audio.sample_offset(), sent);
      EXPECT_EQ(audio.data().size(), audio.sample_count() * sample_bytes);
      EXPECT_LE(audio.sample_count() * 1000ULL, limits.max_frame_ms() * rate) << "a longer frame";
      short_body = audio.sample_count() * 1000ULL < limits.min_frame_ms() * rate;
      short_bodies = short_bodies || short_body;
      sent += audio.sample_count();
      audio_after_final = audio_after_final || finished != 0;
    } else if (event.client && event.client->has_finalize()) {
      // A property of this recording: the schema allows a repeat, which starts nothing.
      EXPECT_EQ(finalize, 0U) << "a Finalize before the one before it was answered";
      finalize = event.client->sequence();
    } else if (event.server && event.server->has_partial_transcript()) {
      const v1::PartialTranscript& partial = event.server->partial_transcript();
      EXPECT_GT(partial.utterance_id(), ended) << "a partial after its utterance's endpoint";
      if (partial.utterance_id() == revised) {
        EXPECT_GT(partial.revision(), revision);
      }
      revised = partial.utterance_id();
      revision = partial.revision();
      hypothesis = partial.text();
    } else if (event.server && event.server->has_endpoint_detected()) {
      // Exactly one per utterance, before the utterance's FinalTranscript.
      EXPECT_FALSE(endpoint.has_value()) << "the utterance before has no FinalTranscript";
      endpoint = event.server->endpoint_detected();
      EXPECT_GT(endpoint->utterance_id(), ended);
      ended = endpoint->utterance_id();
      EXPECT_GE(endpoint->end_sample_offset(), covered);
      EXPECT_LE(endpoint->end_sample_offset(), sent);
      ended_by = endpoint->reason();
      EXPECT_TRUE(ended_by != v1::ENDPOINT_REASON_HALF_CLOSE || client_events == 0)
          << "a half-close before the client's last event";
      // A property of this recording: a Finalize ends an utterance exactly when one waits.
      EXPECT_EQ(ended_by == v1::ENDPOINT_REASON_CLIENT_FINALIZE, finalize != 0);
    } else if (event.server && event.server->has_final_transcript()) {
      const v1::FinalTranscript& final_transcript = event.server->final_transcript();
      ASSERT_TRUE(endpoint.has_value()) << "FinalTranscript without EndpointDetected";
      EXPECT_EQ(final_transcript.utterance_id(), endpoint->utterance_id());
      EXPECT_EQ(final_transcript.end_sample_offset(), endpoint->end_sample_offset());
      // Zero when the utterance ended without a Finalize.
      EXPECT_EQ(final_transcript.finalize_sequence(), finalize);
      // An utterance's audio starts where the one before ended. That its segments
      // are in order, apart and never empty is a property of this recording.
      std::uint64_t from_us = start_us_of(covered, rate);
      const std::uint64_t until_us = end_us_of(final_transcript.end_sample_offset(), rate);
      std::string text;
      for (const v1::TranscriptSegment& segment : final_transcript.segments()) {
        EXPECT_GE(segment.start_us(), from_us);
        EXPECT_LT(segment.start_us(), segment.end_us());
        EXPECT_LE(segment.end_us(), until_us);
        from_us = segment.end_us();
        text += segment.text();
      }
      // Properties of this recording, not of the schema: the last hypothesis is the
      // transcript, which is the segments' text with nothing between, and each spoken
      // utterance lasts to its endpoint, so a time in another unit ends short of it.
      EXPECT_EQ(text, hypothesis);
      if (final_transcript.segments_size() > 0) {
        EXPECT_EQ(final_transcript.segments().rbegin()->end_us(), until_us);
      }
      finished = final_transcript.utterance_id();
      covered = final_transcript.end_sample_offset();
      endpoint.reset();
      hypothesis.clear();
      finalize = 0;
    }
  }
  EXPECT_FALSE(endpoint.has_value()) << "the last utterance has no FinalTranscript";
  EXPECT_EQ(finalize, 0U) << "a Finalize has no FinalTranscript";
  EXPECT_TRUE(audio_after_final);
  EXPECT_TRUE(short_bodies);
  // A property of this recording: the client closes its sending half with an utterance open.
  EXPECT_EQ(ended_by, v1::ENDPOINT_REASON_HALF_CLOSE);
}

TEST(StreamEnvelope, TranscriptIntervalsRoundOutward) {
  const auto frames = load_frames("lifecycle.frames");
  const auto frame = std::find_if(frames.begin(), frames.end(), [](const Frame& candidate) {
    return candidate.name == "final_between_microseconds";
  });
  ASSERT_NE(frame, frames.end()) << "lifecycle.frames has no frame final_between_microseconds";
  v1::ServerEvent event;
  ASSERT_TRUE(event.ParseFromString(frame->bytes));
  ASSERT_EQ(event.final_transcript().segments_size(), 1);
  const v1::TranscriptSegment& segment = event.final_transcript().segments(0);
  // At 16,000 Hz a sample boundary is 62.5 microseconds: samples 1 up to 3 are 62.5 to 187.5.
  constexpr std::uint64_t rate = 16'000;
  EXPECT_EQ(segment.start_us(), start_us_of(1, rate));
  EXPECT_EQ(segment.start_us(), 62U);
  EXPECT_EQ(segment.end_us(), end_us_of(3, rate));
  EXPECT_EQ(segment.end_us(), 188U);
}

TEST(StreamEnvelope, TextToSpeechScriptDeliversWholeSegmentsInOrder) {
  const auto script = load_script("tts_session.frames");
  ASSERT_TRUE(opens_and_is_ready(script));
  const v1::AudioFormat& format = script[1].server->ready().output_format();
  EXPECT_EQ(script[0].client->open().mode(), v1::SESSION_MODE_TTS_STREAMING);
  const std::uint64_t sample_bytes = bytes_per_sample(format);
  const std::uint64_t rate = format.sample_rate_hz();
  const std::uint64_t full_chunk = rate / 50;  // 20 ms
  ASSERT_NE(sample_bytes, 0U);
  ASSERT_NE(full_chunk, 0U);
  // What the deployment admits; a segment is never above 4,096 bytes.
  const v1::TextToSpeechLimits& limits = script[1].server->ready().limits().text_to_speech();
  EXPECT_LE(limits.max_segment_text_bytes(), 4096U);

  struct Delivery {
    std::uint64_t segment_id = 0;
    std::uint64_t samples = 0;
    std::uint32_t chunks = 0;
    bool ended_short = false;
  };
  std::optional<Delivery> delivery;  // the segment whose chunks are arriving
  std::uint64_t submitted_id = 0;
  std::uint32_t submitted = 0;
  std::uint64_t submitted_bytes = 0;
  std::uint64_t completed_id = 0;
  std::uint32_t completed = 0;
  std::uint64_t completed_samples = 0;
  int syntheses_completed = 0;
  std::uint64_t finalize = 0;  // sequence of a Finalize that no SynthesisCompleted has answered
  bool exact_multiple = false;
  bool short_last_chunk = false;
  for (const Event& event : script) {
    SCOPED_TRACE(event.name);
    if (event.client && event.client->has_text_segment()) {
      const v1::TextSegment& segment = event.client->text_segment();
      EXPECT_EQ(finalize, 0U) << "input before the SynthesisCompleted that answers Finalize";
      EXPECT_GT(segment.segment_id(), submitted_id);
      EXPECT_LE(segment.text().size(), limits.max_segment_text_bytes());
      submitted_id = segment.segment_id();
      submitted_bytes += segment.text().size();
      ++submitted;
    } else if (event.client && event.client->has_finalize()) {
      // A property of this recording: the schema allows a repeat, which starts nothing.
      EXPECT_EQ(finalize, 0U) << "a Finalize before the one before it was answered";
      finalize = event.client->sequence();
    } else if (event.server && event.server->has_audio_chunk()) {
      const v1::AudioChunk& chunk = event.server->audio_chunk();
      if (!delivery) {
        EXPECT_GT(chunk.segment_id(), completed_id);
        EXPECT_LE(chunk.segment_id(), submitted_id);
        delivery.emplace().segment_id = chunk.segment_id();
      }
      EXPECT_EQ(chunk.segment_id(), delivery->segment_id) << "segments are interleaved";
      EXPECT_EQ(chunk.chunk_index(), delivery->chunks);
      EXPECT_EQ(chunk.segment_sample_offset(), delivery->samples);
      EXPECT_TRUE(same(chunk.format(), format));
      EXPECT_FALSE(delivery->ended_short) << "only a segment's last chunk may be short";
      const std::uint64_t samples = chunk.data().size() / sample_bytes;
      EXPECT_EQ(chunk.data().size(), samples * sample_bytes);
      EXPECT_GT(samples, 0U);
      EXPECT_LE(samples, full_chunk);
      delivery->ended_short = samples < full_chunk;
      delivery->samples += samples;
      ++delivery->chunks;
    } else if (event.server && event.server->has_segment_completed()) {
      const v1::SegmentCompleted& done = event.server->segment_completed();
      ASSERT_TRUE(delivery.has_value()) << "SegmentCompleted without a chunk before it";
      EXPECT_EQ(done.segment_id(), delivery->segment_id);
      EXPECT_EQ(done.total_samples(), delivery->samples);
      EXPECT_EQ(done.chunk_count(), delivery->chunks);
      EXPECT_EQ(done.chunk_count(), (done.total_samples() + full_chunk - 1) / full_chunk);
      EXPECT_LE(done.total_samples() * 1000, limits.max_segment_audio_ms() * rate);
      (delivery->samples % full_chunk == 0 ? exact_multiple : short_last_chunk) = true;
      completed_id = delivery->segment_id;
      completed_samples += delivery->samples;
      ++completed;
      delivery.reset();
    } else if (event.server && event.server->has_synthesis_completed()) {
      const v1::SynthesisCompleted& done = event.server->synthesis_completed();
      EXPECT_NE(finalize, 0U) << "SynthesisCompleted answers a Finalize";
      EXPECT_EQ(done.finalize_sequence(), finalize);
      finalize = 0;
      EXPECT_FALSE(delivery.has_value()) << "a segment has chunks and no SegmentCompleted";
      EXPECT_EQ(completed, submitted) << "a submitted segment has no SegmentCompleted";
      EXPECT_EQ(done.segment_count(), completed);
      EXPECT_EQ(done.total_samples(), completed_samples);
      // The bounds on the segments of one synthesis together; this recording has one.
      EXPECT_LE(submitted_bytes, limits.max_synthesis_text_bytes());
      EXPECT_LE(done.total_samples() * 1000, limits.max_synthesis_audio_ms() * rate);
      ++syntheses_completed;
    }
  }
  EXPECT_EQ(syntheses_completed, 1);
  EXPECT_EQ(finalize, 0U) << "a Finalize has no SynthesisCompleted";
  EXPECT_TRUE(exact_multiple) << "no segment ends on a full chunk";
  EXPECT_TRUE(short_last_chunk) << "no segment ends on a short chunk";
}

TEST(StreamEnvelope, ServiceIsOneBidirectionalSessionCall) {
  EXPECT_EQ(schema()->package(), "tensorplate.stream.v1");
  ASSERT_EQ(schema()->service_count(), 1);
  const auto* service = schema()->service(0);
  EXPECT_EQ(service->full_name(), "tensorplate.stream.v1.SessionService");
  EXPECT_EQ(service->full_name(), v1::SessionService::service_full_name());
  ASSERT_EQ(service->method_count(), 1);
  const auto* method = service->method(0);
  EXPECT_EQ(method->name(), "Session");
  EXPECT_TRUE(method->client_streaming());
  EXPECT_TRUE(method->server_streaming());
  EXPECT_EQ(method->input_type(), v1::ClientEvent::descriptor());
  EXPECT_EQ(method->output_type(), v1::ServerEvent::descriptor());
}

TEST(StreamEnvelope, FieldsKeepTheirTypesNumbersAndLabels) {
  pb::FileDescriptorProto file;
  schema()->CopyTo(&file);
  EXPECT_EQ(file.syntax(), "proto3");

  std::set<std::string> declared;
  for (const auto* message : schema_messages()) {
    for (int i = 0; i < message->field_count(); ++i) {
      declared.insert(declaration(message->field(i)));
    }
  }
  std::set<std::string> listed;
  std::istringstream lines{std::string{kFields}};
  for (std::string line; std::getline(lines, line);) {
    if (!line.empty()) {
      listed.insert(line);
    }
  }
  for (const auto& field : declared) {
    EXPECT_TRUE(listed.contains(field)) << "the schema declares, and kFields lacks: " << field;
  }
  for (const auto& field : listed) {
    EXPECT_TRUE(declared.contains(field)) << "kFields has, and the schema lacks: " << field;
  }
}

TEST(StreamEnvelope, EnvelopeNumbersFollowTheLayout) {
  for (const auto* envelope : {v1::ClientEvent::descriptor(), v1::ServerEvent::descriptor()}) {
    SCOPED_TRACE(envelope->full_name());
    const auto* sequence = envelope->FindFieldByNumber(1);
    ASSERT_NE(sequence, nullptr);
    EXPECT_EQ(sequence->name(), "sequence");
    EXPECT_EQ(sequence->type(), pb::FieldDescriptor::TYPE_UINT64);
    EXPECT_FALSE(sequence->is_repeated());

    ASSERT_EQ(envelope->real_oneof_decl_count(), 1);
    EXPECT_EQ(envelope->oneof_decl(0)->name(), "body");
    for (int i = 0; i < envelope->field_count(); ++i) {
      const auto* field = envelope->field(i);
      SCOPED_TRACE(field->name());
      if (field->real_containing_oneof() == nullptr) {
        EXPECT_LE(field->number(), 9) << "an envelope field outside 1 to 9";
        continue;
      }
      EXPECT_GE(field->number(), 10) << "a body outside 10 to 39";
      EXPECT_LE(field->number(), 39) << "a body outside 10 to 39";
      EXPECT_EQ(field->type(), pb::FieldDescriptor::TYPE_MESSAGE);
    }
    for (int number = 40; number <= 99; ++number) {
      EXPECT_TRUE(envelope->IsReservedNumber(number)) << number;
    }
  }
}

TEST(StreamEnvelope, OpenReservesTheOperatorOnlyNames) {
  for (const char* name : {"admission_mode", "test_count", "evidence_ref"}) {
    EXPECT_TRUE(v1::Open::descriptor()->IsReservedName(name)) << name;
  }
}

// Kept for a cancel scoped to text segments, which is not defined yet.
TEST(StreamEnvelope, SegmentCancelStaysReserved) {
  EXPECT_TRUE(v1::ClientEvent::descriptor()->IsReservedNumber(15));
  EXPECT_TRUE(v1::ClientEvent::descriptor()->IsReservedName("cancel_segments"));
  EXPECT_TRUE(v1::ServerEvent::descriptor()->IsReservedNumber(16));
  EXPECT_TRUE(v1::ServerEvent::descriptor()->IsReservedName("segments_cancelled"));
  EXPECT_TRUE(v1::Capability_descriptor()->IsReservedNumber(1));
  EXPECT_TRUE(v1::Capability_descriptor()->IsReservedName("CAPABILITY_SEGMENT_CANCEL"));
}

// Each comparison is of whole lists, so a name, a number or a count that
// differs on either side fails it.
TEST(StreamEnvelope, EnumsMirrorTheirSources) {
  EXPECT_EQ(names_by_number(v1::ErrorCode_descriptor()),
            mirror_of("ERROR_CODE_", schema_enum("error.json", "code")));
  EXPECT_EQ(names_by_number(v1::FailureReason_descriptor()),
            mirror_of("FAILURE_REASON_", schema_enum("failure_reason.json", "reason")));

  // The states are counted by is_session_state and named by the runtime.
  std::vector<std::string> states;
  for (unsigned value = 0; value <= 255; ++value) {
    const auto state = static_cast<tensorplate::serving::LogicalSessionState>(value);
    if (!is_session_state(state)) {
      break;
    }
    states.emplace_back(tensorplate::serving::to_string(state));
  }
  EXPECT_EQ(names_by_number(v1::SessionState_descriptor()), mirror_of("SESSION_STATE_", states));
}

TEST(StreamEnvelope, UnknownAdditionsAreReadAsTheSchemaSays) {
  const auto frames = load_frames("extensibility.frames");
  const auto bytes_of = [&frames](std::string_view name) {
    const auto frame = std::find_if(frames.begin(), frames.end(), [name](const Frame& candidate) {
      return candidate.name == name;
    });
    EXPECT_NE(frame, frames.end()) << "extensibility.frames has no frame " << name;
    return frame == frames.end() ? std::string{} : frame->bytes;
  };
  std::size_t of_another_schema = 0;
  for (const Frame& frame : all_frames()) {
    of_another_schema += schema_type(frame) == nullptr ? 1 : 0;
  }
  EXPECT_EQ(of_another_schema, 7U) << "a frame of extension.proto that nothing below reads";

  // What a reader is handed is all this shows: ignoring or refusing it is the receiver's to do.

  // An added envelope field and an added field of a known body, which a receiver ignores:
  // the event decodes, what is known is intact and what is not is kept.
  v1::ClientEvent client_fields;
  ASSERT_TRUE(client_fields.ParseFromString(bytes_of("added_client_fields")));
  EXPECT_EQ(client_fields.sequence(), 1U);
  EXPECT_EQ(client_fields.open().resolved().deployment_id(), "synthetic-deployment");
  EXPECT_EQ(client_fields.open().resolved().generation(), 7U);
  EXPECT_EQ(client_fields.open().mode(), v1::SESSION_MODE_STT_STREAMING);
  EXPECT_EQ(unknown_fields(client_fields), "4=5 ");
  EXPECT_EQ(unknown_fields(client_fields.open()), "10='synthetic addition' ");

  v1::ServerEvent server_fields;
  ASSERT_TRUE(server_fields.ParseFromString(bytes_of("added_server_fields")));
  EXPECT_EQ(server_fields.sequence(), 1U);
  EXPECT_EQ(server_fields.ready().session_id(), "synthetic-session-1");
  EXPECT_EQ(server_fields.ready().generation(), 7U);
  EXPECT_EQ(unknown_fields(server_fields), "2=5 ");
  EXPECT_EQ(unknown_fields(server_fields.ready()), "10='synthetic addition' ");

  // A body in the reserved range, which a server refuses and a client ignores: the event
  // decodes with no body at all. "\x0a\x0e" and the text are extension.proto's AddedBody.
  const std::string added_body = "40='\x0a\x0esynthetic body' ";
  v1::ClientEvent client_body;
  ASSERT_TRUE(client_body.ParseFromString(bytes_of("added_client_body")));
  EXPECT_EQ(client_body.body_case(), v1::ClientEvent::BODY_NOT_SET);
  EXPECT_EQ(client_body.sequence(), 2U);
  EXPECT_EQ(client_body.session_id(), "synthetic-session-1");
  EXPECT_EQ(unknown_fields(client_body), added_body);

  v1::ServerEvent server_body;
  ASSERT_TRUE(server_body.ParseFromString(bytes_of("added_server_body")));
  EXPECT_EQ(server_body.body_case(), v1::ServerEvent::BODY_NOT_SET);
  EXPECT_EQ(server_body.sequence(), 2U);
  EXPECT_EQ(unknown_fields(server_body), added_body);

  // An enum value this schema does not name stays the number that was sent, in its field. A
  // receiver reads it as unspecified, and a server refuses an Open with such a mode.
  v1::ClientEvent added_mode;
  ASSERT_TRUE(added_mode.ParseFromString(bytes_of("added_mode")));
  EXPECT_EQ(added_mode.open().resolved().deployment_id(), "synthetic-deployment");
  EXPECT_EQ(static_cast<int>(added_mode.open().mode()), 3);
  EXPECT_FALSE(v1::SessionMode_IsValid(added_mode.open().mode()));
  EXPECT_EQ(unknown_field_count(added_mode), 0);

  v1::ClientEvent client_capability;
  ASSERT_TRUE(client_capability.ParseFromString(bytes_of("added_client_capability")));
  EXPECT_EQ(client_capability.open().mode(), v1::SESSION_MODE_TTS_STREAMING);
  ASSERT_EQ(client_capability.open().capabilities_size(), 1);
  EXPECT_EQ(static_cast<int>(client_capability.open().capabilities(0)), 1);
  EXPECT_FALSE(v1::Capability_IsValid(client_capability.open().capabilities(0)));
  EXPECT_EQ(unknown_field_count(client_capability), 0);

  v1::ServerEvent server_capability;
  ASSERT_TRUE(server_capability.ParseFromString(bytes_of("added_server_capability")));
  EXPECT_EQ(server_capability.ready().session_id(), "synthetic-session-1");
  ASSERT_EQ(server_capability.ready().capabilities_size(), 1);
  EXPECT_EQ(static_cast<int>(server_capability.ready().capabilities(0)), 1);
  EXPECT_FALSE(v1::Capability_IsValid(server_capability.ready().capabilities(0)));
  EXPECT_EQ(unknown_field_count(server_capability), 0);
}

}  // namespace
