// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F05: Python/PyTorch sidecar adapter implementation.

#include "python_pytorch_session.hpp"

#include <atomic>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <memory>
#include <nlohmann/json.hpp>
#include <optional>
#include <span>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include "tensorplate/backend/capability.hpp"
#include "tensorplate/backend/registry.hpp"
#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/buffer_ref.hpp"
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

#include "sidecar_process.hpp"

namespace tensorplate::adapters::python_pytorch {

namespace {

using json = nlohmann::json;

constexpr std::string_view kSchemaVersion = "0.1";

// Message kinds the adapter sends to and accepts from the sidecar.
// InferAsync / Cancel / health_check / error_event are defined by the
// protocol schema but not yet driven by the C++ ExecutionSession adapter;
// the scheduler (V01-E06) and the agent's worker supervision (V01-E09)
// will issue them once their completion/cancellation wiring lands.
constexpr std::string_view kKindLoadModel = "load_model";
constexpr std::string_view kKindLoadModelResponse = "load_model_response";
constexpr std::string_view kKindPrime = "prime";
constexpr std::string_view kKindPrimeResponse = "prime_response";
constexpr std::string_view kKindInfer = "infer";
constexpr std::string_view kKindInferResponse = "infer_response";
constexpr std::string_view kKindUnload = "unload";
constexpr std::string_view kKindUnloadResponse = "unload_response";
constexpr std::string_view kKindReadyEvent = "ready_event";

std::atomic<std::uint64_t>& message_seq() noexcept {
  static std::atomic<std::uint64_t> seq{0};
  return seq;
}

std::string next_message_id() {
  return "tp-msg-" + std::to_string(message_seq().fetch_add(1, std::memory_order_relaxed));
}

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

Error::Code error_code_from_wire(const std::string& wire) {
  if (auto c = error_code_from_string(wire); c.has_value()) {
    return *c;
  }
  return Error::Code::Internal;
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

[[nodiscard]] Result<void> write_frame(ipc::UnixSocket& sock, const ipc::SidecarFrame& frame,
                                       ipc::UnixSocket::TimePoint deadline) {
  auto enc = ipc::encode_frame(frame);
  if (!enc.has_value()) {
    return unexpected(enc.error());
  }
  return sock.write_all(std::span<const std::byte>(enc.value()), deadline);
}

[[nodiscard]] Result<ipc::SidecarFrame> read_frame(ipc::UnixSocket& sock,
                                                   ipc::UnixSocket::TimePoint deadline) {
  std::vector<std::byte> buf(16);
  if (auto r = sock.read_exact(std::span<std::byte>(buf.data(), buf.size()), deadline);
      !r.has_value()) {
    return unexpected(r.error());
  }
  const auto* p = buf.data();
  auto u32 = [&](std::size_t off) {
    return (static_cast<std::uint32_t>(p[off]) << 24) |
           (static_cast<std::uint32_t>(p[off + 1]) << 16) |
           (static_cast<std::uint32_t>(p[off + 2]) << 8) | static_cast<std::uint32_t>(p[off + 3]);
  };
  if (u32(0) != ipc::kFrameMagic) {
    return unexpected(Error::Code::InferenceFailed, "sidecar frame magic mismatch");
  }
  if (u32(4) != ipc::kProtocolWireVersion) {
    return unexpected(Error::Code::InferenceFailed,
                      "sidecar wire version " + std::to_string(u32(4)) + " unsupported");
  }
  const std::uint32_t hdr_len = u32(8);
  const std::uint32_t pld_len = u32(12);
  if (hdr_len > ipc::kMaxHeaderLen || pld_len > ipc::kMaxPayloadLen) {
    return unexpected(Error::Code::InferenceFailed, "sidecar frame size exceeds limit");
  }
  buf.resize(16 + hdr_len + pld_len);
  if (hdr_len + pld_len > 0) {
    if (auto r =
            sock.read_exact(std::span<std::byte>(buf.data() + 16, hdr_len + pld_len), deadline);
        !r.has_value()) {
      return unexpected(r.error());
    }
  }
  return ipc::decode_frame(buf, nullptr);
}

[[nodiscard]] Error error_from_response(const json& header, Error::Code default_code) {
  if (!header.contains("error")) {
    return Error::make(default_code, "sidecar returned error without error envelope");
  }
  const auto& err = header.at("error");
  const std::string code_str = err.value("code", "internal");
  const std::string message = err.value("message", "sidecar error");
  std::optional<std::string> context;
  if (err.contains("context")) {
    context = err.at("context").get<std::string>();
  }
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
  if (!obj.contains("dtype") || !obj.contains("shape")) {
    return unexpected(Error::Code::InferenceFailed,
                      "tensor metadata missing required fields (dtype, shape)");
  }
  auto dtype = dtype_from_string(obj.at("dtype").get<std::string>());
  if (!dtype.has_value()) {
    return unexpected(Error::Code::InferenceFailed,
                      "tensor metadata has unknown dtype " + obj.at("dtype").get<std::string>());
  }
  std::vector<std::int64_t> shape = obj.at("shape").get<std::vector<std::int64_t>>();
  return TensorView::create(*dtype, std::move(shape));
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
                       SidecarLauncher launcher)
      : ExecutionSession(hooks),
        config_(std::move(config)),
        launcher_(std::move(launcher)),
        manager_(hooks.buffer_manager) {}

  [[nodiscard]] std::string_view backend_name() const noexcept override { return kBackendName; }

 protected:
  Result<void> do_load(const ModelSpec& spec) override {
    if (process_) {
      return unexpected(Error::Code::Internal, "python_pytorch session already loaded");
    }
    SidecarLaunchRequest req;
    req.python_exe = config_.python_exe;
    auto proc_r = SidecarProcess::start(req, launcher_, deadline_from_now(config_.startup_timeout));
    if (!proc_r.has_value()) {
      return unexpected(proc_r.error());
    }
    process_ = std::move(proc_r).value();

    // Read the ready_event the runner emits on connect.
    auto ready = read_frame(process_->socket(), deadline_from_now(config_.startup_timeout));
    if (!ready.has_value()) {
      process_->shutdown();
      process_.reset();
      return unexpected(ready.error());
    }
    auto ready_hdr = json::parse(ready.value().json_header, nullptr, /*allow_exceptions=*/false);
    if (ready_hdr.is_discarded() || ready_hdr.value("kind", "") != std::string(kKindReadyEvent)) {
      process_->shutdown();
      process_.reset();
      return unexpected(Error::Code::LoadFailed, "sidecar did not emit ready_event on connect");
    }

    json req_hdr;
    req_hdr["schema_version"] = std::string(kSchemaVersion);
    req_hdr["message_id"] = next_message_id();
    req_hdr["kind"] = std::string(kKindLoadModel);
    req_hdr["model_spec"] = model_spec_to_json(spec);

    auto resp = exchange(req_hdr, /*payload=*/{}, deadline_from_now(config_.startup_timeout),
                         std::string(kKindLoadModelResponse), Error::Code::LoadFailed);
    if (!resp.has_value()) {
      shutdown_sidecar();
      return unexpected(resp.error());
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
    return Result<void>{};
  }

  Result<std::vector<NamedOutput>> do_infer(const InferRequest& request) override {
    return run_infer(request);
  }

  Result<void> do_unload() override {
    if (!process_) {
      return Result<void>{};
    }
    json hdr;
    hdr["schema_version"] = std::string(kSchemaVersion);
    hdr["message_id"] = next_message_id();
    hdr["kind"] = std::string(kKindUnload);
    auto resp = exchange(hdr, {}, deadline_from_now(config_.startup_timeout),
                         std::string(kKindUnloadResponse), Error::Code::InferenceFailed);
    shutdown_sidecar();
    if (!resp.has_value()) {
      return unexpected(resp.error());
    }
    return Result<void>{};
  }

 private:
  Result<json> exchange(const json& request_header, const std::vector<std::byte>& payload,
                        ipc::UnixSocket::TimePoint deadline, const std::string& expected_kind,
                        Error::Code default_error_code) {
    if (!process_) {
      return unexpected(Error::Code::NotReady, "sidecar not started");
    }
    ipc::SidecarFrame frame;
    frame.json_header = request_header.dump();
    frame.payload = payload;
    if (auto w = write_frame(process_->socket(), frame, deadline); !w.has_value()) {
      return transport_failure(w.error());
    }
    auto resp = read_frame(process_->socket(), deadline);
    if (!resp.has_value()) {
      return transport_failure(resp.error());
    }
    auto hdr = json::parse(resp.value().json_header, nullptr, false);
    if (hdr.is_discarded()) {
      return transport_failure(
          Error::make(Error::Code::InferenceFailed, "sidecar returned malformed JSON header"));
    }
    const std::string status = hdr.value("status", "ok");
    if (status == "error") {
      return unexpected(error_from_response(hdr, default_error_code));
    }
    if (!expected_kind.empty() && hdr.value("kind", "") != expected_kind) {
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

  Result<json> transport_failure(Error error) {
    shutdown_sidecar();
    return unexpected(std::move(error));
  }

  void shutdown_sidecar() noexcept {
    if (process_) {
      process_->shutdown();
      process_.reset();
    }
    last_payload_.clear();
  }

  Result<std::vector<NamedOutput>> run_infer(const InferRequest& request) {
    if (!process_) {
      return unexpected(Error::Code::NotReady, "sidecar not started");
    }

    // Pack inputs.
    json hdr;
    hdr["schema_version"] = std::string(kSchemaVersion);
    hdr["message_id"] = next_message_id();
    hdr["kind"] = std::string(kKindInfer);
    hdr["correlation_id"] = request.request_id();
    json tensors_in = json::array();
    std::vector<std::byte> payload;
    payload.reserve(1024);
    if (manager_ == nullptr) {
      return unexpected(Error::Code::ConfigInvalid,
                        "python_pytorch adapter requires a BufferManager hook");
    }
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

    const auto deadline = clamped_deadline(request, config_.infer_timeout);
    const std::string expected_kind = std::string(kKindInferResponse);
    auto resp = exchange(hdr, payload, deadline, expected_kind, Error::Code::InferenceFailed);
    if (!resp.has_value()) {
      // Release the request buffers in line with V01-E03 contract; the
      // host owns request buffers, so we do nothing here, but if the
      // sidecar reports timeout we ensure the process is still alive
      // for the next attempt.
      return unexpected(resp.error());
    }

    // Parse outputs.
    std::vector<NamedOutput> outputs;
    if (!resp.value().contains("tensors")) {
      return unexpected(Error::Code::InferenceFailed, "sidecar infer response missing tensors[]");
    }
    for (const auto& entry : resp.value().at("tensors")) {
      const auto offset = entry.value("payload_offset", 0U);
      const auto length = entry.value("payload_length", 0U);
      if (offset + length > last_payload_.size()) {
        return unexpected(Error::Code::InferenceFailed,
                          "sidecar tensor payload window out of range");
      }
      auto tv = tensor_view_from_json(entry.at("tensor"));
      if (!tv.has_value()) {
        return unexpected(tv.error());
      }

      auto buf_r = manager_->allocate(length);
      if (!buf_r.has_value()) {
        return unexpected(buf_r.error());
      }
      auto buffer = buf_r.value();
      auto dst = manager_->data(buffer);
      if (!dst.has_value()) {
        return unexpected(dst.error());
      }
      std::memcpy(dst.value().data(), last_payload_.data() + offset, length);

      outputs.push_back(NamedOutput{entry.value("name", std::string{}), buffer,
                                    std::move(tv).value(), std::nullopt});
    }
    return outputs;
  }

  PythonPytorchConfig config_;
  SidecarLauncher launcher_;
  BufferManager* manager_ = nullptr;
  std::unique_ptr<SidecarProcess> process_;
  std::vector<std::byte> last_payload_;
};

}  // namespace tensorplate::adapters::python_pytorch

namespace tensorplate {

Result<void> register_python_pytorch_backend(BackendRegistry& registry) {
  using adapters::python_pytorch::default_fork_exec_launcher;
  using adapters::python_pytorch::kBackendName;
  using adapters::python_pytorch::make_python_pytorch_capability;
  using adapters::python_pytorch::PythonPytorchConfig;
  using adapters::python_pytorch::PythonPytorchSession;
  return registry.register_backend(BackendEntry{
      std::string(kBackendName),
      make_python_pytorch_capability(),
      [](ExecutionSessionRuntimeHooks hooks) -> Result<std::unique_ptr<ExecutionSession>> {
        PythonPytorchConfig config;
        config.python_exe = adapters::python_pytorch::configured_python_exe();
        return std::unique_ptr<ExecutionSession>(
            new PythonPytorchSession(hooks, std::move(config), default_fork_exec_launcher()));
      },
  });
}

}  // namespace tensorplate
