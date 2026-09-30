// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <cstddef>
#include <cstdint>
#include <optional>
#include <string>
#include <string_view>
#include <vector>

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate::ipc {

/// Maximum newline-delimited runtime control frame, including the newline.
inline constexpr std::size_t kWorkerControlMaxFrameBytes = 65'536;
/// Maximum number of sessions reported by one member.
inline constexpr std::uint32_t kWorkerControlMaxSessions = 2'048;

/// Runtime operations on the agent-to-worker control channel.
enum class WorkerControlOp {
  AdmissionFence,
  Activate,
  Retire,
  QuotaAssign,
  LedgerStatus,
  PressureDirective,
};

/// One deployment generation served by a worker.
struct WorkerMember {
  std::string deployment_id;
  std::uint64_t generation = 0;
  bool operator==(const WorkerMember&) const = default;
};

/// Per-domain bytes reserved for a member's logical sessions.
struct WorkerDomainBytes {
  std::optional<std::uint64_t> shared_pool;
  std::optional<std::uint64_t> guest_ram;
  std::optional<std::uint64_t> device_vram;
  bool operator==(const WorkerDomainBytes&) const = default;
};

/// Effective session count and per-domain reservation.
struct WorkerQuota {
  std::uint32_t session_count = 0;
  WorkerDomainBytes domain_bytes;
  bool operator==(const WorkerQuota&) const = default;
};

/// Pressure command applied to one member.
enum class WorkerPressureLevel { Normal, ShedAdmission, TerminateNewest, Resume };

/// TerminateNewest alone carries a count.
struct WorkerPressure {
  WorkerPressureLevel level = WorkerPressureLevel::Normal;
  std::optional<std::uint32_t> count;
  bool operator==(const WorkerPressure&) const = default;
};

/// Member reservations retained until physical release.
struct WorkerLedger {
  std::uint32_t reserved = 0;
  std::uint32_t active = 0;
  std::uint32_t closing = 0;
  std::uint32_t ceiling = 0;
  std::vector<std::uint64_t> admission_monotonic_ns;
  bool operator==(const WorkerLedger&) const = default;
};

/// One runtime control request. The transaction id is "runtime" for
/// quota, ledger, and pressure operations.
struct WorkerControlRequest {
  std::string transaction_id;
  std::string correlation_id;
  WorkerControlOp op = WorkerControlOp::LedgerStatus;
  WorkerMember member;
  std::optional<WorkerQuota> quota;
  std::optional<WorkerPressure> pressure;
  std::optional<std::uint64_t> drain_timeout_ms;
  std::optional<std::uint64_t> timeout_ms;
  bool operator==(const WorkerControlRequest&) const = default;
};

/// Outcome of one control request.
enum class WorkerControlStatus { Ok, Error, NotReady, Timeout, Unsupported, MemberMismatch };

/// One runtime control response. Failed outcomes carry an error; a member
/// mismatch names the member that actually answered.
struct WorkerControlResponse {
  std::string transaction_id;
  std::string correlation_id;
  WorkerControlOp op = WorkerControlOp::LedgerStatus;
  WorkerMember member;
  WorkerControlStatus status = WorkerControlStatus::Ok;
  std::optional<WorkerLedger> ledger;
  std::optional<WorkerQuota> quota;
  std::optional<Error> error;
  bool operator==(const WorkerControlResponse&) const = default;
};

/// Decode a complete runtime request frame, including its newline.
/// Legacy in-process operations and malformed or oversized frames fail closed.
[[nodiscard]] Result<WorkerControlRequest> decode_worker_control_request(std::string_view frame);

/// Decode a complete runtime response frame, including its newline.
[[nodiscard]] Result<WorkerControlResponse> decode_worker_control_response(std::string_view frame);

/// Encode a validated request using the Rust peer's canonical field order.
[[nodiscard]] Result<std::string> encode_worker_control_request(
    const WorkerControlRequest& request);

/// Encode a validated response using the Rust peer's canonical field order.
[[nodiscard]] Result<std::string> encode_worker_control_response(
    const WorkerControlResponse& response);

/// Check operation, echoed IDs and member against the request that was sent.
[[nodiscard]] Result<void> worker_control_answers(const WorkerControlResponse& response,
                                                  const WorkerControlRequest& request);

}  // namespace tensorplate::ipc
