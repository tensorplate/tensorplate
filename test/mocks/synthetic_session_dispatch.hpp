// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <array>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstddef>
#include <cstdint>
#include <deque>
#include <map>
#include <memory>
#include <mutex>
#include <optional>
#include <span>
#include <string>
#include <string_view>
#include <thread>
#include <variant>
#include <vector>

#include "tensorplate/scheduler/clock.hpp"

#include "serving/session/dispatch.hpp"

namespace tensorplate::testing {

/// A dispatch with no backend, for exercising a transport binding. It
/// follows SessionDispatch on one thread and yields a function of its input:
///
/// - Audio (16 kHz mono 16-bit PCM, frames of 20 to 320 ms): per utterance
///   an endpoint and a final whose one segment is transcript_of() its audio;
///   no segment for no audio. An utterance is ended at 30 s.
/// - Text: per segment kSamplesPerTextByte samples of 24 kHz mono 16-bit PCM
///   a byte, sample `i` being sample_of(text, i), in chunks of kChunkSamples.
///
/// It serves kLanguages and kVoices only. `clock` is the manager's.
class SyntheticSessionDispatch final : public serving::SessionDispatch {
 public:
  static constexpr std::array<std::string_view, 2> kLanguages{"en", "en-US"};
  static constexpr std::array<std::string_view, 2> kVoices{"voice-a", "voice-b"};
  static constexpr std::uint32_t kSamplesPerTextByte = 80;
  static constexpr std::uint32_t kChunkSamples = 480;

  /// The transcript of an utterance that holds `audio`: a digest of it.
  [[nodiscard]] static std::string transcript_of(std::span<const std::byte> audio);
  /// Sample `index` of the segment synthesized from `text`.
  [[nodiscard]] static std::int16_t sample_of(std::string_view text, std::uint64_t index) noexcept;

  enum class Gate : std::uint8_t {
    /// The thread parks at the next hand-over, with all that is queued
    /// behind it: input stays accepted and its credit is not returned.
    BeforeWork = 0,
    /// An utterance being transcribed or a segment being synthesized does
    /// not complete, and the cleanup of a session that has one is not
    /// acknowledged. Everything else goes on.
    InJob = 1,
  };

  explicit SyntheticSessionDispatch(const SchedulerClock& clock);
  ~SyntheticSessionDispatch() override;

  /// Holds work at `gate`, from the next step the thread takes, until
  /// resume().
  void pause(Gate gate);
  /// Opens both gates. What was held continues in the order it arrived.
  void resume();
  /// Waits until nothing handed over or reported so far is left to work on.
  /// @return false if that takes longer than `patience`, as it does while
  ///   the thread is parked.
  [[nodiscard]] bool wait_idle(std::chrono::milliseconds patience);
  /// Sessions opened here whose cleanup has not been acknowledged.
  [[nodiscard]] std::size_t held_sessions() const noexcept { return held_sessions_.load(); }

  void attach(serving::SessionManager& manager) override;
  void stop() noexcept override;
  [[nodiscard]] Result<serving::SessionTerms> negotiate(
      const serving::SessionRequest& request) const override;
  [[nodiscard]] Result<void> open_session(std::uint64_t session_key,
                                          const serving::SessionTerms& terms) override;
  void on_transition(const serving::ManagedSessionTransition& transition) override;
  void audio(std::uint64_t session_key, std::span<const std::byte> frame) override;
  void text_segment(std::uint64_t session_key, std::uint64_t segment_id, std::string text) override;
  void finalize(std::uint64_t session_key, std::uint64_t client_sequence) override;
  void output_delivered(std::uint64_t session_key, serving::OutputKind kind) override;

 private:
  /// What a transcript says of the audio it covers.
  struct Digest {
    std::uint64_t hash = 14'695'981'039'346'656'037ULL;
    std::uint64_t bytes = 0;
    void add(std::span<const std::byte> audio) noexcept;
    [[nodiscard]] std::string text() const;
  };
  struct Opened {
    serving::SessionTerms terms;
  };
  struct Moved {
    std::optional<serving::LogicalSessionEvent> event;
    serving::LogicalSessionEffects effects;
    std::shared_ptr<serving::BoundedOutputQueue> output;
  };
  struct AudioIn {
    std::vector<std::byte> frame;
  };
  struct TextIn {
    std::uint64_t segment_id = 0;
    std::string text;
  };
  struct FinalizeIn {
    std::uint64_t client_sequence = 0;
  };
  struct Delivered {
    bool task_output = false;
  };
  struct Resumed {};
  struct Command {
    std::uint64_t session_key = 0;
    std::variant<Opened, Moved, AudioIn, TextIn, FinalizeIn, Delivered, Resumed> body;
  };

