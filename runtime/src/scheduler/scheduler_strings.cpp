// SPDX-License-Identifier: Apache-2.0
//
// V01-E06-F01: Stable wire-name conversions for scheduler enums.

#include "tensorplate/scheduler/pressure.hpp"
#include "tensorplate/scheduler/scheduler.hpp"

namespace tensorplate {

std::string_view to_string(SchedulerEventKind kind) noexcept {
  switch (kind) {
    case SchedulerEventKind::Admitted:
      return "admitted";
    case SchedulerEventKind::AdmissionRejected:
      return "admission_rejected";
    case SchedulerEventKind::Dispatched:
      return "dispatched";
    case SchedulerEventKind::Completed:
      return "completed";
    case SchedulerEventKind::Cancelled:
      return "cancelled";
    case SchedulerEventKind::Expired:
      return "expired";
    case SchedulerEventKind::MemoryPressure:
      return "memory_pressure";
    case SchedulerEventKind::ThermalPressure:
      return "thermal_pressure";
  }
  return "unknown";
}

std::optional<SchedulerEventKind> scheduler_event_kind_from_string(std::string_view name) noexcept {
  if (name == "admitted")
    return SchedulerEventKind::Admitted;
  if (name == "admission_rejected")
    return SchedulerEventKind::AdmissionRejected;
  if (name == "dispatched")
    return SchedulerEventKind::Dispatched;
  if (name == "completed")
    return SchedulerEventKind::Completed;
  if (name == "cancelled")
    return SchedulerEventKind::Cancelled;
  if (name == "expired")
    return SchedulerEventKind::Expired;
  if (name == "memory_pressure")
    return SchedulerEventKind::MemoryPressure;
  if (name == "thermal_pressure")
    return SchedulerEventKind::ThermalPressure;
  return std::nullopt;
}

std::string_view to_string(CompletionStatus status) noexcept {
  switch (status) {
    case CompletionStatus::Success:
      return "success";
    case CompletionStatus::Failure:
      return "failure";
  }
  return "unknown";
}

std::string_view to_string(CancellationReason reason) noexcept {
  switch (reason) {
    case CancellationReason::ClientRequest:
      return "client_request";
    case CancellationReason::StaleSequence:
      return "stale_sequence";
    case CancellationReason::Shutdown:
      return "shutdown";
    case CancellationReason::Pressure:
      return "pressure";
  }
  return "unknown";
}

std::string_view to_string(PressureSource source) noexcept {
  switch (source) {
    case PressureSource::Memory:
      return "memory";
    case PressureSource::Thermal:
      return "thermal";
  }
  return "unknown";
}

std::optional<PressureSource> pressure_source_from_string(std::string_view name) noexcept {
  if (name == "memory")
    return PressureSource::Memory;
  if (name == "thermal")
    return PressureSource::Thermal;
  return std::nullopt;
}

std::string_view to_string(PressureSeverity severity) noexcept {
  switch (severity) {
    case PressureSeverity::Normal:
      return "normal";
    case PressureSeverity::Warning:
      return "warning";
    case PressureSeverity::Critical:
      return "critical";
  }
  return "unknown";
}

std::optional<PressureSeverity> pressure_severity_from_string(std::string_view name) noexcept {
  if (name == "normal")
    return PressureSeverity::Normal;
  if (name == "warning")
    return PressureSeverity::Warning;
  if (name == "critical")
    return PressureSeverity::Critical;
  return std::nullopt;
}

}  // namespace tensorplate
