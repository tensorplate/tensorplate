// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F05: Python/PyTorch sidecar adapter implementation.

#include "python_pytorch_session.hpp"

#include <cerrno>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <memory>
#include <nlohmann/json.hpp>
#include <optional>
#include <span>
#include <string>
#include <string_view>
#include <system_error>
#include <utility>
#include <vector>

#include "tensorplate/backend/capability.hpp"
#include "tensorplate/backend/registry.hpp"
#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/buffer/cleanup.hpp"
#include "tensorplate/buffer/output.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/core/infer_result.hpp"
#include "tensorplate/core/model_spec.hpp"
#include "tensorplate/core/result.hpp"
#include "tensorplate/ipc/sidecar_codec.hpp"
#include "tensorplate/ipc/unix_socket.hpp"

#include "job_channel.hpp"
#include "job_wire.hpp"
#include "runner_profile.hpp"
#include "sidecar_frame_io.hpp"
#include "sidecar_process.hpp"

namespace tensorplate::adapters::python_pytorch {

namespace {

using json = nlohmann::json;

constexpr std::string_view kSchemaVersion = "0.1";

// Message kinds the adapter sends to and accepts from the sidecar.
// InferAsync / Cancel / error_event are defined by the protocol schema
// but not yet driven by the C++ ExecutionSession adapter; the scheduler
// (V01-E06) and the agent's worker supervision (V01-E09) will issue
// them once their completion/cancellation wiring lands.
constexpr std::string_view kKindLoadModel = "load_model";
constexpr std::string_view kKindLoadModelResponse = "load_model_response";
constexpr std::string_view kKindPrime = "prime";
constexpr std::string_view kKindPrimeResponse = "prime_response";
constexpr std::string_view kKindInfer = "infer";
constexpr std::string_view kKindInferResponse = "infer_response";
constexpr std::string_view kKindUnload = "unload";
constexpr std::string_view kKindUnloadResponse = "unload_response";
constexpr std::string_view kKindHealthCheck = "health_check";
constexpr std::string_view kKindHealthCheckResponse = "health_check_response";
constexpr std::string_view kKindReadyEvent = "ready_event";

std::string configured_python_exe() {
  for (const char* name :
       {"TP_PYTHON_PYTORCH_EXECUTABLE", "TP_TEST_PYTHON_EXE", "TP_TEST_PYTHON"}) {
    const char* value = std::getenv(name);
    if (value != nullptr && *value != '\0') {
      return value;
    }
  }
  return "python3";
}

std::chrono::milliseconds configured_duration_ms(const char* env_name,
                                                 std::chrono::milliseconds fallback) {
  const char* value = std::getenv(env_name);
  if (value == nullptr || *value == '\0') {
    return fallback;
  }
  char* end = nullptr;
  errno = 0;
  const long long parsed = std::strtoll(value, &end, 10);
  if (errno != 0 || end == value || *end != '\0' || parsed <= 0) {
    return fallback;
  }
  return std::chrono::milliseconds{parsed};
}

Error::Code error_code_from_wire(const std::string& wire) {
  if (auto c = error_code_from_string(wire); c.has_value()) {
    return *c;
  }
  return Error::Code::Internal;
}

std::optional<std::string> string_field(const json& obj, const char* name) {
  if (!obj.is_object()) {
    return std::nullopt;
  }
  auto it = obj.find(name);
  if (it == obj.end() || !it->is_string()) {
    return std::nullopt;
  }
  return it->get<std::string>();
}

std::optional<bool> bool_field(const json& obj, const char* name) {
  if (!obj.is_object()) {
    return std::nullopt;
  }
  auto it = obj.find(name);
  if (it == obj.end() || !it->is_boolean()) {
    return std::nullopt;
  }
  return it->get<bool>();
}

std::optional<std::size_t> size_field(const json& obj, const char* name) {
  if (!obj.is_object()) {
    return std::nullopt;
  }
  auto it = obj.find(name);
  if (it == obj.end() || !it->is_number_unsigned()) {
    return std::nullopt;
  }
  return it->get<std::size_t>();
}

ipc::UnixSocket::TimePoint deadline_from_now(std::chrono::milliseconds dur) {
  return ipc::UnixSocket::Clock::now() + dur;
}

ipc::UnixSocket::TimePoint clamped_deadline(const InferRequest& req,
                                            std::chrono::milliseconds ceiling) {
  const auto fallback = deadline_from_now(ceiling);
  if (const auto& d = req.deadline(); d.has_value()) {
    const auto dl = ipc::UnixSocket::TimePoint(d->time_since_epoch());
    return dl < fallback ? dl : fallback;
  }
  return fallback;
}

[[nodiscard]] Error error_from_response(const json& header, Error::Code default_code) {
  auto err_it = header.find("error");
  if (err_it == header.end() || !err_it->is_object()) {
    return Error::make(default_code, "sidecar returned error without error envelope");
  }
  const auto& err = *err_it;
  const std::string code_str = string_field(err, "code").value_or("internal");
  const std::string message = string_field(err, "message").value_or("sidecar error");
  std::optional<std::string> context = string_field(err, "context");
  return Error{error_code_from_wire(code_str), message, std::move(context)};
}

[[nodiscard]] json tensor_view_to_json(const TensorView& view) {
  json out;
  out["dtype"] = std::string(to_string(view.dtype()));
  out["shape"] = view.shape();
  if (view.layout() != Layout::RowMajor) {
    out["layout"] = std::string(to_string(view.layout()));
  }
  out["byte_offset"] = view.byte_offset();
  out["byte_size"] = view.byte_size();
  return out;
}

[[nodiscard]] Result<TensorView> tensor_view_from_json(const json& obj) {
  if (!obj.is_object()) {
    return unexpected(Error::Code::InferenceFailed, "tensor metadata is not a JSON object");
  }
  auto dtype_str = string_field(obj, "dtype");
  auto shape_it = obj.find("shape");
  if (!dtype_str.has_value() || shape_it == obj.end() || !shape_it->is_array()) {
    return unexpected(Error::Code::InferenceFailed,
                      "tensor metadata missing required fields (dtype, shape)");
  }
  auto dtype = dtype_from_string(*dtype_str);
  if (!dtype.has_value()) {
    return unexpected(Error::Code::InferenceFailed,
                      "tensor metadata has unknown dtype " + *dtype_str);
  }
  std::vector<std::int64_t> shape;
  shape.reserve(shape_it->size());
  for (const auto& dim : *shape_it) {
    if (!dim.is_number_integer()) {
      return unexpected(Error::Code::InferenceFailed,
                        "tensor metadata shape contains a non-integer dimension");
    }
    shape.push_back(dim.get<std::int64_t>());
  }
  return TensorView::create(*dtype, std::move(shape));
}

[[nodiscard]] Unexpected buffer_manager_missing() {
  return unexpected(Error::Code::ConfigInvalid,
                    "python_pytorch adapter requires a BufferManager hook");
}

[[nodiscard]] json model_spec_to_json(const ModelSpec& spec) {
  json out;
  out["schema_version"] = std::string(kSchemaVersion);
  out["model_id"] = spec.model_id();
  out["model_class"] = std::string(to_string(spec.model_class()));
  out["artifact_path"] = spec.artifact_path();
  out["backend_hint"] = spec.backend_hint();
  out["precision_hint"] = std::string(to_string(spec.precision_hint()));
  if (spec.profile_id().has_value()) {
    out["profile_id"] = *spec.profile_id();
  }
  return out;
}

}  // namespace

BackendCapability make_python_pytorch_capability() {
  std::vector<PrecisionHint> precision = {PrecisionHint::Auto, PrecisionHint::Fp32,
                                          PrecisionHint::Fp16, PrecisionHint::BFloat16};
  std::vector<std::string> notes = {
      "managed Python subprocess (one per execution session)",
      "required path for SmolVLA-class VLA bundles",
      "Unix-domain socket IPC; not an HTTP service",
  };
  return BackendCapability::create(std::string(kBackendName), std::move(precision),
                                   /*shape_support=*/ShapeSupport::Dynamic,
                                   /*profile_id=*/std::nullopt,
                                   /*supports_async=*/false,
                                   /*supports_generation=*/false,
                                   /*supports_streaming=*/false,
                                   /*supports_kv_cache=*/false,
                                   /*op_coverage_score_pct=*/std::nullopt,
                                   /*memory_estimate_bytes=*/std::nullopt,
                                   /*memory_limit_bytes=*/std::nullopt,
                                   /*target_compatibility_notes=*/std::move(notes))
      .value();
}

class PythonPytorchSession final : public ExecutionSession {
 public:
  PythonPytorchSession(ExecutionSessionRuntimeHooks hooks, PythonPytorchConfig config,
                       SidecarLauncher launcher, std::shared_ptr<JobChannel> channel)
      : ExecutionSession(hooks),
        config_(std::move(config)),
        launcher_(std::move(launcher)),
        manager_(hooks.buffer_manager),
        channel_(std::move(channel)) {}

