// SPDX-License-Identifier: Apache-2.0
#include "serving/session/speech_dispatch.hpp"

#include <utility>

namespace tensorplate::serving {
namespace {
Error not_built() {
  return Error::make(Error::Code::Unsupported, "the speech dispatch is not built yet");
}
}  // namespace

SpeechJobDispatch::HeldBuffer::HeldBuffer(HeldBuffer&& other) noexcept
    : buffers_(std::exchange(other.buffers_, nullptr)), ref_(other.ref_) {}

SpeechJobDispatch::HeldBuffer& SpeechJobDispatch::HeldBuffer::operator=(
    HeldBuffer&& other) noexcept {
  if (this != &other) {
    reset();
    buffers_ = std::exchange(other.buffers_, nullptr);
    ref_ = other.ref_;
  }
  return *this;
}

SpeechJobDispatch::HeldBuffer::~HeldBuffer() {
  reset();
}

void SpeechJobDispatch::HeldBuffer::reset() noexcept {
  if (buffers_ != nullptr) {
    (void)std::exchange(buffers_, nullptr)->release(ref_);
  }
}

Result<std::unique_ptr<SpeechJobDispatch>> SpeechJobDispatch::create(
    std::shared_ptr<BoundedJobBridge> /*bridge*/, BufferManager& /*buffers*/,
    const SchedulerClock& /*clock*/, SpeechDispatchOptions /*options*/) {
  return unexpected(not_built());
}

SpeechJobDispatch::~SpeechJobDispatch() = default;

SpeechDispatchStats SpeechJobDispatch::stats() const {
  return {};
}

bool SpeechJobDispatch::wait_idle(std::chrono::milliseconds /*patience*/) {
  return true;
}

void SpeechJobDispatch::attach(SessionManager& /*manager*/) {}

void SpeechJobDispatch::stop() noexcept {}

Result<SessionTerms> SpeechJobDispatch::negotiate(const SessionRequest& /*request*/) const {
  return unexpected(not_built());
}

Result<void> SpeechJobDispatch::open_session(std::uint64_t /*session_key*/,
                                             const SessionTerms& /*terms*/) {
  return unexpected(not_built());
}

void SpeechJobDispatch::on_transition(const ManagedSessionTransition& /*transition*/) {}

void SpeechJobDispatch::audio(std::uint64_t /*session_key*/, std::span<const std::byte> /*frame*/) {
}

void SpeechJobDispatch::text_segment(std::uint64_t /*session_key*/, std::uint64_t /*segment_id*/,
                                     std::string /*text*/) {}

void SpeechJobDispatch::finalize(std::uint64_t /*session_key*/, std::uint64_t /*client_sequence*/) {
}

void SpeechJobDispatch::output_delivered(std::uint64_t /*session_key*/, OutputKind /*kind*/) {}
}  // namespace tensorplate::serving
