// SPDX-License-Identifier: Apache-2.0
#include "tensorplate/ipc/worker_control.hpp"

#include <algorithm>
#include <array>
#include <limits>
#include <nlohmann/json.hpp>
#include <stdexcept>
#include <string>
#include <string_view>
#include <unordered_set>
#include <utility>
#include <vector>

namespace tensorplate::ipc {
namespace {
using Json = nlohmann::ordered_json;
constexpr std::uint64_t kMaxSafeInteger = 9'007'199'254'740'991ULL;
constexpr std::string_view kVersion = "0.1";
constexpr std::string_view kRuntime = "runtime";

[[noreturn]] void invalid(std::string_view reason) {
  throw std::invalid_argument(std::string(reason));
}

void fields(const Json& object, std::initializer_list<std::string_view> allowed) {
  if (!object.is_object()) {
    invalid("expected object");
  }
  for (auto it = object.begin(); it != object.end(); ++it) {
    if (std::find(allowed.begin(), allowed.end(), it.key()) == allowed.end()) {
      invalid("unknown field: " + it.key());
    }
  }
}

std::string string_field(const Json& object, const char* name) {
  if (!object.contains(name) || !object.at(name).is_string()) {
    invalid(name);
  }
  return object.at(name).get<std::string>();
}

std::uint64_t number_field(const Json& object, const char* name, std::uint64_t maximum,
                           std::uint64_t minimum = 0) {
  if (!object.contains(name)) {
    invalid(name);
  }
  const auto& value = object.at(name);
  if (!value.is_number_integer()) {
    invalid(name);
  }
  if (value.is_number_unsigned()) {
    const auto n = value.get<std::uint64_t>();
    if (n < minimum || n > maximum) {
      invalid(name);
    }
    return n;
  }
  const auto n = value.get<std::int64_t>();
  if (n < 0 || static_cast<std::uint64_t>(n) < minimum || static_cast<std::uint64_t>(n) > maximum) {
    invalid(name);
  }
  return static_cast<std::uint64_t>(n);
}

bool valid_id(std::string_view id) {
  if (id.empty() || id.size() > 64) {
    return false;
  }
  return std::all_of(id.begin(), id.end(), [](unsigned char c) {
    return (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '_' ||
           c == '-';
  });
}

void validate_member(const WorkerMember& member) {
  const auto& id = member.deployment_id;
  if (id.empty() || id.size() > 128 || id == "." || id == ".." ||
      !std::all_of(id.begin(), id.end(),
                   [](unsigned char c) {
                     return (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') ||
                            (c >= '0' && c <= '9') || c == '.' || c == '_' || c == '-';
                   }) ||
      member.generation == 0 || member.generation > kMaxSafeInteger) {
    invalid("invalid member");
  }
}

void validate_quota(const WorkerQuota& quota) {
  const auto& d = quota.domain_bytes;
  if (quota.session_count > kWorkerControlMaxSessions ||
      (quota.session_count > 0 && !d.shared_pool && !d.guest_ram && !d.device_vram) ||
      (d.shared_pool && (d.guest_ram || d.device_vram))) {
    invalid("invalid quota");
  }
  for (const auto& n : {d.shared_pool, d.guest_ram, d.device_vram}) {
    if (n && *n > kMaxSafeInteger) {
      invalid("invalid quota bytes");
    }
  }
}

void validate_pressure(const WorkerPressure& pressure) {
  if (pressure.level == WorkerPressureLevel::TerminateNewest) {
    if (!pressure.count || *pressure.count == 0 || *pressure.count > kWorkerControlMaxSessions) {
      invalid("invalid pressure count");
    }
  } else if (pressure.count) {
    invalid("unexpected pressure count");
  }
}

void validate_ledger(const WorkerLedger& ledger) {
  const auto held = static_cast<std::uint64_t>(ledger.reserved) + ledger.active + ledger.closing;
  const auto admitted = static_cast<std::uint64_t>(ledger.reserved) + ledger.active;
  if (ledger.ceiling > kWorkerControlMaxSessions || held > ledger.ceiling ||
      ledger.admission_monotonic_ns.size() != admitted ||
      !std::is_sorted(ledger.admission_monotonic_ns.begin(), ledger.admission_monotonic_ns.end())) {
    invalid("invalid ledger");
  }
}

std::string_view op_name(WorkerControlOp op) {
  switch (op) {
    case WorkerControlOp::AdmissionFence:
      return "admission_fence";
    case WorkerControlOp::Activate:
      return "activate";
    case WorkerControlOp::Retire:
      return "retire";
    case WorkerControlOp::QuotaAssign:
      return "quota_assign";
    case WorkerControlOp::LedgerStatus:
      return "ledger_status";
    case WorkerControlOp::PressureDirective:
      return "pressure_directive";
  }
  invalid("unknown operation");
}

WorkerControlOp parse_op(const std::string& op) {
  constexpr std::array<std::pair<std::string_view, WorkerControlOp>, 6> kOps{{
      {"admission_fence", WorkerControlOp::AdmissionFence},
      {"activate", WorkerControlOp::Activate},
      {"retire", WorkerControlOp::Retire},
      {"quota_assign", WorkerControlOp::QuotaAssign},
      {"ledger_status", WorkerControlOp::LedgerStatus},
      {"pressure_directive", WorkerControlOp::PressureDirective},
  }};
  for (const auto& [name, value] : kOps) {
    if (op == name) {
      return value;
    }
  }
  invalid("unsupported operation");
}

bool transactional(WorkerControlOp op) {
  return op == WorkerControlOp::AdmissionFence || op == WorkerControlOp::Activate ||
         op == WorkerControlOp::Retire;
}

std::string_view status_name(WorkerControlStatus status) {
  switch (status) {
    case WorkerControlStatus::Ok:
      return "ok";
    case WorkerControlStatus::Error:
      return "error";
    case WorkerControlStatus::NotReady:
      return "not_ready";
    case WorkerControlStatus::Timeout:
      return "timeout";
    case WorkerControlStatus::Unsupported:
      return "unsupported";
    case WorkerControlStatus::MemberMismatch:
      return "member_mismatch";
  }
  invalid("unknown status");
}

WorkerControlStatus parse_status(const std::string& status) {
  constexpr std::array<std::pair<std::string_view, WorkerControlStatus>, 6> kStatuses{{
      {"ok", WorkerControlStatus::Ok},
      {"error", WorkerControlStatus::Error},
      {"not_ready", WorkerControlStatus::NotReady},
      {"timeout", WorkerControlStatus::Timeout},
      {"unsupported", WorkerControlStatus::Unsupported},
      {"member_mismatch", WorkerControlStatus::MemberMismatch},
  }};
  for (const auto& [name, value] : kStatuses) {
    if (status == name) {
      return value;
    }
  }
  invalid("unsupported status");
}

std::string_view pressure_name(WorkerPressureLevel level) {
  switch (level) {
    case WorkerPressureLevel::Normal:
      return "normal";
    case WorkerPressureLevel::ShedAdmission:
      return "shed_admission";
    case WorkerPressureLevel::TerminateNewest:
      return "terminate_newest";
    case WorkerPressureLevel::Resume:
      return "resume";
  }
  invalid("unknown pressure level");
}

WorkerPressureLevel parse_pressure_level(const std::string& level) {
  if (level == "normal") {
    return WorkerPressureLevel::Normal;
  }
  if (level == "shed_admission") {
    return WorkerPressureLevel::ShedAdmission;
  }
  if (level == "terminate_newest") {
    return WorkerPressureLevel::TerminateNewest;
  }
  if (level == "resume") {
    return WorkerPressureLevel::Resume;
  }
  invalid("unsupported pressure level");
}

void validate_ids(WorkerControlOp op, const std::string& transaction_id,
                  const std::string& correlation_id, const WorkerMember& member) {
  if (!valid_id(transaction_id) || !valid_id(correlation_id) ||
      (transaction_id == kRuntime) == transactional(op)) {
    invalid("invalid control ids");
  }
  validate_member(member);
}

void validate_request(const WorkerControlRequest& r) {
  validate_ids(r.op, r.transaction_id, r.correlation_id, r.member);
  if (r.quota.has_value() != (r.op == WorkerControlOp::QuotaAssign) ||
      r.pressure.has_value() != (r.op == WorkerControlOp::PressureDirective) ||
      r.drain_timeout_ms.has_value() != (r.op == WorkerControlOp::Retire)) {
    invalid("request payload does not match operation");
  }
  if (r.quota) {
    validate_quota(*r.quota);
  }
  if (r.pressure) {
    validate_pressure(*r.pressure);
  }
  if (r.drain_timeout_ms && *r.drain_timeout_ms == 0) {
    invalid("invalid drain timeout");
  }
  if (r.timeout_ms && *r.timeout_ms == 0) {
    invalid("invalid timeout");
  }
}

void validate_response(const WorkerControlResponse& r) {
  validate_ids(r.op, r.transaction_id, r.correlation_id, r.member);
  const bool failed =
      r.status != WorkerControlStatus::Ok && r.status != WorkerControlStatus::MemberMismatch;
  if (failed != r.error.has_value() ||
      r.ledger.has_value() !=
          (r.op == WorkerControlOp::LedgerStatus && r.status == WorkerControlStatus::Ok) ||
      r.quota.has_value() !=
          ((r.op == WorkerControlOp::Activate || r.op == WorkerControlOp::QuotaAssign) &&
           r.status == WorkerControlStatus::Ok)) {
    invalid("response payload does not match outcome");
  }
  if (r.ledger) {
    validate_ledger(*r.ledger);
  }
  if (r.quota) {
    validate_quota(*r.quota);
  }
}

WorkerMember parse_member(const Json& j) {
  fields(j, {"deployment_id", "generation"});
  WorkerMember member{string_field(j, "deployment_id"),
                      number_field(j, "generation", kMaxSafeInteger, 1)};
  validate_member(member);
  return member;
}

Json member_json(const WorkerMember& m) {
  return Json{{"deployment_id", m.deployment_id}, {"generation", m.generation}};
}

WorkerQuota parse_quota(const Json& j) {
  fields(j, {"session_count", "domain_bytes"});
  if (!j.contains("domain_bytes")) {
    invalid("domain_bytes");
  }
  const auto& d = j.at("domain_bytes");
  fields(d, {"shared_pool", "guest_ram", "device_vram"});
  WorkerQuota q;
  q.session_count =
      static_cast<std::uint32_t>(number_field(j, "session_count", kWorkerControlMaxSessions));
  if (d.contains("shared_pool")) {
    q.domain_bytes.shared_pool = number_field(d, "shared_pool", kMaxSafeInteger);
  }
  if (d.contains("guest_ram")) {
    q.domain_bytes.guest_ram = number_field(d, "guest_ram", kMaxSafeInteger);
  }
  if (d.contains("device_vram")) {
    q.domain_bytes.device_vram = number_field(d, "device_vram", kMaxSafeInteger);
  }
  validate_quota(q);
  return q;
}

Json quota_json(const WorkerQuota& q) {
  Json d = Json::object();
  if (q.domain_bytes.shared_pool) {
    d["shared_pool"] = *q.domain_bytes.shared_pool;
  }
  if (q.domain_bytes.guest_ram) {
    d["guest_ram"] = *q.domain_bytes.guest_ram;
  }
  if (q.domain_bytes.device_vram) {
    d["device_vram"] = *q.domain_bytes.device_vram;
  }
  return Json{{"session_count", q.session_count}, {"domain_bytes", std::move(d)}};
}

WorkerPressure parse_pressure(const Json& j) {
  fields(j, {"level", "count"});
  WorkerPressure p;
  p.level = parse_pressure_level(string_field(j, "level"));
  if (j.contains("count")) {
    p.count = static_cast<std::uint32_t>(number_field(j, "count", kWorkerControlMaxSessions, 1));
  }
  validate_pressure(p);
  return p;
}

Json pressure_json(const WorkerPressure& p) {
  Json j = {{"level", pressure_name(p.level)}};
  if (p.count) {
    j["count"] = *p.count;
  }
  return j;
}

WorkerLedger parse_ledger(const Json& j) {
  fields(j, {"reserved", "active", "closing", "ceiling", "admission_monotonic_ns"});
  WorkerLedger l;
  l.reserved = static_cast<std::uint32_t>(number_field(j, "reserved", kWorkerControlMaxSessions));
  l.active = static_cast<std::uint32_t>(number_field(j, "active", kWorkerControlMaxSessions));
  l.closing = static_cast<std::uint32_t>(number_field(j, "closing", kWorkerControlMaxSessions));
  l.ceiling = static_cast<std::uint32_t>(number_field(j, "ceiling", kWorkerControlMaxSessions));
  if (!j.contains("admission_monotonic_ns") || !j.at("admission_monotonic_ns").is_array()) {
    invalid("admission_monotonic_ns");
  }
  for (const auto& n : j.at("admission_monotonic_ns")) {
    Json wrapper = {{"value", n}};
    l.admission_monotonic_ns.push_back(
        number_field(wrapper, "value", std::numeric_limits<std::uint64_t>::max()));
    if (l.admission_monotonic_ns.size() > kWorkerControlMaxSessions) {
      invalid("ledger size");
    }
  }
  validate_ledger(l);
  return l;
}

Json ledger_json(const WorkerLedger& l) {
  return Json{{"reserved", l.reserved},
              {"active", l.active},
              {"closing", l.closing},
              {"ceiling", l.ceiling},
              {"admission_monotonic_ns", l.admission_monotonic_ns}};
}

Json parse_frame(std::string_view frame) {
  if (frame.empty() || frame.size() > kWorkerControlMaxFrameBytes || frame.back() != '\n' ||
      frame.find('\n') != frame.size() - 1 || frame.find('\r') != std::string_view::npos) {
    invalid("invalid frame boundary");
  }
  std::vector<std::unordered_set<std::string>> keys;
  auto unique_keys = [&keys](int, Json::parse_event_t event, Json& value) {
    if (event == Json::parse_event_t::object_start) {
      keys.emplace_back();
    }
    if (event == Json::parse_event_t::key && !keys.back().insert(value.get<std::string>()).second) {
      invalid("duplicate field");
    }
    if (event == Json::parse_event_t::object_end) {
      keys.pop_back();
    }
    return true;
  };
  return Json::parse(frame.begin(), frame.end() - 1, unique_keys);
}

Result<std::string> finish_frame(const Json& j) {
  std::string frame = j.dump(-1, ' ', false, Json::error_handler_t::strict) + "\n";
  if (frame.size() > kWorkerControlMaxFrameBytes) {
    return unexpected(Error::Code::ConfigInvalid, "control frame exceeds limit");
  }
  return frame;
}

std::string utf8_prefix(const std::string& text, std::size_t length) {
  std::size_t end = std::min(length, text.size());
  while (end < text.size() && end > 0 && (static_cast<unsigned char>(text[end]) & 0xC0U) == 0x80U) {
    --end;
  }
  return text.substr(0, end);
}

void fit_error_field(Json& frame, const char* field, const std::string& text) {
  std::size_t lo = 0;
  std::size_t hi = text.size();
  while (lo < hi) {
    const auto mid = lo + (hi - lo + 1) / 2;
    frame["error"][field] = utf8_prefix(text, mid);
    if (finish_frame(frame)) {
      lo = mid;
    } else {
      hi = mid - 1;
    }
  }
  frame["error"][field] = utf8_prefix(text, lo);
}

Result<void> check_version(const Json& j) {
  if (string_field(j, "schema_version") != kVersion) {
    return unexpected(Error::Code::Unsupported, "unsupported control schema version");
  }
  return {};
}
}  // namespace

Result<WorkerControlRequest> decode_worker_control_request(std::string_view frame) {
  try {
    const Json j = parse_frame(frame);
    fields(j, {"schema_version", "transaction_id", "correlation_id", "op", "member", "quota",
               "pressure", "drain_timeout_ms", "timeout_ms"});
    auto version = check_version(j);
    if (!version) {
      return unexpected(version.error());
    }
    WorkerControlRequest r;
    r.transaction_id = string_field(j, "transaction_id");
    r.correlation_id = string_field(j, "correlation_id");
    r.op = parse_op(string_field(j, "op"));
    if (!j.contains("member")) {
      invalid("member");
    }
    r.member = parse_member(j.at("member"));
    if (j.contains("quota")) {
      r.quota = parse_quota(j.at("quota"));
    }
    if (j.contains("pressure")) {
      r.pressure = parse_pressure(j.at("pressure"));
    }
    if (j.contains("drain_timeout_ms")) {
      r.drain_timeout_ms =
          number_field(j, "drain_timeout_ms", std::numeric_limits<std::uint64_t>::max(), 1);
    }
    if (j.contains("timeout_ms")) {
      r.timeout_ms = number_field(j, "timeout_ms", std::numeric_limits<std::uint64_t>::max(), 1);
    }
    validate_request(r);
    return r;
  } catch (const std::exception& e) {
    return unexpected(Error::Code::ConfigInvalid, e.what());
  }
}

Result<WorkerControlResponse> decode_worker_control_response(std::string_view frame) {
  try {
    const Json j = parse_frame(frame);
    fields(j, {"schema_version", "transaction_id", "correlation_id", "op", "member", "status",
               "ledger", "quota", "error"});
    auto version = check_version(j);
    if (!version) {
      return unexpected(version.error());
    }
    WorkerControlResponse r;
    r.transaction_id = string_field(j, "transaction_id");
    r.correlation_id = string_field(j, "correlation_id");
    r.op = parse_op(string_field(j, "op"));
    if (!j.contains("member")) {
      invalid("member");
    }
    r.member = parse_member(j.at("member"));
    r.status = parse_status(string_field(j, "status"));
    if (j.contains("ledger")) {
      r.ledger = parse_ledger(j.at("ledger"));
    }
    if (j.contains("quota")) {
      r.quota = parse_quota(j.at("quota"));
    }
    if (j.contains("error")) {
      const auto& err = j.at("error");
      fields(err, {"code", "message", "context"});
      const auto code = error_code_from_string(string_field(err, "code"));
      if (!code) {
        invalid("unknown error code");
      }
      r.error = Error::make(*code, string_field(err, "message"));
      if (err.contains("context")) {
        r.error->context = string_field(err, "context");
      }
    }
    validate_response(r);
    return r;
  } catch (const std::exception& e) {
    return unexpected(Error::Code::ConfigInvalid, e.what());
  }
}

Result<std::string> encode_worker_control_request(const WorkerControlRequest& r) {
  try {
    validate_request(r);
    Json j = {{"schema_version", kVersion},
              {"transaction_id", r.transaction_id},
              {"correlation_id", r.correlation_id},
              {"op", op_name(r.op)},
              {"member", member_json(r.member)}};
    if (r.quota) {
      j["quota"] = quota_json(*r.quota);
    }
    if (r.pressure) {
      j["pressure"] = pressure_json(*r.pressure);
    }
    if (r.drain_timeout_ms) {
      j["drain_timeout_ms"] = *r.drain_timeout_ms;
    }
    if (r.timeout_ms) {
      j["timeout_ms"] = *r.timeout_ms;
    }
    return finish_frame(j);
  } catch (const std::exception& e) {
    return unexpected(Error::Code::ConfigInvalid, e.what());
  }
}

Result<std::string> encode_worker_control_response(const WorkerControlResponse& r) {
  try {
    validate_response(r);
    Json j = {{"schema_version", kVersion},         {"transaction_id", r.transaction_id},
              {"correlation_id", r.correlation_id}, {"op", op_name(r.op)},
              {"member", member_json(r.member)},    {"status", status_name(r.status)}};
    if (r.ledger) {
      j["ledger"] = ledger_json(*r.ledger);
    }
    if (r.quota) {
      j["quota"] = quota_json(*r.quota);
    }
    if (r.error) {
      Json err = {{"code", to_string(r.error->code)}, {"message", r.error->message}};
      if (r.error->context) {
        err["context"] = *r.error->context;
      }
      j["error"] = std::move(err);
      auto full = finish_frame(j);
      if (full) {
        return full;
      }
      if (r.error->context) {
        j["error"]["message"] = "";
        if (finish_frame(j)) {
          fit_error_field(j, "message", r.error->message);
          return finish_frame(j);
        }
        j["error"]["context"] = "";
        j["error"]["message"] = r.error->message;
        if (finish_frame(j)) {
          fit_error_field(j, "context", *r.error->context);
          return finish_frame(j);
        }
      }
      j["error"]["message"] = "";
      fit_error_field(j, "message", r.error->message);
      if (r.error->context) {
        fit_error_field(j, "context", *r.error->context);
      }
    }
    return finish_frame(j);
  } catch (const std::exception& e) {
    return unexpected(Error::Code::ConfigInvalid, e.what());
  }
}

Result<void> worker_control_answers(const WorkerControlResponse& response,
                                    const WorkerControlRequest& request) {
  if (response.op != request.op || response.transaction_id != request.transaction_id ||
      response.correlation_id != request.correlation_id ||
      (response.status == WorkerControlStatus::MemberMismatch
           ? response.member == request.member
           : response.member != request.member)) {
    return unexpected(Error::Code::ConfigInvalid, "control response does not answer request");
  }
  return {};
}
}  // namespace tensorplate::ipc