  ~PythonPytorchSession() override { stop_sidecar(kSessionUnloaded); }

  PythonPytorchSession(const PythonPytorchSession&) = delete;
  PythonPytorchSession& operator=(const PythonPytorchSession&) = delete;
  PythonPytorchSession(PythonPytorchSession&&) = delete;
  PythonPytorchSession& operator=(PythonPytorchSession&&) = delete;

  [[nodiscard]] std::string_view backend_name() const noexcept override { return kBackendName; }

 protected:
  Result<void> do_load(const ModelSpec& spec) override {
    if (process_) {
      return unexpected(Error::Code::Internal, "python_pytorch session already loaded");
    }
    if (config_.speech_jobs && manager_ == nullptr) {
      // The channel would refuse every job for good.
      return buffer_manager_missing();
    }
    auto req = launch_request(spec);
    if (!req.has_value()) {
      return unexpected(req.error());
    }
    auto proc_r =
        SidecarProcess::start(req.value(), launcher_, deadline_from_now(config_.startup_timeout));
    if (!proc_r.has_value()) {
      return unexpected(proc_r.error());
    }
    process_ = std::move(proc_r).value();

    // Read the ready_event the runner emits on connect.
    auto ready = read_frame(process_->socket(), deadline_from_now(config_.startup_timeout));
    if (!ready.has_value()) {
      return load_failure(ready.error());
    }
    auto ready_hdr = json::parse(ready.value().json_header, nullptr, /*allow_exceptions=*/false);
    if (ready_hdr.is_discarded() ||
        string_field(ready_hdr, "schema_version").value_or("") != std::string(kSchemaVersion) ||
        string_field(ready_hdr, "kind").value_or("") != std::string(kKindReadyEvent)) {
      return load_failure(
          Error::make(Error::Code::LoadFailed, "sidecar did not emit ready_event on connect"));
    }
    if (config_.speech_jobs) {
      // Enabled only where the sidecar lists it: an older one refuses the field.
      const auto offered = offers_speech_jobs(ready.value().json_header);
      if (!offered.has_value()) {
        return load_failure(offered.error());
      }
      if (!offered.value()) {
        return load_failure(Error::make(Error::Code::Unsupported,
                                        "sidecar does not offer speech jobs",
                                        "speech_jobs_not_offered"));
      }
    }

    json req_hdr;
    req_hdr["schema_version"] = std::string(kSchemaVersion);
    req_hdr["message_id"] = next_message_id();
    req_hdr["kind"] = std::string(kKindLoadModel);
    req_hdr["model_spec"] = model_spec_to_json(spec);
    if (config_.speech_jobs) {
      req_hdr["capabilities"] = json::array({std::string(kSpeechJobsCapability)});
    }

    auto resp = exchange(req_hdr, /*payload=*/{}, deadline_from_now(config_.startup_timeout),
                         std::string(kKindLoadModelResponse), Error::Code::LoadFailed);
    if (!resp.has_value()) {
      return load_failure(resp.error());
    }
    if (config_.speech_jobs) {
      const auto classes = declared_job_classes(resp.value().dump());
      if (!classes.has_value()) {
        return load_failure(classes.error());
      }
      // From here on the channel's reader owns the socket.
      if (auto started = channel_->start(process_->socket(), classes.value());
          !started.has_value()) {
        return load_failure(started.error());
      }
      jobs_started_ = true;
    }
    return Result<void>{};
  }