  /// An utterance that ended, its final still to be produced.
  struct EndedUtterance {
    std::uint64_t first_sample = 0;
    std::uint64_t end_sample = 0;
    Digest audio;
    serving::EndpointReason reason = serving::EndpointReason::ClientFinalize;
    std::uint64_t finalize_sequence = 0;
  };
  /// The segment being synthesized or delivered.
  struct ActiveSegment {
    std::uint64_t segment_id = 0;
    std::string text;
    std::uint64_t total_samples = 0;
    std::uint64_t next_sample = 0;
    std::uint32_t next_chunk = 0;
    bool synthesized = false;
    bool completion_queued = false;
  };
  struct Session {
    serving::SessionTerms terms;
    std::shared_ptr<serving::BoundedOutputQueue> output;
    /// Produced and not yet queued, oldest first; nothing else is produced
    /// meanwhile.
    std::deque<serving::OutputItem> held;
    /// Task items queued and not yet reported delivered.
    std::uint64_t undelivered = 0;
    bool draining = false;
    /// The drain is a client's half-close, which ends the open utterance.
    bool close_open_utterance = false;
    /// Client Finalizes the manager accepted and the binding has yet to hand
    /// over.
    std::uint32_t owed_finalize = 0;
    /// The manager refused a report or the queue stopped taking output; the
    /// session's cleanup request is on its way.
    bool ended = false;
    /// Cleanup was requested while a job was held; acknowledged at resume().
    bool releasing = false;

    /// Audio input. The open utterance is [utterance_start, samples_accepted).
    std::uint64_t samples_accepted = 0;
    std::uint64_t utterance_start = 0;
    Digest open_audio;
    std::uint64_t utterances = 0;
    std::deque<EndedUtterance> ended_utterances;
    /// The manager counts a finalization this dispatch has yet to complete.
    bool finalizing = false;
    /// Completions applied here that the sink has not shown yet. A
    /// `start_finalize` shown before one of them was answered by it.
    std::uint32_t completions_unseen = 0;

    /// Text input. The Finalize to answer once the segments were delivered.
    std::optional<std::uint64_t> finalize_sequence;
    std::deque<TextIn> waiting;
    std::optional<ActiveSegment> active;
    /// Segments finished since the last answered Finalize, and their samples.
    std::uint32_t segments_finished = 0;
    std::uint64_t samples_finished = 0;
  };

  void post(Command command);
  void run();
  /// The thread is held at the hand-over that is next. Caller holds mu_.
  [[nodiscard]] bool parked_locked() const;
  void handle(serving::SessionManager& manager, Command& command);
  void moved(serving::SessionManager& manager, std::uint64_t key, const Moved& moved);
  void resumed(serving::SessionManager& manager);
  [[nodiscard]] bool job_held(const Session& session) const;
  static void take_audio(serving::SessionManager& manager, std::uint64_t key, Session& session,
                         std::span<const std::byte> frame);
  void pump(serving::SessionManager& manager, std::uint64_t key, Session& session);
  /// Offers the oldest held item. False while the queue has no room or once
  /// it stopped taking task output.
  [[nodiscard]] bool offer_held(serving::SessionManager& manager, std::uint64_t key,
                                Session& session);
  /// One step of the session's work. False when nothing can be done until
  /// another command arrives.
  [[nodiscard]] bool advance_audio(serving::SessionManager& manager, std::uint64_t key,
                                   Session& session);
  [[nodiscard]] bool advance_text(serving::SessionManager& manager, std::uint64_t key,
                                  Session& session);
  [[nodiscard]] bool advance_segment(serving::SessionManager& manager, std::uint64_t key,
                                     Session& session);
  /// Produces the oldest ended utterance's endpoint and final.
  [[nodiscard]] bool complete_utterance(serving::SessionManager& manager, std::uint64_t key,
                                        Session& session);
  /// Reports `drain_completed` once nothing is owed or undelivered.
  static void complete_drain(serving::SessionManager& manager, std::uint64_t key, Session& session);
  static void end_utterance(Session& session, serving::EndpointReason reason,
                            std::uint64_t finalize_sequence);
  static void fail(serving::SessionManager& manager, std::uint64_t key, Session& session,
                   Error cause);

  const SchedulerClock& clock_;
  std::mutex mu_;
  std::condition_variable cv_;
  std::condition_variable idle_cv_;
  serving::SessionManager* manager_ = nullptr;
  std::deque<Command> commands_;
  bool stopping_ = false;
  bool working_ = false;
  bool before_work_paused_ = false;
  std::atomic<bool> in_job_paused_{false};
  std::once_flag joined_;
  std::atomic<std::size_t> held_sessions_{0};
  /// Touched by the worker thread only.
  std::map<std::uint64_t, Session> sessions_;
  std::thread worker_;
};
}  // namespace tensorplate::testing
