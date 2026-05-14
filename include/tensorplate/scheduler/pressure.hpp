// SPDX-License-Identifier: Apache-2.0
//
// V01-E06-F06-T01: Pressure signal value objects.
//
// Memory- and thermal-pressure signals are passed into the scheduler as
// vendor-neutral value objects. The signals carry source, severity,
// monotonic timestamp, and a bounded human-readable detail string. No
// CUDA, NVML, TensorRT, Jetson, Vitis, or other SDK type appears here.
//
// Memory pressure can be produced from the V01-E03 buffer-plane
// accounting (BufferAccounting::pressure). Thermal pressure can be
// produced from a future device collector; v0.1.0 only requires that
// the contract exist so serving-worker and observability integration
// can carry the signal end to end.

#pragma once

#include <cstdint>
#include <optional>
#include <string>
#include <string_view>

#include "tensorplate/scheduler/clock.hpp"

namespace tensorplate {

/// Source of a pressure signal.
enum class PressureSource : std::uint8_t {
  Memory = 0,
  Thermal = 1,
};

[[nodiscard]] std::string_view to_string(PressureSource source) noexcept;
[[nodiscard]] std::optional<PressureSource> pressure_source_from_string(
    std::string_view name) noexcept;

/// Severity of a pressure signal. Ordering is meaningful:
/// Normal < Warning < Critical.
enum class PressureSeverity : std::uint8_t {
  Normal = 0,
  Warning = 1,
  Critical = 2,
};

[[nodiscard]] std::string_view to_string(PressureSeverity severity) noexcept;
[[nodiscard]] std::optional<PressureSeverity> pressure_severity_from_string(
    std::string_view name) noexcept;

/// Memory- or thermal-pressure signal delivered to the scheduler.
/// Plain data; no hardware handles.
struct PressureSignal {
  PressureSource source = PressureSource::Memory;
  PressureSeverity severity = PressureSeverity::Normal;
  SchedulerClock::TimePoint timestamp{};
  /// Bounded free-text detail, e.g. "memory_manager:warning" or
  /// "thermal_zone:gpu:hot". v0.1.0 does not impose a length limit
  /// here; the metrics/event layer is responsible for label bounding.
  std::string detail;

  friend bool operator==(const PressureSignal& lhs, const PressureSignal& rhs) noexcept {
    return lhs.source == rhs.source && lhs.severity == rhs.severity &&
           lhs.timestamp == rhs.timestamp && lhs.detail == rhs.detail;
  }
  friend bool operator!=(const PressureSignal& lhs, const PressureSignal& rhs) noexcept {
    return !(lhs == rhs);
  }
};

}  // namespace tensorplate