  Result<void> do_prime() override {
    if (!process_) {
      return unexpected(Error::Code::NotReady, "sidecar not started");
    }
    json hdr;
    hdr["schema_version"] = std::string(kSchemaVersion);
    hdr["message_id"] = next_message_id();
    hdr["kind"] = std::string(kKindPrime);
    auto resp = exchange(hdr, {}, deadline_from_now(config_.startup_timeout),
                         std::string(kKindPrimeResponse), Error::Code::LoadFailed);
    if (!resp.has_value()) {
      return unexpected(resp.error());
    }
    auto health = verify_sidecar_health();
    if (!health.has_value()) {
      return unexpected(health.error());
    }
    channel_->admit_jobs();
    return Result<void>{};
  }

  Result<std::vector<NamedOutput>> do_infer(const InferRequest& request) override {
    if (config_.speech_jobs) {
      // The one exchange slot stays free for prime, health and unload.
      return unexpected(Error::make(Error::Code::Unsupported,
                                    "a session that runs speech jobs takes no tensor request",
                                    "tensor_inference_unsupported"));
    }
    return run_infer(request);
  }

  Result<void> do_unload() override {
    if (!process_) {
      return Result<void>{};
    }
    // The sidecar fails the jobs that wait when the unload arrives.
    channel_->close_admission();
    json hdr;
    hdr["schema_version"] = std::string(kSchemaVersion);
    hdr["message_id"] = next_message_id();
    hdr["kind"] = std::string(kKindUnload);
    auto resp = exchange(hdr, {}, deadline_from_now(config_.startup_timeout),
                         std::string(kKindUnloadResponse), Error::Code::InferenceFailed);
    stop_sidecar(kSessionUnloaded);
    if (!resp.has_value()) {
      return unexpected(resp.error());
    }
    return Result<void>{};
  }

