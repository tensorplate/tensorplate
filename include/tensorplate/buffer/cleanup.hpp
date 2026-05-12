// SPDX-License-Identifier: Apache-2.0
//
// V01-E03-F03: Deterministic buffer-release helpers for cancellation,
// timeout, and partial-output error paths.
//
// These helpers exist so that scheduler, serving-worker, and execution-
// session code can release every buffer touched by a failed inference
// without rebuilding cleanup boilerplate at each callsite. They sit
// directly on top of `BufferManager::release_if_owned` and are safe to
// call from any error path:
//
//   - They release every *unique* buffer id at most once. Duplicate ids
//     in malformed input fixtures do not cause a double release.
//   - They never throw and never block.
//   - They surface release failures through a CleanupReport so the
//     caller can preserve the original request error and still emit a
//     structured log if cleanup itself misbehaved.
//
// The helpers are deliberately backend-neutral: they take an
// `InferRequest` or a list of `NamedOutput` values, not a scheduler
// or session pointer. This keeps the dependency direction downward
// (buffer plane has no upward dependency on scheduler/session).

#pragma once

#include <cstdint>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/core/infer_result.hpp"

namespace tensorplate {

/// Outcome of a cleanup pass. Plain data; safe to log without further
/// processing. Use this to record release failures alongside the
/// original request failure (cancellation, timeout, validation error)
/// without masking the original error.
struct CleanupReport {
  /// Number of buffers actually released by this pass.
  std::size_t buffers_released = 0;

  /// Number of release attempts that returned a typed error (unknown
  /// handle, double release). Each entry in `errors` corresponds to one
  /// such failure.
  std::vector<Error> errors;

  /// True if the cleanup pass freed everything it tried to free.
  [[nodiscard]] bool clean() const noexcept { return errors.empty(); }
};

/// Release every unique input buffer in `request`. Idempotent on
/// duplicate ids inside the request's inputs vector. Returns a report
/// regardless of partial failures so the caller can preserve the
/// original error.
///
/// Typical callsites:
///   - scheduler cancellation,
///   - scheduler deadline expiry,
///   - request validation rejection that occurred *after* buffers were
///     allocated (e.g. shape mismatch caught in the NVI wrapper).
[[nodiscard]] CleanupReport release_request_buffers(BufferManager& manager,
                                                    const InferRequest& request) noexcept;

/// Release every unique output buffer in `outputs`. Used by the
/// execution session when result construction fails after one or more
/// outputs were allocated. Does not touch input buffers; caller pairs
/// this with `release_request_buffers` when both must be cleaned up.
[[nodiscard]] CleanupReport release_partial_outputs(BufferManager& manager,
                                                    const std::vector<NamedOutput>& outputs) noexcept;

/// RAII guard that releases the input buffers of `request` when it
/// leaves scope unless `dismiss()` is called first. Useful for the
/// adapter-side flow: take the guard at request entry, run inference,
/// dismiss the guard on the success path. Cancellation, timeout, and
/// exception unwinding all release the buffers deterministically.
///
/// The guard borrows `manager` and `request` by reference; both must
/// outlive the guard. A dismissed guard performs no work in its
/// destructor.
class RequestBufferGuard {
 public:
  RequestBufferGuard(BufferManager& manager, const InferRequest& request) noexcept;
  ~RequestBufferGuard();

  RequestBufferGuard(const RequestBufferGuard&) = delete;
  RequestBufferGuard& operator=(const RequestBufferGuard&) = delete;
  RequestBufferGuard(RequestBufferGuard&&) = delete;
  RequestBufferGuard& operator=(RequestBufferGuard&&) = delete;

  /// Cancel the destructor-side release. Call this only on the success
  /// path, after the inference result has taken ownership of any output
  /// buffers. The guard becomes inert and `~RequestBufferGuard` does
  /// nothing.
  void dismiss() noexcept;

  /// True if `dismiss()` has been called.
  [[nodiscard]] bool dismissed() const noexcept { return dismissed_; }

  /// Report produced by the last destructor-side cleanup. Empty until
  /// the destructor runs.
  [[nodiscard]] const CleanupReport& report() const noexcept { return report_; }

 private:
  BufferManager* manager_;
  const InferRequest* request_;
  bool dismissed_ = false;
  CleanupReport report_;
};

}  // namespace tensorplate
