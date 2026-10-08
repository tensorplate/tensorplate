// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <atomic>
#include <condition_variable>
#include <cstdint>
#include <deque>
#include <map>
#include <memory>
#include <mutex>
#include <optional>
#include <string>
#include <string_view>
#include <thread>
#include <variant>
#include <vector>

#include "tensorplate/scheduler/clock.hpp"

#include "serving/session/dispatch.hpp"

namespace tensorplate::serving {

/// A dispatch with no backend, for exercising a transport binding. It serves
/// both input kinds and produces fixed results:
///
/// - Audio input: 16 kHz mono 16-bit PCM in frames of 20 to 320 ms. Each
///   utterance yields its endpoint and then a final: kTranscript as one
///   segment spanning it, or no segment if it holds no audio.
/// - Text input: each segment yields kSamplesPerTextByte samples of 24 kHz
///   mono 16-bit PCM per byte of text, sample `i` of a segment being
///   sample_at(i), in chunks of kChunkSamples with a shorter last one. A
///   Finalize is answered once every accepted segment was delivered.
///
/// Credit, backpressure, drain and cleanup follow SessionDispatch; cleanup is
/// confirmed at once. It keeps no audio and never ends an utterance itself,
/// so SessionTerms::max_utterance is not kept. `clock` is the manager's.
class SyntheticSessionDispatch final : public SessionDispatch {
 public:
  static constexpr std::string_view kTranscript = "hello";
  static constexpr std::uint32_t kSamplesPerTextByte = 80;
  static constexpr std::uint32_t kChunkSamples = 480;

  [[nodiscard]] static constexpr std::int16_t sample_at(std::uint64_t index) noexcept {
    return static_cast<std::int16_t>(index & 0x7fffU);
  }

  explicit SyntheticSessionDispatch(const SchedulerClock& clock);
  ~SyntheticSessionDispatch() override;

  void attach(SessionManager& manager) override;
  void stop() noexcept override;
  [[nodiscard]] Result<SessionTerms> negotiate(const SessionRequest& request) const override;
  [[nodiscard]] Result<void> open_session(std::uint64_t session_key,
                                          const SessionTerms& terms) override;
  void on_transition(const ManagedSessionTransition& transition) override;
  void audio(std::uint64_t session_key, std::span<const std::byte> frame) override;
  void text_segment(std::uint64_t session_key, std::uint64_t segment_id, std::string text) override;
  void finalize(std::uint64_t session_key, std::uint64_t client_sequence) override;
  void output_delivered(std::uint64_t session_key, OutputKind kind) override;

  /// Sessions opened here whose cleanup or release has not been seen.
  [[nodiscard]] std::size_t held_sessions() const noexcept { return held_sessions_.load(); }

 private:
  struct Opened {
    SessionTerms terms;
  };
  struct Moved {
    std::optional<LogicalSessionEvent> event;
    LogicalSessionEffects effects;
    std::shared_ptr<BoundedOutputQueue> output;
  };
  struct AudioIn {
    std::uint64_t bytes = 0;
  };
  struct TextIn {
    std::uint64_t segment_id = 0;
    std::uint64_t bytes = 0;
  };
  struct FinalizeIn {
    std::uint64_t client_sequence = 0;
  };
  struct Delivered {
    bool task_output = false;
  };
  struct Command {
    std::uint64_t session_key = 0;
    std::variant<Opened, Moved, AudioIn, TextIn, FinalizeIn, Delivered> body;
  };

  /// The segment being synthesized or delivered.
  struct ActiveSegment {
    std::uint64_t segment_id = 0;
    std::uint64_t total_samples = 0;
    std::uint64_t next_sample = 0;
    std::uint32_t next_chunk = 0;
    bool completion_queued = false;
  };
  struct Session {
    SessionTerms terms;
    std::shared_ptr<BoundedOutputQueue> output;
    /// Produced and not yet queued, oldest first; nothing else is produced
    /// meanwhile.
    std::deque<OutputItem> held;
    /// Task items queued and not yet reported delivered.
    std::uint64_t undelivered = 0;
    std::optional<std::uint64_t> finalize_sequence;
    bool draining = false;
    /// The drain is a client's half-close, which ends the open utterance.
    bool close_open_utterance = false;
    /// Client Finalizes the manager accepted and the binding has yet to hand
    /// over.
    std::uint32_t owed_finalize = 0;
    /// The manager refused a report or the queue stopped taking output; the
    /// session's cleanup request is on its way.
    bool ended = false;
    std::uint64_t samples_accepted = 0;
    std::uint64_t utterance_start = 0;
    std::uint64_t utterances = 0;
    std::deque<TextIn> waiting;
    std::optional<ActiveSegment> active;
    /// Segments finished since the last answered Finalize, and their samples.
    std::uint32_t segments_finished = 0;
    std::uint64_t samples_finished = 0;
  };

  void post(Command command);
  void run();
  void handle(SessionManager& manager, Command& command);
  void moved(SessionManager& manager, std::uint64_t key, const Moved& moved);
  void pump(SessionManager& manager, std::uint64_t key, Session& session);
  /// Offers the oldest held item. False while the queue has no room or once
  /// it stopped taking task output.
  [[nodiscard]] bool offer_held(SessionManager& manager, std::uint64_t key, Session& session);
  /// One step of the session's work. False when nothing can be done until
  /// another command arrives.
  [[nodiscard]] static bool advance_audio(SessionManager& manager, std::uint64_t key,
                                          Session& session);
  [[nodiscard]] static bool advance_text(SessionManager& manager, std::uint64_t key,
                                         Session& session);
  [[nodiscard]] static bool advance_segment(SessionManager& manager, std::uint64_t key,
                                            Session& session);
  /// Reports `drain_completed` once nothing is owed or undelivered.
  static void complete_drain(SessionManager& manager, std::uint64_t key, Session& session);
  /// Ends the open utterance: holds its endpoint and its final.
  static void hold_final(Session& session, EndpointReason reason, std::uint64_t finalize_sequence);
  static void fail(SessionManager& manager, std::uint64_t key, Session& session, Error cause);

  const SchedulerClock& clock_;
  std::mutex mu_;
  std::condition_variable cv_;
  SessionManager* manager_ = nullptr;
  std::deque<Command> commands_;
  bool stopping_ = false;
  std::once_flag joined_;
  std::atomic<std::size_t> held_sessions_{0};
  /// Touched by the worker thread only.
  std::map<std::uint64_t, Session> sessions_;
  std::thread worker_;
};
}  // namespace tensorplate::serving