 private:
  // A deployment that selects a runner profile runs where the descriptor
  // says or not at all: `config_.python_exe` and PATH are not a fallback.
  Result<SidecarLaunchRequest> launch_request(const ModelSpec& spec) const {
    const std::optional<std::string>& runner_profile = spec.runner_profile();
    if (!runner_profile.has_value()) {
      SidecarLaunchRequest req;
      req.python_exe = config_.python_exe;
      return req;
    }
    std::filesystem::path descriptor = config_.descriptor_path;
    if (descriptor.empty()) {
      auto installed = installed_descriptor_path();
      if (!installed.has_value()) {
        return unexpected(installed.error());
      }
      descriptor = std::move(installed).value();
    }
    auto environment = read_runner_environment(descriptor, *runner_profile);
    if (!environment.has_value()) {
      return unexpected(environment.error());
    }
    std::error_code ec;
    auto temp_dir = std::filesystem::temp_directory_path(ec);
    return runner_launch_request(environment.value(), ec ? std::filesystem::path{} : temp_dir);
  }

  Result<json> exchange(const json& request_header, const std::vector<std::byte>& payload,
                        ipc::UnixSocket::TimePoint deadline, const std::string& expected_kind,
                        Error::Code default_error_code) {
    if (!process_) {
      return unexpected(Error::Code::NotReady, "sidecar not started");
    }
    if (!process_->is_alive()) {
      return transport_failure(Error::make(Error::Code::InferenceFailed, "sidecar process exited"));
    }
    ipc::SidecarFrame frame;
    frame.json_header = request_header.dump();
    frame.payload = payload;
    const std::string message_id = string_field(request_header, "message_id").value_or("");
    // Once the channel is started its reader owns the socket.
    auto resp = jobs_started_ ? channel_->exchange(frame, message_id, deadline)
                              : exchange_on_socket(frame, deadline);
    if (!resp.has_value()) {
      // A timeout among them: the slot does not tell a late answer from the next one.
      return transport_failure(resp.error());
    }
    auto hdr = json::parse(resp.value().json_header, nullptr, false);
    if (hdr.is_discarded()) {
      return transport_failure(
          Error::make(Error::Code::InferenceFailed, "sidecar returned malformed JSON header"));
    }
    // The slot hands over the next frame that is not a job message.
    if (jobs_started_ && string_field(hdr, "message_id").value_or("") != message_id) {
      return transport_failure(
          Error::make(Error::Code::InferenceFailed, "sidecar response answers another request"));
    }
    if (string_field(hdr, "schema_version").value_or("") != std::string(kSchemaVersion)) {
      return transport_failure(
          Error::make(Error::Code::InferenceFailed, "sidecar response schema_version mismatch"));
    }
    const std::string status = string_field(hdr, "status").value_or("ok");
    if (status == "error") {
      last_payload_.clear();
      return unexpected(error_from_response(hdr, default_error_code));
    }
    if (status != "ok") {
      return transport_failure(
          Error::make(Error::Code::InferenceFailed, "sidecar response status is not ok/error"));
    }
    if (!expected_kind.empty() && string_field(hdr, "kind").value_or("") != expected_kind) {
      return transport_failure(
          Error::make(Error::Code::InferenceFailed,
                      "sidecar response kind mismatch (expected " + expected_kind + ")"));
    }
    // Attach payload to header object so callers can read it via the
    // json `__payload__` extension key; we intentionally do not embed
    // payload bytes in the JSON. Instead we pack them as a second
    // return value via a side channel: we re-use `hdr["__payload__"]`
    // *only* in this in-process exchange function call.
    if (!resp.value().payload.empty()) {
      hdr["__payload_size__"] = resp.value().payload.size();
    }
    last_payload_ = std::move(resp.value().payload);
    return hdr;
  }

