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

std::unique_ptr<pb::Message> new_message(const pb::Descriptor* type) {
  return std::unique_ptr<pb::Message>(
      pb::MessageFactory::generated_factory()->GetPrototype(type)->New());
}

// Decoded frames of stream/v1's own types, in file order.
std::vector<std::pair<Frame, std::unique_ptr<pb::Message>>> golden_messages() {
  std::vector<std::pair<Frame, std::unique_ptr<pb::Message>>> messages;
  for (auto& frame : all_frames()) {
    if (const auto* type = schema_type(frame)) {
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
// "repeated TranscriptSegment FinalTranscript.segments = 3".
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
uint32 SessionLimits.idle_timeout_ms = 1
uint32 SessionLimits.heartbeat_interval_ms = 2
uint32 SessionLimits.liveness_timeout_ms = 3
uint32 SessionLimits.max_duration_ms = 4
uint32 SessionLimits.finalize_deadline_ms = 5
uint32 SessionLimits.output_stall_timeout_ms = 6
ErrorCode EndCause.code = 1
string EndCause.reason = 2
FailureReason EndCause.failure_reason = 3
uint64 ClientEvent.sequence = 1
string ClientEvent.session_id = 2
uint64 ClientEvent.generation = 3
Open ClientEvent.open = 10 (oneof body)
Finalize ClientEvent.finalize = 11 (oneof body)
Cancel ClientEvent.cancel = 12 (oneof body)
Ping ClientEvent.ping = 13 (oneof body)
StatusRequest ClientEvent.status_request = 14 (oneof body)
Audio ClientEvent.audio = 20 (oneof body)
TextSegment ClientEvent.text_segment = 21 (oneof body)
string Open.deployment_id = 1
string Open.descriptor_digest = 2
SessionMode Open.mode = 3
string Open.language = 4
AudioFormat Open.input_format = 5
string Open.voice = 6
string Open.trace_id = 7
string Open.turn_id = 8
bytes Audio.data = 1
uint32 Audio.sample_count = 2
uint64 Audio.sample_offset = 3
uint64 TextSegment.segment_id = 1
string TextSegment.text = 2
string TextSegment.turn_id = 3
uint64 ServerEvent.sequence = 1
Ready ServerEvent.ready = 10 (oneof body)
Accepted ServerEvent.accepted = 11 (oneof body)
Status ServerEvent.status = 12 (oneof body)
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
Status Ready.status = 8
uint64 Accepted.accepted_sequence = 1
InputCredit Accepted.input_credit = 2
SessionState Status.state = 1
uint32 Status.input_queue_depth = 2
InputCredit Status.input_credit = 3
Usage Status.output_pcm_bytes = 4
Usage Status.output_metadata_bytes = 5
SessionState SessionClosed.state = 1
EndCause SessionClosed.cause = 2
uint64 SessionClosed.last_accepted_sequence = 3
uint64 SessionClosed.last_produced_sequence = 4
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
string FinalTranscript.text = 2
repeated TranscriptSegment FinalTranscript.segments = 3
uint64 FinalTranscript.end_sample_offset = 4
uint64 AudioChunk.segment_id = 1
uint32 AudioChunk.chunk_index = 2
uint64 AudioChunk.sample_offset = 3
AudioFormat AudioChunk.format = 4
bytes AudioChunk.data = 5
uint64 SegmentCompleted.segment_id = 1
uint64 SegmentCompleted.total_samples = 2
uint32 SegmentCompleted.chunk_count = 3
uint32 SynthesisCompleted.segment_count = 1
uint64 SynthesisCompleted.total_samples = 2
)";

// A property of the recordings: the code and the outcome the session layer
// gives each end with a cause that the frames show
// (docs/architecture/serving-worker.md, "How sessions end").
struct End {
  std::string_view reason;
  v1::ErrorCode code;
  v1::SessionState state;
};
constexpr End kEnds[] = {
    {"client_cancelled", v1::ERROR_CODE_CANCELLED, v1::SESSION_STATE_CLOSED},
    {"input_credit_exceeded", v1::ERROR_CODE_RESOURCE_EXHAUSTED, v1::SESSION_STATE_FAILED},
    {"deployment_retired", v1::ERROR_CODE_UNAVAILABLE, v1::SESSION_STATE_CLOSED},
    {"idle_timeout", v1::ERROR_CODE_TIMEOUT, v1::SESSION_STATE_CLOSED},
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
    for_each_message(*message, [&used](const pb::Message& part) {
      std::vector<const pb::FieldDescriptor*> fields;
      part.GetReflection()->ListFields(part, &fields);
      for (const auto* field : fields) {
        used.emplace(field->full_name());
        if (field->cpp_type() == pb::FieldDescriptor::CPPTYPE_ENUM && !field->is_repeated()) {
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
  const auto failure_reasons = schema_enum("failure_reason.json", "reason");
  for (const auto& [frame, message] : golden_messages()) {
    SCOPED_TRACE(frame.file + ": " + frame.name);
    for_each_message(*message, [&failure_reasons](const pb::Message& part) {
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
        if (closed->has_cause()) {
          const auto* end = std::find_if(
              std::begin(kEnds), std::end(kEnds),
              [closed](const End& known) { return known.reason == closed->cause().reason(); });
          ASSERT_NE(end, std::end(kEnds)) << "list this end in kEnds with its code and outcome";
          EXPECT_EQ(closed->cause().code(), end->code);
          EXPECT_EQ(closed->state(), end->state);
        }
      }
      if (const auto* cause = dynamic_cast<const v1::EndCause*>(&part)) {
        EXPECT_NE(cause->code(), v1::ERROR_CODE_UNSPECIFIED);
        const std::string& reason = cause->reason();
        EXPECT_TRUE(is_snake_case(reason) && reason.size() <= 64) << reason;
        // Numbered as the taxonomy lists them, which EnumsMirrorTheirSources holds.
        const auto listed = std::find(failure_reasons.begin(), failure_reasons.end(), reason);
        const auto expected =
            listed == failure_reasons.end() ? 0 : 1 + (listed - failure_reasons.begin());
        EXPECT_EQ(cause->failure_reason(), expected) << reason;
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
    const v1::Ready& ready = script[1].server->ready();
    EXPECT_TRUE(open.session_id().empty());
    EXPECT_NE(open.generation(), 0U);
    EXPECT_FALSE(ready.session_id().empty());
    EXPECT_EQ(ready.generation(), open.generation());
    // A property of the recordings: the client resolved the descriptor that is served.
    EXPECT_EQ(ready.descriptor_digest(), open.open().descriptor_digest());
    // The active state is the one in which Ready has been sent (session.hpp).
    EXPECT_EQ(ready.status().state(), v1::SESSION_STATE_ACTIVE);

    std::uint64_t client_sequence = 0;
    std::uint64_t server_sequence = 0;
    std::uint64_t accepted_sequence = 0;
    for (std::size_t i = 0; i < script.size(); ++i) {
      SCOPED_TRACE(script[i].name);
      if (script[i].client) {
        const v1::ClientEvent& event = *script[i].client;
        EXPECT_EQ(event.sequence(), ++client_sequence);
        if (i > 0) {
          EXPECT_FALSE(event.has_open());
          EXPECT_EQ(event.session_id(), ready.session_id());
          EXPECT_EQ(event.generation(), ready.generation());
        }
      } else if (script[i].server) {
        const v1::ServerEvent& event = *script[i].server;
        EXPECT_EQ(event.sequence(), ++server_sequence);
        EXPECT_EQ(event.has_ready(), i == 1);
        EXPECT_EQ(event.has_session_closed(), i + 1 == script.size());
        if (event.has_accepted()) {
          const v1::Accepted& accepted = event.accepted();
          EXPECT_LE(accepted.accepted_sequence(), client_sequence) << "an event not sent yet";
          EXPECT_GE(accepted.accepted_sequence(), accepted_sequence) << "acceptance went back";
          accepted_sequence = accepted.accepted_sequence();
          // A property of the recordings: a session keeps the budgets its Ready reported.
          const v1::InputCredit& credit = ready.status().input_credit();
          EXPECT_EQ(accepted.input_credit().bytes().limit(), credit.bytes().limit());
          EXPECT_EQ(accepted.input_credit().waiting_segments().limit(),
                    credit.waiting_segments().limit());
        }
      }
    }

    ASSERT_TRUE(script.back().server && script.back().server->has_session_closed());
    const v1::SessionClosed& closed = script.back().server->session_closed();
    EXPECT_EQ(closed.state(), v1::SESSION_STATE_CLOSED);
    EXPECT_FALSE(closed.has_cause());
    EXPECT_EQ(closed.last_accepted_sequence(), client_sequence);
    EXPECT_EQ(closed.last_produced_sequence(), server_sequence - 1);
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

  std::uint64_t sent = 0;          // samples the client has sent
  std::uint64_t covered = 0;       // where the last finished utterance ended, in samples
  std::uint64_t finished = 0;      // id of the last finished utterance
  std::uint64_t revised = 0;       // utterance of the last partial
  std::uint32_t revision = 0;      // and its revision
  bool audio_after_final = false;  // without it nothing shows that offsets are not reset
  bool finalizing = false;         // a Finalize that no FinalTranscript has answered
  std::optional<v1::EndpointDetected> endpoint;
  for (const Event& event : script) {
    SCOPED_TRACE(event.name);
    if (event.client && event.client->has_audio()) {
      const v1::Audio& audio = event.client->audio();
      EXPECT_FALSE(finalizing) << "input before the FinalTranscript that answers Finalize";
      EXPECT_EQ(audio.sample_offset(), sent);
      EXPECT_EQ(audio.data().size(), audio.sample_count() * sample_bytes);
      EXPECT_GE(audio.sample_count() * 1000ULL, 20 * rate) << "less than 20 ms";
      EXPECT_LE(audio.sample_count() * 1000ULL, 320 * rate) << "more than 320 ms";
      sent += audio.sample_count();
      audio_after_final = audio_after_final || finished != 0;
    } else if (event.client && event.client->has_finalize()) {
      // A property of this recording: the schema allows a repeat, which starts nothing.
      EXPECT_FALSE(finalizing) << "a Finalize before the one before it was answered";
      finalizing = true;
    } else if (event.server && event.server->has_partial_transcript()) {
      const v1::PartialTranscript& partial = event.server->partial_transcript();
      EXPECT_GT(partial.utterance_id(), finished) << "a final transcript is never revised";
      if (partial.utterance_id() == revised) {
        EXPECT_GT(partial.revision(), revision);
      }
      revised = partial.utterance_id();
      revision = partial.revision();
    } else if (event.server && event.server->has_endpoint_detected()) {
      // Properties of this recording, not of the schema: each utterance's endpoint
      // comes before its FinalTranscript, ended by a Finalize exactly when one waits.
      EXPECT_FALSE(endpoint.has_value()) << "the utterance before has no FinalTranscript";
      endpoint = event.server->endpoint_detected();
      EXPECT_GT(endpoint->utterance_id(), finished);
      EXPECT_GE(endpoint->end_sample_offset(), covered);
      EXPECT_LE(endpoint->end_sample_offset(), sent);
      EXPECT_EQ(endpoint->reason() == v1::ENDPOINT_REASON_CLIENT_FINALIZE, finalizing)
          << "ended by a Finalize exactly when one is waiting";
    } else if (event.server && event.server->has_final_transcript()) {
      const v1::FinalTranscript& final_transcript = event.server->final_transcript();
      // As above: the schema does not order the two events.
      ASSERT_TRUE(endpoint.has_value()) << "FinalTranscript without EndpointDetected";
      EXPECT_EQ(final_transcript.utterance_id(), endpoint->utterance_id());
      EXPECT_EQ(final_transcript.end_sample_offset(), endpoint->end_sample_offset());
      // An utterance's audio starts where the one before ended. That its segments
      // are in order, apart and never empty is a property of this recording.
      std::uint64_t from_us = covered * 1'000'000 / rate;
      const std::uint64_t until_us = final_transcript.end_sample_offset() * 1'000'000 / rate;
      for (const v1::TranscriptSegment& segment : final_transcript.segments()) {
        EXPECT_GE(segment.start_us(), from_us);
        EXPECT_LT(segment.start_us(), segment.end_us());
        EXPECT_LE(segment.end_us(), until_us);
        from_us = segment.end_us();
      }
      // A property of this recording, not of the schema: each spoken utterance
      // lasts to its endpoint, so a time in another unit ends short of it.
      if (final_transcript.segments_size() > 0) {
        EXPECT_EQ(final_transcript.segments().rbegin()->end_us(), until_us);
      }
      finished = final_transcript.utterance_id();
      covered = final_transcript.end_sample_offset();
      endpoint.reset();
      finalizing = false;
    }
  }
  EXPECT_FALSE(endpoint.has_value()) << "the last utterance has no FinalTranscript";
  EXPECT_FALSE(finalizing) << "a Finalize has no FinalTranscript";
  EXPECT_TRUE(audio_after_final);
}

TEST(StreamEnvelope, TextToSpeechScriptDeliversWholeSegmentsInOrder) {
  const auto script = load_script("tts_session.frames");
  ASSERT_TRUE(opens_and_is_ready(script));
  const v1::AudioFormat& format = script[1].server->ready().output_format();
  EXPECT_EQ(script[0].client->open().mode(), v1::SESSION_MODE_TTS_STREAMING);
  const std::uint64_t sample_bytes = bytes_per_sample(format);
  const std::uint64_t full_chunk = format.sample_rate_hz() / 50;  // 20 ms
  ASSERT_NE(sample_bytes, 0U);
  ASSERT_NE(full_chunk, 0U);

  struct Delivery {
    std::uint64_t segment_id = 0;
    std::uint64_t samples = 0;
    std::uint32_t chunks = 0;
    bool ended_short = false;
  };
  std::optional<Delivery> delivery;  // the segment whose chunks are arriving
  std::uint64_t submitted_id = 0;
  std::uint32_t submitted = 0;
  std::uint64_t completed_id = 0;
  std::uint32_t completed = 0;
  std::uint64_t completed_samples = 0;
  int syntheses_completed = 0;
  bool finalizing = false;  // a Finalize that no SynthesisCompleted has answered
  bool exact_multiple = false;
  bool short_last_chunk = false;
  for (const Event& event : script) {
    SCOPED_TRACE(event.name);
    if (event.client && event.client->has_text_segment()) {
      EXPECT_FALSE(finalizing) << "input before the SynthesisCompleted that answers Finalize";
      EXPECT_GT(event.client->text_segment().segment_id(), submitted_id);
      submitted_id = event.client->text_segment().segment_id();
      ++submitted;
    } else if (event.client && event.client->has_finalize()) {
      // A property of this recording: the schema allows a repeat, which starts nothing.
      EXPECT_FALSE(finalizing) << "a Finalize before the one before it was answered";
      finalizing = true;
    } else if (event.server && event.server->has_audio_chunk()) {
      const v1::AudioChunk& chunk = event.server->audio_chunk();
      if (!delivery) {
        EXPECT_GT(chunk.segment_id(), completed_id);
        EXPECT_LE(chunk.segment_id(), submitted_id);
        delivery.emplace().segment_id = chunk.segment_id();
      }
      EXPECT_EQ(chunk.segment_id(), delivery->segment_id) << "segments are interleaved";
      EXPECT_EQ(chunk.chunk_index(), delivery->chunks);
      EXPECT_EQ(chunk.sample_offset(), delivery->samples);
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
      (delivery->samples % full_chunk == 0 ? exact_multiple : short_last_chunk) = true;
      completed_id = delivery->segment_id;
      completed_samples += delivery->samples;
      ++completed;
      delivery.reset();
    } else if (event.server && event.server->has_synthesis_completed()) {
      const v1::SynthesisCompleted& done = event.server->synthesis_completed();
      EXPECT_TRUE(finalizing) << "SynthesisCompleted answers a Finalize";
      finalizing = false;
      EXPECT_FALSE(delivery.has_value()) << "a segment has chunks and no SegmentCompleted";
      EXPECT_EQ(completed, submitted) << "a submitted segment has no SegmentCompleted";
      EXPECT_EQ(done.segment_count(), completed);
      EXPECT_EQ(done.total_samples(), completed_samples);
      ++syntheses_completed;
    }
  }
  EXPECT_EQ(syntheses_completed, 1);
  EXPECT_FALSE(finalizing) << "a Finalize has no SynthesisCompleted";
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
  EXPECT_EQ(of_another_schema, 4U) << "a frame of extension.proto that nothing below reads";

  const std::string digest = "sha256:" + std::string(64, '0');
  // extension.proto's AddedBody{text: "synthetic body"}, kept as the bytes it arrived in.
  const std::string added_body = std::string{"\x0a\x0e"} + "synthetic body";

  // An added envelope field and an added field of a known body: the event
  // decodes, what is known is intact and what is not is kept.
  v1::ClientEvent added_fields;
  ASSERT_TRUE(added_fields.ParseFromString(bytes_of("added_fields")));
  EXPECT_EQ(added_fields.sequence(), 1U);
  EXPECT_EQ(added_fields.generation(), 7U);
  ASSERT_TRUE(added_fields.has_open());
  EXPECT_EQ(added_fields.open().deployment_id(), "synthetic-deployment");
  EXPECT_EQ(added_fields.open().descriptor_digest(), digest);
  EXPECT_EQ(added_fields.open().mode(), v1::SESSION_MODE_STT_STREAMING);
  const pb::UnknownFieldSet& envelope_unknown = added_fields.unknown_fields();
  ASSERT_EQ(envelope_unknown.field_count(), 1);
  EXPECT_EQ(envelope_unknown.field(0).number(), 4);
  ASSERT_EQ(envelope_unknown.field(0).type(), pb::UnknownField::TYPE_VARINT);
  EXPECT_EQ(envelope_unknown.field(0).varint(), 5U);
  const pb::UnknownFieldSet& open_unknown = added_fields.open().unknown_fields();
  ASSERT_EQ(open_unknown.field_count(), 1);
  EXPECT_EQ(open_unknown.field(0).number(), 9);
  ASSERT_EQ(open_unknown.field(0).type(), pb::UnknownField::TYPE_LENGTH_DELIMITED);
  EXPECT_EQ(open_unknown.field(0).length_delimited(), "synthetic addition");

  // A body in the reserved range: the event decodes with no body at all.
  v1::ClientEvent client_body;
  ASSERT_TRUE(client_body.ParseFromString(bytes_of("added_client_body")));
  EXPECT_EQ(client_body.body_case(), v1::ClientEvent::BODY_NOT_SET);
  EXPECT_EQ(client_body.sequence(), 2U);
  EXPECT_EQ(client_body.session_id(), "synthetic-session-1");
  EXPECT_EQ(client_body.generation(), 7U);
  ASSERT_EQ(client_body.unknown_fields().field_count(), 1);
  EXPECT_EQ(client_body.unknown_fields().field(0).number(), 40);
  ASSERT_EQ(client_body.unknown_fields().field(0).type(), pb::UnknownField::TYPE_LENGTH_DELIMITED);
  EXPECT_EQ(client_body.unknown_fields().field(0).length_delimited(), added_body);

  v1::ServerEvent server_body;
  ASSERT_TRUE(server_body.ParseFromString(bytes_of("added_server_body")));
  EXPECT_EQ(server_body.body_case(), v1::ServerEvent::BODY_NOT_SET);
  EXPECT_EQ(server_body.sequence(), 2U);
  ASSERT_EQ(server_body.unknown_fields().field_count(), 1);
  EXPECT_EQ(server_body.unknown_fields().field(0).number(), 40);
  ASSERT_EQ(server_body.unknown_fields().field(0).type(), pb::UnknownField::TYPE_LENGTH_DELIMITED);
  EXPECT_EQ(server_body.unknown_fields().field(0).length_delimited(), added_body);

  // A mode this schema does not name stays the number that was sent.
  v1::ClientEvent added_mode;
  ASSERT_TRUE(added_mode.ParseFromString(bytes_of("added_mode")));
  ASSERT_TRUE(added_mode.has_open());
  EXPECT_EQ(added_mode.sequence(), 1U);
  EXPECT_EQ(added_mode.generation(), 7U);
  EXPECT_EQ(added_mode.open().deployment_id(), "synthetic-deployment");
  EXPECT_EQ(added_mode.open().descriptor_digest(), digest);
  EXPECT_EQ(static_cast<int>(added_mode.open().mode()), 3);
  EXPECT_FALSE(v1::SessionMode_IsValid(added_mode.open().mode()));
  EXPECT_EQ(unknown_field_count(added_mode), 0);
}

}  // namespace
