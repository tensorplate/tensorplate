// SPDX-License-Identifier: Apache-2.0
// Replays what a real Whisper model and the sidecar's voice activity model
// returned for what the commitment asked (fixtures/transcript_commitment/).
#include <gtest/gtest.h>

#include <algorithm>
#include <array>
#include <cctype>
#include <cstdint>
#include <fstream>
#include <map>
#include <nlohmann/json.hpp>
#include <optional>
#include <set>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include "serving/session/transcript_commitment.hpp"
#include "serving/session/vad_policy.hpp"
#include "serving/session/window_scheduler.hpp"

namespace tensorplate::serving {
namespace {

// The whole-clip replay's decodes: 320 ms in, then every 160 ms.
constexpr std::uint64_t kAccumulation = 5'120;
constexpr std::uint64_t kStride = 2'560;
constexpr std::uint64_t kFrame = 512;

TranscriptResult result_of(const nlohmann::json& entry) {
  TranscriptResult result;
  result.text = entry.at("text").get<std::string>();
  result.tokens = entry.at("tokens").get<std::vector<std::uint32_t>>();
  for (const auto& word : entry.at("words")) {
    result.words.push_back(WordUnit{word.at(0).get<std::string>(), word.at(1).get<std::uint32_t>(),
                                    word.at(2).get<std::uint32_t>(),
                                    word.at(3).get<std::uint64_t>(),
                                    word.at(4).get<std::uint64_t>()});
  }
  return result;
}

struct Record {
  std::string utterances;
  std::uint64_t samples = 0;
  TranscriptResult reference;
  /// Empty when the job failed: a failed partial decode is dropped.
  std::map<std::pair<std::uint64_t, std::uint64_t>, std::optional<TranscriptResult>> decodes;
  /// Frame probabilities from a reset at each start to the clip's end.
  std::map<std::uint64_t, std::vector<float>> voice_activity;
};

Record load(const std::string& name) {
  const std::string path =
      std::string{TP_SOURCE_DIR} + "/test/unit/fixtures/transcript_commitment/" + name;
  std::ifstream in{path};
  EXPECT_TRUE(in) << path;
  const auto json = nlohmann::json::parse(in);
  Record record;
  record.utterances = json.at("utterances").get<std::string>();
  record.samples = json.at("clip").at("samples").get<std::uint64_t>();
  record.reference = result_of(json.at("reference"));
  for (const auto& entry : json.at("decodes")) {
    std::optional<TranscriptResult> result;
    if (!entry.contains("error")) {
      result = result_of(entry);
    }
    record.decodes.emplace(
        std::pair{entry.at("start").get<std::uint64_t>(), entry.at("end").get<std::uint64_t>()},
        std::move(result));
  }
  for (const auto& entry : json.at("voice_activity")) {
    record.voice_activity.emplace(entry.at("start").get<std::uint64_t>(),
                                  entry.at("probabilities").get<std::vector<float>>());
  }
  return record;
}

/// What the replay saw.
struct Replay {
  /// Each utterance's final words, in order.
  std::vector<std::vector<AlignedWord>> finals;
  std::vector<std::pair<std::uint64_t, std::uint64_t>> utterances;
  std::size_t unreconciled = 0;
  std::size_t ring_advances = 0;
  /// The record's entries the replay read.
  std::set<std::pair<std::uint64_t, std::uint64_t>> decodes_read;
  std::set<std::uint64_t> voice_activity_read;
  /// Where admit() refused audio, if it did.
  std::optional<std::uint64_t> unstable_at;
  /// The committed words when it did.
  std::vector<AlignedWord> committed_when_unstable;
};

// One utterance's decodes. A decode or voice activity the record lacks fails
// the case and names it, which is what record.py reads.
class Utterance {
 public:
  Utterance(const Record& record, Replay& seen, std::uint64_t first)
      : record_(record), seen_(seen), commitment_(VadProfile{}, first), first_(first) {}

  /// The audio scored up to `end`, decoded when `due`. False once the case
  /// cannot go on.
  bool scored(std::uint64_t end, bool due) {
    if (!commitment_.admit(end)) {
      seen_.unstable_at = end;
      seen_.committed_when_unstable = commitment_.committed();
      return false;
    }
    if (!due) {
      return true;
    }
    const auto* result = decoded(end);
    if (result == nullptr) {
      return false;
    }
    if (!*result) {
      return true;
    }
    const auto before = commitment_.committed();
    const std::uint64_t ring = commitment_.ring_start();
    const auto step = commitment_.observe(ring, end, **result);
    EXPECT_TRUE(step) << step.error().message;
    if (!step) {
      return false;
    }
    seen_.unreconciled += step->reconciled ? 0 : 1;
    seen_.ring_advances += commitment_.ring_start() != ring ? 1 : 0;
    // Committed words are irreversible.
    EXPECT_GE(commitment_.committed().size(), before.size());
    EXPECT_TRUE(std::equal(before.begin(), before.end(), commitment_.committed().begin()));
    return !::testing::Test::HasFailure();
  }