  Result<ipc::SidecarFrame> exchange_on_socket(const ipc::SidecarFrame& frame,
                                               ipc::UnixSocket::TimePoint deadline) {
    if (auto w = write_frame(process_->socket(), frame, deadline); !w.has_value()) {
      return unexpected(w.error());
    }
    return read_frame(process_->socket(), deadline);
  }

  Result<json> transport_failure(Error error) {
    stop_sidecar(kBackendReset);
    return unexpected(std::move(error));
  }

  Result<void> load_failure(Error error) {
    stop_sidecar(kBackendReset);
    return unexpected(std::move(error));
  }

  // Ends the connection and the process. The channel's threads leave the
  // socket before the process object closes it; a channel that was not
  // started only runs the reap. `reason` is what open jobs fail with.
  void stop_sidecar(std::string_view reason) {
    channel_->stop(reason, [this] {
      if (process_) {
        process_->shutdown();
      }
    });
    process_.reset();
    jobs_started_ = false;
    last_payload_.clear();
  }

  Result<void> verify_sidecar_health() {
    if (!process_) {
      return unexpected(Error::Code::NotReady, "sidecar not started");
    }
    json hdr;
    hdr["schema_version"] = std::string(kSchemaVersion);
    hdr["message_id"] = next_message_id();
    hdr["kind"] = std::string(kKindHealthCheck);
    auto resp = exchange(hdr, {}, deadline_from_now(config_.health_timeout),
                         std::string(kKindHealthCheckResponse), Error::Code::InferenceFailed);
    if (!resp.has_value()) {
      return unexpected(resp.error());
    }
    auto health_it = resp.value().find("health");
    if (health_it == resp.value().end() || !health_it->is_object()) {
      return unexpected(Error::Code::InferenceFailed,
                        "sidecar health_check_response missing health payload");
    }
    auto ready = bool_field(*health_it, "ready");
    if (!ready.has_value()) {
      return unexpected(Error::Code::InferenceFailed,
                        "sidecar health payload missing boolean ready field");
    }
    if (!*ready) {
      return unexpected(Error::Code::NotReady, "sidecar health check reported not ready");
    }
    return Result<void>{};
  }

