// SPDX-License-Identifier: Apache-2.0
#include "serving/session/transcript_commitment.hpp"

#include <algorithm>
#include <array>
#include <iterator>
#include <string>
#include <string_view>
#include <utility>

namespace tensorplate::serving {
namespace {

Error caller_defect(std::string message, std::string context) {
  return Error::make(Error::Code::Internal, std::move(message), std::move(context));
}

std::uint64_t distance(std::uint64_t lhs, std::uint64_t rhs) noexcept {
  return lhs > rhs ? lhs - rhs : rhs - lhs;
}

// Non-ASCII marks a decoder attaches to a word or keeps inside one, in UTF-8:
// the quotes, CJK and Spanish marks faster-whisper attaches, the ellipsis, the
// Arabic comma, semicolon, question mark and full stop, and the no-break space.
constexpr std::array<std::string_view, 15> kMarks{"\u201c", "\u201d", "\u00bf", "\u3002", "\uff0c",
                                                  "\uff01", "\uff1f", "\uff1a", "\u3001", "\u2026",
                                                  "\u060c", "\u061b", "\u061f", "\u06d4", "\u00a0"};

bool ascii_space_or_mark(char c) noexcept {
  return c == ' ' || (c >= '\t' && c <= '\r') || (c >= '!' && c <= '/') || (c >= ':' && c <= '@') ||
         (c >= '[' && c <= '`') || (c >= '{' && c <= '~');
}

// The word without the spaces and marks around it.
std::string_view body(std::string_view text) noexcept {
  for (bool trimmed = true; trimmed && !text.empty();) {
    trimmed = false;
    if (ascii_space_or_mark(text.front())) {
      text.remove_prefix(1);
      trimmed = true;
    } else if (ascii_space_or_mark(text.back())) {
      text.remove_suffix(1);
      trimmed = true;
    }
    for (const std::string_view mark : kMarks) {
      if (text.starts_with(mark)) {
        text.remove_prefix(mark.size());
        trimmed = true;
      } else if (text.ends_with(mark)) {
        text.remove_suffix(mark.size());
        trimmed = true;
      }
    }
  }
  return text;
}

// The word as anchors compare it: its body in ASCII lower case, without
// Arabic tatweel (U+0640) or short-vowel marks (U+064B to U+0652), which the
// decoder writes in one decode and leaves out in the next.
std::string normalized(std::string_view text) {
  text = body(text);
  std::string out;
  out.reserve(text.size());
  for (std::size_t i = 0; i < text.size(); ++i) {
    const auto byte = static_cast<unsigned char>(text[i]);
    if (byte == 0xD9 && i + 1 < text.size()) {
      const auto next = static_cast<unsigned char>(text[i + 1]);
      if (next == 0x80 || (next >= 0x8B && next <= 0x92)) {
        ++i;
        continue;
      }
    }
    out.push_back(byte >= 'A' && byte <= 'Z' ? static_cast<char>(byte - 'A' + 'a') : text[i]);
  }
  return out;
}

// The same word whatever marks the decoder attached to it and however it
// cased it: a window that starts a sentence capitalizes its first word.
bool same_body(std::string_view lhs, std::string_view rhs) {
  return normalized(lhs) == normalized(rhs);
}

// `word` is `committed` decoded again, ending within the tolerance. Its start
// is not compared: after a pause it moves with the window's start.
bool repeats(const AlignedWord& committed, const AlignedWord& word) {
  return same_body(committed.text, word.text) &&
         distance(committed.end_sample, word.end_sample) <= kCommitTolerance;
}

}  // namespace

TranscriptCommitment::TranscriptCommitment(const VadProfile& profile, std::uint64_t utterance_start)
    : agreement_(profile.commit_agreement), ring_start_(utterance_start), history_(1) {}

void TranscriptCommitment::reset(std::uint64_t utterance_start) {
  committed_.clear();
  ring_index_ = 0;
  ring_start_ = utterance_start;
  history_.assign(1, Snapshot{});
  observed_end_ = 0;
}

Result<std::vector<AlignedWord>> TranscriptCommitment::words_of(
    std::uint64_t window_start, std::uint64_t window_end, std::uint64_t min_end,
    const TranscriptResult& result) const {
  if (window_start != ring_start_ || window_end <= window_start || window_end < min_end) {
    return unexpected(caller_defect("a decode's window is not the one the ring holds",
                                    "commitment_window_mismatch"));
  }
  std::vector<AlignedWord> words;
  words.reserve(result.words.size());
  std::uint64_t previous_start = window_start;
  std::uint32_t previous_token = 0;
  for (const WordUnit& word : result.words) {
    if (word.start_sample < previous_start || word.end_sample < word.start_sample ||
        word.end_sample > window_end || word.token_begin < previous_token ||
        word.token_end <= word.token_begin || word.token_end > result.tokens.size()) {
      return unexpected(caller_defect("a decode's words do not lie in order inside its window",
                                      "invalid_transcript_snapshot"));
    }
    previous_start = word.start_sample;
    previous_token = word.token_end;
    words.push_back(AlignedWord{
        word.text,
        {result.tokens.begin() + word.token_begin, result.tokens.begin() + word.token_end},
        word.start_sample,
        word.end_sample});
  }
  return words;
}

std::optional<std::size_t> TranscriptCommitment::anchored(
    const std::vector<AlignedWord>& words) const {
  const auto first_anchor = committed_.begin() + static_cast<std::ptrdiff_t>(ring_index_);
  std::size_t at = 0;
  // The word that ends at the ring start, heard again at the window's start.
  if (ring_index_ > 0 && !words.empty() &&
      words.front().end_sample <= ring_start_ + kCommitTolerance &&
      same_body(words.front().text, committed_[ring_index_ - 1].text)) {
    // advance_ring() always leaves a committed word after the ring start.
    if (repeats(*first_anchor, words.front())) {
      return std::nullopt;
    }
    at = 1;
  }
  for (auto anchor = first_anchor; anchor != committed_.end(); ++anchor, ++at) {
    if (at == words.size() || !repeats(*anchor, words[at])) {
      return std::nullopt;
    }
  }
  return at;
}

Result<CommitStep> TranscriptCommitment::observe(std::uint64_t window_start,
                                                 std::uint64_t window_end,
                                                 const TranscriptResult& result) {
  auto words = words_of(window_start, window_end, observed_end_ + 1, result);
  if (!words) {
    return unexpected(std::move(words.error()));
  }
  observed_end_ = window_end;
  const auto anchors = anchored(*words);
  if (!anchors) {
    return CommitStep{};
  }
  words->erase(words->begin(), words->begin() + static_cast<std::ptrdiff_t>(*anchors));
  std::size_t stable = 0;
  while (stable < words->size() && settled(stable, (*words)[stable])) {
    ++stable;
  }
  const auto kept = words->begin() + static_cast<std::ptrdiff_t>(stable);
  committed_.insert(committed_.end(), std::make_move_iterator(words->begin()),
                    std::make_move_iterator(kept));
  words->erase(words->begin(), kept);
  for (Snapshot& snapshot : history_) {
    snapshot.words.erase(snapshot.words.begin(),
                         snapshot.words.begin() + static_cast<std::ptrdiff_t>(stable));
  }
  if (history_.size() + 1 == agreement_) {
    history_.erase(history_.begin());
  }
  history_.push_back(Snapshot{*std::move(words), window_end});
  advance_ring();
  return CommitStep{true, stable};
}

bool TranscriptCommitment::settled(std::size_t index, const AlignedWord& word) const {
  std::uint64_t first_start = word.start_sample;
  std::uint64_t last_start = word.start_sample;
  std::uint64_t first_end = word.end_sample;
  std::uint64_t last_end = word.end_sample;
  for (const Snapshot& snapshot : history_) {
    if (index >= snapshot.words.size()) {
      return false;
    }
    const AlignedWord& earlier = snapshot.words[index];
    if (!same_body(earlier.text, word.text) || earlier.tokens != word.tokens) {
      return false;
    }
    first_start = std::min(first_start, earlier.start_sample);
    last_start = std::max(last_start, earlier.start_sample);
    first_end = std::min(first_end, earlier.end_sample);
    last_end = std::max(last_end, earlier.end_sample);
  }
  // Every decode holds audio up to the oldest one's end.
  return last_start - first_start <= kCommitTolerance && last_end - first_end <= kCommitTolerance &&
         last_end + kCommitGuard <= history_.front().end;
}

void TranscriptCommitment::advance_ring() noexcept {
  if (committed_.empty() || committed_.back().end_sample < ring_start_ + kRetainedOverlap) {
    return;
  }
  const std::uint64_t latest = committed_.back().end_sample - kRetainedOverlap;
  std::size_t index = ring_index_;
  while (index < committed_.size() && committed_[index].end_sample <= latest) {
    ++index;
  }
  if (index > ring_index_) {
    ring_index_ = index;
    ring_start_ = committed_[index - 1].end_sample;
  }
}

Result<void> TranscriptCommitment::admit(std::uint64_t end) const {
  if (end > ring_start_ && end - ring_start_ > kTranscriptWindow) {
    return unexpected(Error::make(Error::Code::ResourceExhausted,
                                  "the transcript did not stabilize within its window",
                                  "unstable_transcript_window"));
  }
  return {};
}

Result<std::vector<AlignedWord>> TranscriptCommitment::finalize(
    std::uint64_t window_start, std::uint64_t window_end, const TranscriptResult& tail) const {
  auto words = words_of(window_start, window_end, observed_end_, tail);
  if (!words) {
    return unexpected(std::move(words.error()));
  }
  const auto anchors = anchored(*words);
  if (!anchors) {
    return unexpected(Error::make(Error::Code::InferenceFailed,
                                  "the final decode does not repeat the committed words",
                                  "ambiguous_final_overlap"));
  }
  std::vector<AlignedWord> final_words = committed_;
  final_words.insert(
      final_words.end(),
      std::make_move_iterator(words->begin() + static_cast<std::ptrdiff_t>(*anchors)),
      std::make_move_iterator(words->end()));
  return final_words;
}

}  // namespace tensorplate::serving
