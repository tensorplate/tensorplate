// SPDX-License-Identifier: Apache-2.0
// Synthetic by design: each case builds the decodes whose property it checks.
#include "serving/session/transcript_commitment.hpp"

#include <gtest/gtest.h>

#include <algorithm>
#include <cstdint>
#include <string>
#include <utility>
#include <vector>

namespace tensorplate::serving {
namespace {

constexpr std::uint64_t kStride = 2'560;
constexpr std::uint64_t kStart = 1'000'000;
const VadProfile kProfile{};

// 160 ms, 320 ms, 1 s and 8 s at 16 kHz; the cases below are built on them.
static_assert(kCommitTolerance == 2'560 && kCommitGuard == 5'120 && kRetainedOverlap == 16'000 &&
              kTranscriptWindow == 128'000);

struct Word {
  std::string text;
  std::uint64_t start;
  std::uint64_t end;
  /// Tokens default to one per word, numbered from its text.
  std::vector<std::uint32_t> tokens{};
};

std::vector<std::uint32_t> tokens_of(const Word& word) {
  if (!word.tokens.empty()) {
    return word.tokens;
  }
  std::uint32_t token = 0;
  for (const char c : word.text) {
    token = token * 31 + static_cast<unsigned char>(c);
  }
  return {token % 50'000};
}

TranscriptResult decode(const std::vector<Word>& words) {
  TranscriptResult result;
  for (const Word& word : words) {
    const auto tokens = tokens_of(word);
    WordUnit unit;
    unit.text = word.text;
    unit.token_begin = static_cast<std::uint32_t>(result.tokens.size());
    result.tokens.insert(result.tokens.end(), tokens.begin(), tokens.end());
    unit.token_end = static_cast<std::uint32_t>(result.tokens.size());
    unit.start_sample = word.start;
    unit.end_sample = word.end;
    result.text += word.text;
    result.words.push_back(std::move(unit));
  }
  return result;
}

std::vector<std::string> texts(const std::vector<AlignedWord>& words) {
  std::vector<std::string> out;
  out.reserve(words.size());
  for (const AlignedWord& word : words) {
    out.push_back(word.text);
  }
  return out;
}

using Texts = std::vector<std::string>;

CommitStep observe(TranscriptCommitment& commitment, std::uint64_t end,
                   const std::vector<Word>& words) {
  const auto step = commitment.observe(commitment.ring_start(), end, decode(words));
  EXPECT_TRUE(step) << step.error().message;
  return step ? *step : CommitStep{};
}

// Three words a second long from kStart, then a fourth.
const std::vector<Word> kThree{{" one", kStart, kStart + 16'000},
                               {" two", kStart + 16'000, kStart + 32'000},
                               {" three", kStart + 32'000, kStart + 48'000}};

TEST(TranscriptCommitment, PublishesTheFirstDecodeWithoutCommittingAnything) {
  TranscriptCommitment commitment{kProfile, kStart};
  EXPECT_EQ(observe(commitment, kStart + 60'000, kThree), (CommitStep{true, 0}));
  EXPECT_TRUE(commitment.committed().empty());
  EXPECT_EQ(texts(commitment.suffix()), (Texts{" one", " two", " three"}));
  EXPECT_EQ(commitment.ring_start(), kStart);
}

// Observes `decodes` at successive strides from `end`; the last one's step.
CommitStep observe_each(TranscriptCommitment& commitment, std::uint64_t end,
                        const std::vector<std::vector<Word>>& decodes) {
  CommitStep step;
  for (const auto& words : decodes) {
    step = observe(commitment, end, words);
    end += kStride;
  }
  return step;
}

TEST(TranscriptCommitment, CommitsTheWordsThreeDecodesAgreeOnBeforeTheGuard) {
  TranscriptCommitment commitment{kProfile, kStart};
  (void)observe(commitment, kStart + 53'120, kThree);
  EXPECT_EQ(observe(commitment, kStart + 55'680, kThree), (CommitStep{true, 0}));
  // " three" ends exactly kCommitGuard before the oldest decode's end.
  EXPECT_EQ(observe(commitment, kStart + 58'240, kThree), (CommitStep{true, 3}));
  EXPECT_EQ(texts(commitment.committed()), (Texts{" one", " two", " three"}));
  EXPECT_TRUE(commitment.suffix().empty());
}

TEST(TranscriptCommitment, CommitsOnceTheProfilesCountOfDecodesAgree) {
  for (const std::uint32_t agreement : {2U, 3U, 4U}) {
    VadProfile profile;
    profile.commit_agreement = agreement;
    TranscriptCommitment commitment{profile, kStart};
    std::uint64_t end = kStart + 60'000;
    for (std::uint32_t decode = 1; decode < agreement; ++decode, end += kStride) {
      EXPECT_EQ(observe(commitment, end, kThree), (CommitStep{true, 0})) << agreement;
    }
    EXPECT_EQ(observe(commitment, end, kThree), (CommitStep{true, 3})) << agreement;
  }
}

TEST(TranscriptCommitment, LeavesAWordWithinTheGuardOfAnyDecodeUncommitted) {
  // The oldest decode ends at 53,119: a word ending after 47,999 there is
  // within its guard, whichever decode placed it so.
  for (const auto& ends : std::vector<std::vector<std::uint64_t>>{{48'000, 48'000, 48'000},
                                                                  {48'000, 46'000, 46'000},
                                                                  {46'000, 48'000, 46'000},
                                                                  {46'000, 46'000, 48'000}}) {
    std::vector<std::vector<Word>> decodes;
    for (const std::uint64_t end : ends) {
      decodes.push_back(kThree);
      decodes.back()[2].end = kStart + end;
    }
    TranscriptCommitment commitment{kProfile, kStart};
    (void)observe(commitment, kStart + 53'119, decodes[0]);
    decodes.erase(decodes.begin());
    EXPECT_EQ(observe_each(commitment, kStart + 60'000, decodes), (CommitStep{true, 2}))
        << ends[0] << ", " << ends[1] << ", " << ends[2];
    EXPECT_EQ(texts(commitment.suffix()), (Texts{" three"}));
  }
}

TEST(TranscriptCommitment, CommitsOnlyThePrefixEveryDecodeAgreesOn) {
  auto changed = kThree;
  changed[1].text = " too";
  for (std::size_t at = 0; at < 3; ++at) {
    std::vector<std::vector<Word>> decodes{kThree, kThree, kThree};
    decodes[at] = changed;
    TranscriptCommitment commitment{kProfile, kStart};
    EXPECT_EQ(observe_each(commitment, kStart + 80'000, decodes), (CommitStep{true, 1})) << at;
    EXPECT_EQ(texts(commitment.committed()), (Texts{" one"})) << at;
  }
}

TEST(TranscriptCommitment, ComparesTokensAndTextWithoutItsMarksOrCase) {
  auto retokenized = kThree;
  retokenized[0].tokens = {7, 8};
  auto respelled = kThree;
  respelled[0].text = " won";
  respelled[0].tokens = tokens_of(kThree[0]);
  auto recased = kThree;
  recased[0].text = " One,";
  recased[0].tokens = tokens_of(kThree[0]);
  for (const auto& [changed, expected] : std::vector<std::pair<std::vector<Word>, std::size_t>>{
           {retokenized, 0}, {respelled, 0}, {recased, 3}}) {
    TranscriptCommitment commitment{kProfile, kStart};
    EXPECT_EQ(observe_each(commitment, kStart + 80'000, {kThree, changed, kThree}).newly_committed,
              expected)
        << changed[0].text;
  }
}

TEST(TranscriptCommitment, CommitsAWordWhereTheLatestDecodePlacedIt) {
  auto moved = kThree;
  moved[1].start += 100;
  moved[1].end -= 100;
  TranscriptCommitment commitment{kProfile, kStart};
  ASSERT_EQ(observe_each(commitment, kStart + 80'000, {kThree, kThree, moved}).newly_committed, 3U);
  EXPECT_EQ(commitment.committed()[1].start_sample, kStart + 16'100);
  EXPECT_EQ(commitment.committed()[1].end_sample, kStart + 31'900);
}

TEST(TranscriptCommitment, ToleratesDriftOf160MillisecondsAcrossAllTheDecodes) {
  // The second word's edge moves by these offsets in the three decodes: it
  // commits only when every two of them are within the tolerance.
  constexpr std::uint64_t kTol = kCommitTolerance;
  for (const bool at_start : {true, false}) {
    for (const auto& offsets : std::vector<std::vector<std::uint64_t>>{{0, kTol, 0},
                                                                       {0, kTol + 1, 0},
                                                                       {kTol + 1, 0, 0},
                                                                       {0, 0, kTol + 1},
                                                                       {0, 2 * kTol, kTol},
                                                                       {kTol, 0, 2 * kTol}}) {
      std::vector<std::vector<Word>> decodes;
      for (const std::uint64_t offset : offsets) {
        decodes.push_back(kThree);
        (at_start ? decodes.back()[1].start : decodes.back()[1].end) += offset;
      }
      const std::size_t expected = offsets == std::vector<std::uint64_t>{0, kTol, 0} ? 3 : 1;
      TranscriptCommitment commitment{kProfile, kStart};
      EXPECT_EQ(observe_each(commitment, kStart + 80'000, decodes).newly_committed, expected)
          << "at_start=" << at_start << " offsets " << offsets[0] << ", " << offsets[1] << ", "
          << offsets[2];
    }
  }
}

TEST(TranscriptCommitment, ComparesWithTheLastReconciledDecodesAfterOneIsMissing) {
  // A decode whose response never came is simply not observed.
  TranscriptCommitment commitment{kProfile, kStart};
  (void)observe(commitment, kStart + 60'000, kThree);
  (void)observe(commitment, kStart + 60'000 + 3 * kStride, kThree);
  EXPECT_EQ(observe(commitment, kStart + 60'000 + 5 * kStride, kThree), (CommitStep{true, 3}));
}

TEST(TranscriptCommitment, KeepsComparingTheWordsLeftAfterACommit) {
  // The oldest decode ends 3,000 samples after " w1": only " w0" is past its
  // guard. The next decode commits " w1" from the words each kept.
  const std::vector<Word> words{{" w0", kStart, kStart + 16'000},
                                {" w1", kStart + 16'000, kStart + 32'000}};
  TranscriptCommitment commitment{kProfile, kStart};
  EXPECT_EQ(observe_each(commitment, kStart + 35'000, {words, words, words}),
            (CommitStep{true, 1}));
  EXPECT_EQ(observe(commitment, kStart + 35'000 + 3 * kStride, words), (CommitStep{true, 1}));
  EXPECT_EQ(texts(commitment.committed()), (Texts{" w0", " w1"}));
}

// Words a second long from kStart: " w0", " w1", ...
std::vector<Word> spoken(std::size_t count, std::size_t first = 0) {
  std::vector<Word> words;
  for (std::size_t i = first; i < first + count; ++i) {
    words.push_back({" w" + std::to_string(i), kStart + 16'000 * i, kStart + 16'000 * (i + 1)});
  }
  return words;
}

// The end of the last decode committed_through(count) observes.
std::uint64_t through_end(std::size_t count) {
  return kStart + 16'000 * count + kCommitGuard + 2 * kStride;
}

// Commits the first `count` words of spoken(), whose last ends at kStart + count seconds.
TranscriptCommitment committed_through(std::size_t count) {
  TranscriptCommitment commitment{kProfile, kStart};
  const std::uint64_t end = kStart + 16'000 * count + kCommitGuard;
  EXPECT_EQ(observe_each(commitment, end, {spoken(count), spoken(count), spoken(count)}),
            (CommitStep{true, count}));
  return commitment;
}

TEST(TranscriptCommitment, StartsTheRingAtTheLatestWordEndASecondBeforeTheCommittedEnd) {
  auto commitment = committed_through(4);
  // Committed to 4 s: the latest word end at or before 3 s is " w2"'s.
  EXPECT_EQ(commitment.ring_start(), kStart + 48'000);
  auto short_one = committed_through(1);
  EXPECT_EQ(short_one.ring_start(), kStart);
}

TEST(TranscriptCommitment, TakesOnlyTheWindowTheRingHolds) {
  TranscriptCommitment fresh{kProfile, kStart};
  const auto empty = fresh.observe(kStart, kStart, decode({}));
  ASSERT_FALSE(empty);
  EXPECT_EQ(empty.error().context, "commitment_window_mismatch");
  const auto empty_tail = fresh.finalize(kStart, kStart, decode({}));
  ASSERT_FALSE(empty_tail);
  EXPECT_EQ(empty_tail.error().context, "commitment_window_mismatch");

  auto commitment = committed_through(4);
  const auto refused = commitment.observe(kStart, kStart + 100'000, decode(spoken(5)));
  ASSERT_FALSE(refused);
  EXPECT_EQ(refused.error().context, "commitment_window_mismatch");
  const auto same_end =
      commitment.observe(commitment.ring_start(), through_end(4), decode(spoken(2, 3)));
  ASSERT_FALSE(same_end);
  EXPECT_EQ(same_end.error().context, "commitment_window_mismatch");
}

TEST(TranscriptCommitment, ReconcilesTheCommittedWordsTheWindowRepeats) {
  auto commitment = committed_through(4);
  const std::uint64_t end = kStart + 101'120;
  // The window from 3 s repeats " w3" and adds " w4" and " w5".
  EXPECT_EQ(observe(commitment, end, spoken(3, 3)), (CommitStep{true, 0}));
  EXPECT_EQ(texts(commitment.suffix()), (Texts{" w4", " w5"}));
  EXPECT_EQ(observe_each(commitment, end + kStride, {spoken(3, 3), spoken(3, 3)}),
            (CommitStep{true, 2}));
  EXPECT_EQ(commitment.committed().size(), 6U);
}

TEST(TranscriptCommitment, ReadsAnAnchorWhateverItsMarksCaseAndStart) {
  auto commitment = committed_through(4);
  auto again = spoken(3, 3);
  again[0].text = " W3,";  // a window that starts a sentence capitalizes its first word
  again[0].tokens = {9, 10};
  again[0].start += 12'000;  // a start after a pause moves with the window's start
  again[0].end += kCommitTolerance;
  EXPECT_TRUE(observe(commitment, kStart + 100'000, again).reconciled);
  EXPECT_EQ(texts(commitment.suffix()), (Texts{" w4", " w5"}));
  // The committed word keeps the form it was committed in.
  ASSERT_EQ(commitment.committed().size(), 4U);
  EXPECT_EQ(commitment.committed()[3].text, " w3");
}

TEST(TranscriptCommitment, ReadsAnArabicAnchorWithoutItsTatweelOrShortVowels) {
  // Committed with a tanween (U+064B); decoded again with a tatweel (U+0640),
  // a sukun (U+0652) and an Arabic comma and without the tanween; and as
  // another word.
  const std::string committed = " \u062a\u0645\u0627\u0645\u0627\u064b";
  const std::vector<Word> words{{" w0", kStart, kStart + 16'000},
                                {" w1", kStart + 16'000, kStart + 32'000},
                                {committed, kStart + 32'000, kStart + 48'000}};
  for (const auto& [again, reconciled] : std::vector<std::pair<std::string, bool>>{
           {" \u062a\u0640\u0645\u0652\u0627\u0645\u0627\u060c", true},
           {" \u062a\u0645\u0627\u0645\u064a", false}}) {
    TranscriptCommitment commitment{kProfile, kStart};
    ASSERT_EQ(observe_each(commitment, kStart + 53'120, {words, words, words}).newly_committed, 3U);
    ASSERT_EQ(commitment.ring_start(), kStart + 32'000);
    const std::vector<Word> window{{again, kStart + 32'000, kStart + 48'000},
                                   {" w3", kStart + 48'000, kStart + 64'000}};
    EXPECT_EQ(observe(commitment, kStart + 70'000, window).reconciled, reconciled);
  }
}

TEST(TranscriptCommitment, LeavesADecodeThatDoesNotRepeatTheCommittedWordsUnreconciled) {
  for (const auto& change : std::vector<std::pair<std::string, Word>>{
           {"another word", {" w9", kStart + 48'000, kStart + 64'000}},
           {"an end past the tolerance",
            {" w3", kStart + 48'000, kStart + 64'000 + kCommitTolerance + 1}},
           {"an end before the tolerance",
            {" w3", kStart + 48'000, kStart + 64'000 - kCommitTolerance - 1}}}) {
    auto commitment = committed_through(4);
    (void)observe(commitment, kStart + 101'120, spoken(3, 3));
    auto words = spoken(3, 3);
    words[0] = change.second;
    EXPECT_EQ(observe(commitment, kStart + 103'680, words), CommitStep{}) << change.first;
    // Its window still counts as observed.
    EXPECT_FALSE(
        commitment.observe(commitment.ring_start(), kStart + 103'680, decode(spoken(3, 3))))
        << change.first;
    EXPECT_EQ(texts(commitment.suffix()), (Texts{" w4", " w5"})) << change.first;
    // It did not count: the next two complete the agreement.
    EXPECT_EQ(observe(commitment, kStart + 106'240, spoken(3, 3)).newly_committed, 0U)
        << change.first;
    EXPECT_EQ(observe(commitment, kStart + 108'800, spoken(3, 3)).newly_committed, 2U)
        << change.first;
  }
  auto commitment = committed_through(4);
  EXPECT_EQ(observe(commitment, kStart + 100'000, {}), CommitStep{});
}

TEST(TranscriptCommitment, SkipsTheWordCutAtTheRingStartHeardAgain) {
  auto commitment = committed_through(4);
  auto words = spoken(3, 3);
  // " w2" ended at the ring start; the window hears its last 80 ms.
  words.insert(words.begin(), Word{" w2", kStart + 48'000, kStart + 48'000 + 1'280});
  EXPECT_TRUE(observe(commitment, kStart + 100'000, words).reconciled);
  EXPECT_EQ(texts(commitment.suffix()), (Texts{" w4", " w5"}));

  // Past the tolerance, or another word, it is a word of its own, which the
  // committed words do not hold.
  for (const auto& word : {Word{" w2", kStart + 48'000, kStart + 48'000 + kCommitTolerance + 1},
                           Word{" w9", kStart + 48'000, kStart + 48'000 + 1'280}}) {
    auto later = committed_through(4);
    words[0] = word;
    EXPECT_FALSE(observe(later, kStart + 100'000, words).reconciled) << word.text;
  }
}

TEST(TranscriptCommitment, LeavesARepeatedWordAtTheRingStartProvisional) {
  // " you you": the cut word and the first committed word after the ring are
  // the same word, so the window's first " you" could be either.
  TranscriptCommitment commitment{kProfile, kStart};
  const std::vector<Word> words{{" you", kStart, kStart + 16'000},
                                {" you", kStart + 16'000, kStart + 18'000},
                                {" can", kStart + 18'000, kStart + 33'000}};
  ASSERT_EQ(observe_each(commitment, kStart + 38'120, {words, words, words}).newly_committed, 3U);
  ASSERT_EQ(commitment.ring_start(), kStart + 16'000);
  const std::vector<Word> again{{" you", kStart + 16'000, kStart + 16'800},
                                {" you", kStart + 16'800, kStart + 18'000},
                                {" can", kStart + 18'000, kStart + 33'000},
                                {" do", kStart + 33'000, kStart + 40'000}};
  EXPECT_FALSE(observe(commitment, kStart + 50'000, again).reconciled);

  // When the first committed word after the ring ends far from it, a word
  // with its text ending at the ring start is the cut word heard again.
  TranscriptCommitment longer{kProfile, kStart};
  const std::vector<Word> long_you{{" you", kStart, kStart + 16'000},
                                   {" you", kStart + 16'000, kStart + 40'000},
                                   {" can", kStart + 40'000, kStart + 48'000}};
  ASSERT_EQ(observe_each(longer, kStart + 53'120, {long_you, long_you, long_you}).newly_committed,
            3U);
  ASSERT_EQ(longer.ring_start(), kStart + 16'000);
  const std::vector<Word> heard{{" you", kStart + 16'000, kStart + 16'800},
                                {" you", kStart + 16'800, kStart + 40'000},
                                {" can", kStart + 40'000, kStart + 48'000},
                                {" do", kStart + 48'000, kStart + 56'000}};
  EXPECT_TRUE(observe(longer, kStart + 60'000, heard).reconciled);
  EXPECT_EQ(texts(longer.suffix()), (Texts{" do"}));
}

TEST(TranscriptCommitment, MatchesRepeatedWordsOneToOne) {
  // A repeated word after the committed ones is a new word, never the committed one again.
  auto commitment = committed_through(4);
  auto words = spoken(3, 3);
  words.insert(words.begin() + 1, Word{" w3", kStart + 64'000, kStart + 70'000});
  EXPECT_TRUE(observe(commitment, kStart + 100'000, words).reconciled);
  EXPECT_EQ(texts(commitment.suffix()), (Texts{" w3", " w4", " w5"}));
}

TEST(TranscriptCommitment, RefusesWordsOutsideTheWindowOrOutOfOrder) {
  const auto refuse = [](TranscriptResult result, const char* what) {
    auto commitment = committed_through(4);
    const auto before = commitment.committed();
    const auto step = commitment.observe(commitment.ring_start(), kStart + 100'000, result);
    ASSERT_FALSE(step) << what;
    EXPECT_EQ(step.error().code, Error::Code::Internal) << what;
    EXPECT_EQ(step.error().context, "invalid_transcript_snapshot") << what;
    EXPECT_EQ(commitment.committed(), before) << what;
    // The refused decode did not count as observed.
    EXPECT_TRUE(commitment.observe(commitment.ring_start(), kStart + 100'000, decode(spoken(3, 3))))
        << what;
  };
  auto early = decode(spoken(3, 3));
  early.words[0].start_sample = kStart + 47'999;
  refuse(early, "a start before the window");
  auto late = decode(spoken(3, 3));
  late.words[2].end_sample = kStart + 100'001;
  refuse(late, "an end after the window");
  auto backwards = decode(spoken(3, 3));
  backwards.words[2].start_sample = backwards.words[1].start_sample - 1;
  refuse(backwards, "a start before the previous word's");
  auto inverted = decode(spoken(3, 3));
  inverted.words[1].end_sample = inverted.words[1].start_sample - 1;
  refuse(inverted, "an end before its start");
  auto no_tokens = decode(spoken(3, 3));
  no_tokens.words[1].token_end = no_tokens.words[1].token_begin;
  refuse(no_tokens, "no tokens");
  auto past_tokens = decode(spoken(3, 3));
  past_tokens.words[2].token_end = 4;
  refuse(past_tokens, "tokens past the result's");
  auto shared_tokens = decode(spoken(3, 3));
  shared_tokens.words[1].token_begin = 0;
  refuse(shared_tokens, "tokens shared with the previous word");
}

TEST(TranscriptCommitment, AdmitsAudioUpToEightSecondsPastTheRingStart) {
  auto commitment = committed_through(4);
  EXPECT_TRUE(commitment.admit(commitment.ring_start() + kTranscriptWindow));
  const auto refused = commitment.admit(commitment.ring_start() + kTranscriptWindow + 1);
  ASSERT_FALSE(refused);
  EXPECT_EQ(refused.error().code, Error::Code::ResourceExhausted);
  EXPECT_EQ(refused.error().context, "unstable_transcript_window");
}

TEST(TranscriptCommitment, FailsClosedWhenTheAlignmentNeverSettles) {
  // Every decode moves each word by more than the tolerance: nothing commits,
  // the ring stays, and the window's audio runs out after eight seconds.
  TranscriptCommitment commitment{kProfile, kStart};
  std::uint64_t end = kStart + 5'120;
  for (std::uint64_t step = 0; commitment.admit(end); ++step, end += kStride) {
    std::vector<Word> words;
    const std::uint64_t shift = (step % 2) * (kCommitTolerance + 1);
    for (std::uint64_t at = kStart + shift; at + 8'000 <= end; at += 8'000) {
      words.push_back({" word", at, std::min(at + 8'000, end)});
    }
    EXPECT_EQ(observe(commitment, end, words).newly_committed, 0U);
  }
  EXPECT_GT(end - commitment.ring_start(), kTranscriptWindow);
  EXPECT_LE(end - commitment.ring_start(), kTranscriptWindow + kStride);
  EXPECT_TRUE(commitment.committed().empty());
}

TEST(TranscriptCommitment, FinishesWithTheCommittedWordsAndTheTailsNewWords) {
  auto commitment = committed_through(4);
  auto tail = spoken(4, 3);
  tail[3].text = " w6.";
  const auto final_words =
      commitment.finalize(commitment.ring_start(), kStart + 7 * 16'000, decode(tail));
  ASSERT_TRUE(final_words) << final_words.error().message;
  EXPECT_EQ(texts(*final_words), (Texts{" w0", " w1", " w2", " w3", " w4", " w5", " w6."}));
  // Finishing changes nothing; a final with nothing committed is the tail.
  EXPECT_EQ(commitment.committed().size(), 4U);
  TranscriptCommitment fresh{kProfile, kStart};
  const auto whole = fresh.finalize(kStart, kStart + 40'000, decode(spoken(2)));
  ASSERT_TRUE(whole);
  EXPECT_EQ(texts(*whole), (Texts{" w0", " w1"}));
}

TEST(TranscriptCommitment, FailsAFinalThatDoesNotRepeatTheCommittedWords) {
  auto commitment = committed_through(4);
  for (const auto& tail : {spoken(3, 4), std::vector<Word>{}}) {
    const auto final_words =
        commitment.finalize(commitment.ring_start(), kStart + 7 * 16'000, decode(tail));
    ASSERT_FALSE(final_words);
    EXPECT_EQ(final_words.error().code, Error::Code::InferenceFailed);
    EXPECT_EQ(final_words.error().context, "ambiguous_final_overlap");
  }
}

TEST(TranscriptCommitment, ATailMayEndWhereTheLastDecodeEnded) {
  auto commitment = committed_through(4);
  const std::uint64_t last = through_end(4);
  EXPECT_TRUE(commitment.finalize(commitment.ring_start(), last, decode(spoken(1, 3))));
  const auto earlier = commitment.finalize(commitment.ring_start(), last - 1, decode(spoken(1, 3)));
  ASSERT_FALSE(earlier);
  EXPECT_EQ(earlier.error().context, "commitment_window_mismatch");
}

TEST(TranscriptCommitment, ResetStartsAnUtteranceWithNothingCommitted) {
  auto commitment = committed_through(4);
  (void)observe_each(commitment, kStart + 101'120, {spoken(3, 3), spoken(3, 3)});
  commitment.reset(kStart + 64'000);
  EXPECT_TRUE(commitment.committed().empty());
  EXPECT_TRUE(commitment.suffix().empty());
  EXPECT_EQ(commitment.ring_start(), kStart + 64'000);
  // Nothing observed before the reset counts, neither its words nor its end.
  EXPECT_EQ(observe(commitment, kStart + 101'120, spoken(2, 4)), (CommitStep{true, 0}));
}

TEST(TranscriptCommitment, AdvancesPastEightSecondsOfOneGrowingSegment) {
  // A 30-second utterance of 120 words a quarter second long, decoded every
  // stride over the ring's window. Each decode is one growing segment: its
  // words from the window's start to its end, the last one cut short and
  // misheard, every word moved by up to 100 ms from one decode to the next.
  constexpr std::size_t kWords = 120;
  constexpr std::uint64_t kWord = 4'000;
  constexpr std::uint64_t kEnd = kStart + kWords * kWord;
  const auto decode_window = [](std::uint64_t window_start, std::uint64_t end, bool settled) {
    std::vector<Word> words;
    for (std::size_t i = 0; i < kWords; ++i) {
      const std::uint64_t jitter = settled ? 0 : ((i * 7 + end / kStride) % 5) * 400;
      const std::uint64_t start = kStart + i * kWord + jitter;
      // A word that began well before the window is not heard in it.
      if (kStart + (i + 1) * kWord <= window_start + kWord / 2) {
        continue;
      }
      if (start >= end) {
        break;
      }
      const std::uint64_t word_end = std::min(start + kWord, end);
      const std::string text = " w" + std::to_string(i);
      words.push_back({word_end == end && !settled ? text + "-" : text,
                       std::max(start, window_start), word_end});
    }
    return words;
  };
  TranscriptCommitment commitment{kProfile, kStart};
  for (std::uint64_t end = kStart + 5'120; end <= kEnd; end += kStride) {
    ASSERT_TRUE(commitment.admit(end)) << end;
    const auto step = commitment.observe(
        commitment.ring_start(), end, decode(decode_window(commitment.ring_start(), end, false)));
    ASSERT_TRUE(step) << step.error().message;
    ASSERT_TRUE(step->reconciled) << end;
  }
  EXPECT_GT(commitment.ring_start(), kStart + kTranscriptWindow);
  const auto final_words = commitment.finalize(
      commitment.ring_start(), kEnd, decode(decode_window(commitment.ring_start(), kEnd, true)));
  ASSERT_TRUE(final_words) << final_words.error().message;
  ASSERT_EQ(final_words->size(), kWords);
  for (std::size_t i = 0; i < kWords; ++i) {
    EXPECT_EQ((*final_words)[i].text, " w" + std::to_string(i));
  }
}

}  // namespace
}  // namespace tensorplate::serving