  Result<void> pack_request(const InferRequest& request, json& hdr,
                            std::vector<std::byte>& payload) {
    hdr["schema_version"] = std::string(kSchemaVersion);
    hdr["message_id"] = next_message_id();
    hdr["kind"] = std::string(kKindInfer);
    hdr["correlation_id"] = request.request_id();
    json tensors_in = json::array();
    for (const auto& in : request.inputs()) {
      auto view = manager_->view(in.buffer, in.tensor);
      if (!view.has_value()) {
        return unexpected(view.error());
      }
      const std::size_t offset = payload.size();
      payload.insert(payload.end(), view.value().begin(), view.value().end());
      json entry;
      entry["name"] = in.name;
      entry["tensor"] = tensor_view_to_json(in.tensor);
      entry["payload_offset"] = offset;
      entry["payload_length"] = view.value().size();
      tensors_in.push_back(std::move(entry));
    }
    hdr["tensors"] = std::move(tensors_in);
    return Result<void>{};
  }

  Result<NamedOutput> decode_one_output(const json& entry) {
    if (!entry.is_object()) {
      return unexpected(Error::Code::InferenceFailed, "sidecar tensor entry is not a JSON object");
    }
    auto name = string_field(entry, "name");
    auto offset = size_field(entry, "payload_offset");
    auto length = size_field(entry, "payload_length");
    auto tensor_it = entry.find("tensor");
    if (!name.has_value() || name->empty() || !offset.has_value() || !length.has_value() ||
        tensor_it == entry.end()) {
      return unexpected(Error::Code::InferenceFailed,
                        "sidecar tensor entry missing required metadata");
    }
    if (*length == 0) {
      return unexpected(Error::Code::InferenceFailed,
                        "sidecar tensor payload_length must be non-zero");
    }
    if (*offset > last_payload_.size() || *length > last_payload_.size() - *offset) {
      return unexpected(Error::Code::InferenceFailed, "sidecar tensor payload window out of range");
    }
    auto tv = tensor_view_from_json(*tensor_it);
    if (!tv.has_value()) {
      return unexpected(tv.error());
    }

    auto buf_r = manager_->allocate(*length);
    if (!buf_r.has_value()) {
      return unexpected(buf_r.error());
    }
    auto buffer = buf_r.value();
    auto dst = manager_->data(buffer);
    if (!dst.has_value()) {
      // Buffer allocated but unwritable; let the caller release it.
      (void)manager_->release_if_owned(buffer);
      return unexpected(dst.error());
    }
    std::memcpy(dst.value().data(), last_payload_.data() + *offset, *length);
    return NamedOutput{std::move(*name), buffer, std::move(tv).value(), std::nullopt};
  }

  Result<std::vector<NamedOutput>> decode_response(const json& response_header) {
    std::vector<NamedOutput> outputs;
    auto fail = [&](Error error) -> Result<std::vector<NamedOutput>> {
      (void)release_partial_outputs(*manager_, outputs);
      return unexpected(std::move(error));
    };
    auto tensors_it = response_header.find("tensors");
    if (tensors_it == response_header.end() || !tensors_it->is_array()) {
      return fail(
          Error::make(Error::Code::InferenceFailed, "sidecar infer response missing tensors[]"));
    }
    for (const auto& entry : *tensors_it) {
      auto out_r = decode_one_output(entry);
      if (!out_r.has_value()) {
        return fail(out_r.error());
      }
      outputs.push_back(std::move(out_r).value());
    }
    return outputs;
  }