  /// The tail decode at the utterance's end.
  bool ended(std::uint64_t end) {
    const auto* result = decoded(end);
    if (result == nullptr) {
      return false;
    }
    EXPECT_TRUE(*result) << "the tail's decode failed";
    if (!*result) {
      return false;
    }
    auto words = commitment_.finalize(commitment_.ring_start(), end, **result);
    EXPECT_TRUE(words) << words.error().message;
    if (!words) {
      return false;
    }
    seen_.finals.push_back(*std::move(words));
    seen_.utterances.emplace_back(first_, end);
    return true;
  }

 private:
  const std::optional<TranscriptResult>* decoded(std::uint64_t end) {
    const auto it = record_.decodes.find({commitment_.ring_start(), end});
    seen_.decodes_read.emplace(commitment_.ring_start(), end);
    if (it == record_.decodes.end()) {
      ADD_FAILURE() << "no recorded decode of [" << commitment_.ring_start() << ", " << end << ")";
      return nullptr;
    }
    return &it->second;
  }

  const Record& record_;
  Replay& seen_;
  TranscriptCommitment commitment_;
  std::uint64_t first_;
};

// The clip as one utterance from sample zero, ended at the clip's end.
void replay_whole_clip(const Record& record, Replay& seen) {
  Utterance utterance{record, seen, 0};
  for (std::uint64_t end = kAccumulation; end <= record.samples; end += kStride) {
    if (!utterance.scored(end, true)) {
      return;
    }
  }
  (void)utterance.ended(record.samples);
}

// As the speech dispatch decodes a session, one frame at a time; an
// utterance open at the clip's end is ended there, as by a Finalize.
void replay_detected(const Record& record, Replay& seen) {
  auto policy = VadPolicy::create(VadProfile{});
  ASSERT_TRUE(policy);
  WindowScheduler windows{policy->profile()};
  const std::vector<float>* scores = nullptr;
  std::uint64_t scores_from = 0;
  const auto reset_at = [&](std::uint64_t start) {
    const auto it = record.voice_activity.find(start);
    seen.voice_activity_read.insert(start);
    if (it == record.voice_activity.end()) {
      ADD_FAILURE() << "no recorded voice activity from " << start;
      return false;
    }
    scores = &it->second;
    scores_from = start;
    return true;
  };
  if (!reset_at(0)) {
    return;
  }
  std::optional<Utterance> open;
  for (std::uint64_t start = 0; start + kFrame <= record.samples; start += kFrame) {
    const std::uint64_t index = (start - scores_from) / kFrame;
    ASSERT_LT(index, scores->size());
    const auto outcome = policy->observe((*scores)[index]);
    ASSERT_TRUE(outcome) << outcome.error().message;
    ASSERT_FALSE(outcome->duration_split) << "no utterance here reaches 30 s";
    const std::uint64_t end = start + kFrame;
    if (outcome->onset) {
      open.emplace(record, seen, *outcome->onset);
      windows.open(*outcome->onset, *policy->speech_start());
    }
    if (outcome->silence_endpoint) {
      ASSERT_TRUE(open);
      windows.close();
      if (!open->ended(*outcome->silence_endpoint) || !reset_at(policy->next_frame_start())) {
        return;
      }
      open.reset();
      continue;
    }
    if (open) {
      const auto window = windows.due(end);
      if (window) {
        ASSERT_EQ(window->end, end);
        windows.started(end);
      }
      if (!open->scored(end, window.has_value())) {
        return;
      }
    }
  }
  if (open) {
    (void)open->ended(record.samples);
  }
}

// A record holds exactly what its replay read.
void expect_all_read(const Record& record, const Replay& seen) {
  EXPECT_EQ(seen.decodes_read.size(), record.decodes.size());
  EXPECT_EQ(seen.voice_activity_read.size(), record.voice_activity.size());
}

std::string joined(const std::vector<AlignedWord>& words) {
  std::string text;
  for (const AlignedWord& word : words) {
    text += word.text;
  }
  return text;
}

// A word as the expectations compare it: without the spaces and punctuation
// around it, in ASCII lower case.
std::string comparable(std::string_view text) {
  constexpr std::array<std::string_view, 4> kArabicMarks{"،", "؛", "؟", "۔"};
  const auto mark_at_front = [&] {
    if (!text.empty() && (std::isspace(static_cast<unsigned char>(text.front())) != 0 ||
                          std::ispunct(static_cast<unsigned char>(text.front())) != 0)) {
      return std::string_view::size_type{1};
    }
    for (const std::string_view mark : kArabicMarks) {
      if (text.starts_with(mark)) {
        return mark.size();
      }
    }
    return std::string_view::size_type{0};
  };
  const auto mark_at_back = [&] {
    if (!text.empty() && (std::isspace(static_cast<unsigned char>(text.back())) != 0 ||
                          std::ispunct(static_cast<unsigned char>(text.back())) != 0)) {
      return std::string_view::size_type{1};
    }
    for (const std::string_view mark : kArabicMarks) {
      if (text.ends_with(mark)) {
        return mark.size();
      }
    }
    return std::string_view::size_type{0};
  };
  for (auto size = mark_at_front(); size > 0; size = mark_at_front()) {
    text.remove_prefix(size);
  }
  for (auto size = mark_at_back(); size > 0; size = mark_at_back()) {
    text.remove_suffix(size);
  }
  std::string out{text};
  for (char& c : out) {
    if (c >= 'A' && c <= 'Z') {
      c = static_cast<char>(c - 'A' + 'a');
    }
  }
  return out;
}

std::vector<std::string> comparable_words(const std::vector<AlignedWord>& words) {
  std::vector<std::string> out;
  for (const AlignedWord& word : words) {
    out.push_back(comparable(word.text));
  }
  return out;
}

/// The reference decode's words by the utterance each overlaps most; a word
/// that overlaps none is outside them. A word may span a pause the voice
/// activity policy ends an utterance in.
struct Heard {
  std::vector<std::vector<std::string>> in_utterance;
  std::vector<std::string> outside;
};

Heard reference_heard(const Record& record, const Replay& seen) {
  Heard heard;
  heard.in_utterance.resize(seen.utterances.size());
  for (const WordUnit& word : record.reference.words) {
    std::uint64_t most = 0;
    std::vector<std::string>* in = &heard.outside;
    for (std::size_t i = 0; i < seen.utterances.size(); ++i) {
      const auto& [first, end] = seen.utterances[i];
      const std::uint64_t from = std::max(first, word.start_sample);
      const std::uint64_t to = std::min(end, word.end_sample);
      if (to > from && to - from > most) {
        most = to - from;
        in = &heard.in_utterance[i];
      }
    }
    in->push_back(comparable(word.text));
  }
  return heard;
}

TEST(TranscriptCommitmentRecord, EnglishUtterances) {
  const Record record = load("en_jfk_turbo_int8_cpu.json");
  ASSERT_EQ(record.utterances, "voice activity");
  Replay seen;
  replay_detected(record, seen);
  if (HasFailure()) {
    return;
  }
  expect_all_read(record, seen);
  EXPECT_EQ(seen.unreconciled, 0U);
  EXPECT_FALSE(seen.unstable_at);
  // Each utterance's final holds exactly the words the whole clip's decode
  // heard in it.
  const Heard heard = reference_heard(record, seen);
  ASSERT_EQ(seen.finals.size(), 4U);
  for (std::size_t i = 0; i < seen.finals.size(); ++i) {
    EXPECT_EQ(comparable_words(seen.finals[i]), heard.in_utterance[i]) << joined(seen.finals[i]);
  }
  // The voice activity model, reset for each utterance, never scores the soft
  // "ask" after the first pause as speech, so no utterance holds it.
  EXPECT_EQ(heard.outside, std::vector<std::string>{"ask"});
}

// Stopped past eight seconds at a committed word later decodes spell another
// way: the reference's leading words, the last over its audio, respelled.
void expect_stalled_on_a_respelling(const Record& record, const Replay& seen) {
  ASSERT_TRUE(seen.unstable_at);
  EXPECT_TRUE(seen.finals.empty());
  const auto& committed = seen.committed_when_unstable;
  ASSERT_FALSE(committed.empty());
  ASSERT_LE(committed.size(), record.reference.words.size());
  const std::size_t last = committed.size() - 1;
  for (std::size_t i = 0; i < last; ++i) {
    EXPECT_EQ(comparable(committed[i].text), comparable(record.reference.words[i].text)) << i;
  }
  const AlignedWord& stalled = committed[last];
  const WordUnit& heard = record.reference.words[last];
  const auto distance = [](std::uint64_t lhs, std::uint64_t rhs) {
    return lhs > rhs ? lhs - rhs : rhs - lhs;
  };
  EXPECT_NE(comparable(stalled.text), comparable(heard.text));
  EXPECT_LE(distance(stalled.start_sample, heard.start_sample), kCommitTolerance);
  EXPECT_LE(distance(stalled.end_sample, heard.end_sample), kCommitTolerance);
  EXPECT_GT(*seen.unstable_at - committed.front().start_sample, kTranscriptWindow);
}

TEST(TranscriptCommitmentRecord, ArabicUtteranceStallsOnARespelledWord) {
  const Record record = load("ar_fleurs_turbo_int8_cpu.json");
  ASSERT_EQ(record.utterances, "voice activity");
  Replay seen;
  replay_detected(record, seen);
  if (HasFailure()) {
    return;
  }
  expect_all_read(record, seen);
  EXPECT_GT(seen.ring_advances, 0U);
  expect_stalled_on_a_respelling(record, seen);
}

TEST(TranscriptCommitmentRecord, ArabicWholeClipStallsOnARespelledWord) {
  const Record record = load("ar_fleurs_whole_clip_turbo_int8_cpu.json");
  ASSERT_EQ(record.utterances, "whole clip");
  Replay seen;
  replay_whole_clip(record, seen);
  if (HasFailure()) {
    return;
  }
  expect_all_read(record, seen);
  expect_stalled_on_a_respelling(record, seen);
}

}  // namespace
}  // namespace tensorplate::serving