  Result<std::vector<NamedOutput>> run_infer(const InferRequest& request) {
    if (!process_) {
      return unexpected(Error::Code::NotReady, "sidecar not started");
    }
    if (manager_ == nullptr) {
      return buffer_manager_missing();
    }

    json hdr;
    std::vector<std::byte> payload;
    payload.reserve(1024);
    if (auto r = pack_request(request, hdr, payload); !r.has_value()) {
      return unexpected(r.error());
    }

    const auto deadline = clamped_deadline(request, config_.infer_timeout);
    const std::string expected_kind = std::string(kKindInferResponse);
    auto resp = exchange(hdr, payload, deadline, expected_kind, Error::Code::InferenceFailed);
    if (!resp.has_value()) {
      return unexpected(resp.error());
    }
    return decode_response(resp.value());
  }

  PythonPytorchConfig config_;
  SidecarLauncher launcher_;
  BufferManager* manager_ = nullptr;
  std::shared_ptr<JobChannel> channel_;
  std::unique_ptr<SidecarProcess> process_;
  /// The channel's reader owns the socket: exchanges go through its slot.
  bool jobs_started_ = false;
  std::vector<std::byte> last_payload_;
};

SessionWithJobChannel make_python_pytorch_session_with_channel(ExecutionSessionRuntimeHooks hooks,
                                                               PythonPytorchConfig config,
                                                               SidecarLauncher launcher) {
  auto channel = std::make_shared<JobChannel>(hooks.buffer_manager, config.write_timeout);
  std::unique_ptr<ExecutionSession> session(
      new PythonPytorchSession(hooks, std::move(config), std::move(launcher), channel));
  return SessionWithJobChannel{std::move(session), std::move(channel)};
}

std::unique_ptr<ExecutionSession> make_python_pytorch_session(ExecutionSessionRuntimeHooks hooks,
                                                              PythonPytorchConfig config,
                                                              SidecarLauncher launcher) {
  return make_python_pytorch_session_with_channel(hooks, std::move(config), std::move(launcher))
      .session;
}

}  // namespace tensorplate::adapters::python_pytorch

namespace tensorplate {

Result<void> register_python_pytorch_backend(BackendRegistry& registry) {
  using adapters::python_pytorch::default_fork_exec_launcher;
  using adapters::python_pytorch::kBackendName;
  using adapters::python_pytorch::make_python_pytorch_capability;
  using adapters::python_pytorch::make_python_pytorch_session;
  using adapters::python_pytorch::PythonPytorchConfig;
  return registry.register_backend(BackendEntry{
      std::string(kBackendName),
      make_python_pytorch_capability(),
      [](ExecutionSessionRuntimeHooks hooks) -> Result<std::unique_ptr<ExecutionSession>> {
        PythonPytorchConfig config;
        config.python_exe = adapters::python_pytorch::configured_python_exe();
        config.startup_timeout = adapters::python_pytorch::configured_duration_ms(
            "TP_PYTHON_PYTORCH_STARTUP_TIMEOUT_MS", config.startup_timeout);
        config.infer_timeout = adapters::python_pytorch::configured_duration_ms(
            "TP_PYTHON_PYTORCH_INFER_TIMEOUT_MS", config.infer_timeout);
        config.health_timeout = adapters::python_pytorch::configured_duration_ms(
            "TP_PYTHON_PYTORCH_HEALTH_TIMEOUT_MS", config.health_timeout);
        return make_python_pytorch_session(hooks, std::move(config), default_fork_exec_launcher());
      },
  });
}

}  // namespace tensorplate
