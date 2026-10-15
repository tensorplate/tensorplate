// SPDX-License-Identifier: Apache-2.0
//
// V01-E07-F01-T02: ServingWorker composition root.

#include "tensorplate/serving/worker.hpp"

#include <atomic>
#include <chrono>
#include <iostream>
#include <memory>
#include <mutex>
#include <nlohmann/json.hpp>
#include <optional>
#include <string>
#include <string_view>
#include <thread>
#include <utility>
#include <vector>

#include "tensorplate/backend/builtin.hpp"
#include "tensorplate/backend/registry.hpp"
#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/http/http_server.hpp"
#include "tensorplate/ipc/worker_control.hpp"
#include "tensorplate/scheduler/factory.hpp"
#include "tensorplate/scheduler/scheduler.hpp"
#include "tensorplate/serving/async_policy.hpp"
#include "tensorplate/serving/health.hpp"
#include "tensorplate/serving/metrics.hpp"
#include "tensorplate/serving/pipeline.hpp"
#include "tensorplate/serving/router.hpp"
#include "tensorplate/serving/shutdown.hpp"
#include "tensorplate/version.hpp"

#include "serving/control_channel.hpp"
#include "serving/drain_window.hpp"
#include "serving/member_control.hpp"
#include "serving/mock_session.hpp"
#if TP_ENABLE_STREAMING_GRPC
#include "serving/session/speech_dispatch.hpp"
#include "serving/stream/stream_listener.hpp"
#endif

namespace tensorplate {

namespace {

void log_stderr(const ServingConfig& cfg, std::string_view level, std::string_view message,
                const nlohmann::json& fields = {}) {
  if (!cfg.enable_stderr_logs) {
    return;
  }
  nlohmann::json j;
  j["ts_ns"] = std::chrono::duration_cast<std::chrono::nanoseconds>(
                   std::chrono::steady_clock::now().time_since_epoch())
                   .count();
  j["level"] = std::string{level};
  j["component"] = "serving";
  j["message"] = std::string{message};
  if (!fields.empty()) {
    j["fields"] = fields;
  }
  std::cerr << j.dump() << '\n';
}

/// When the drain window ends, on the clock and in the unit of a line's
/// `ts_ns`, and the cause its sessions end with.
nlohmann::json drain_end_fields(std::chrono::steady_clock::time_point end,
                                serving::DrainCause cause) {
  return {{"drain_ends_ns",
           std::chrono::duration_cast<std::chrono::nanoseconds>(end.time_since_epoch()).count()},
          {"cause", cause == serving::DrainCause::DeploymentRetired ? "deployment_retired"
                                                                    : "worker_shutdown"}};
}

#if TP_ENABLE_STREAMING_GRPC
/// One stream session at a time.
constexpr std::uint32_t kStreamSessions = 1;
#endif

class SchedulerHealthSink final : public SchedulerEventSink {
 public:
  SchedulerHealthSink(HealthState& health, ServingMetrics& metrics, InferScheduler*& scheduler)
      : health_(health), metrics_(metrics), scheduler_(scheduler) {}

  void on_event(const SchedulerEvent& event) override {
    // Sync metrics + health on every event. This is fire-and-forget;
    // a scheduler with a healthy fast path is the source of truth.
    if (scheduler_ != nullptr) {
      const auto m = scheduler_->metrics();
      health_.record_queue_state(m.queue_depth, m.in_flight);
      metrics_.record_scheduler_accounting(m);
    }
    if (event.kind == SchedulerEventKind::Cancelled) {
      metrics_.increment_cancelled();
    } else if (event.kind == SchedulerEventKind::Expired) {
      metrics_.increment_expired();
    }
  }

 private:
  HealthState& health_;
  ServingMetrics& metrics_;
  InferScheduler*& scheduler_;
};

}  // namespace

struct ServingWorker::Impl {
  ServingConfig config;
  BackendRegistry* registry = nullptr;
  std::unique_ptr<BackendRegistry> owned_registry;

  std::unique_ptr<BufferManager> buffer_manager;
  std::unique_ptr<ExecutionSession> session;
  std::unique_ptr<InferScheduler> scheduler;
  InferScheduler* scheduler_ptr = nullptr;
  std::unique_ptr<SchedulerHealthSink> scheduler_sink;
  HealthState health;
  ServingMetrics metrics;
  std::unique_ptr<serving::AsyncPolicyStore> async_store;
  std::unique_ptr<serving::ServingPipeline> pipeline;
  std::unique_ptr<serving::RequestRouter> router;
  std::unique_ptr<http::HttpServer> server;
  std::unique_ptr<SystemSchedulerClock> clock;
  serving::ShutdownController shutdown;
  bool async_policy_supported = true;

  std::atomic<bool> started{false};
  std::atomic<bool> stopped{false};
  std::thread dispatcher;
  std::thread evictor;
  std::atomic<bool> stop_workers{false};
  std::mutex stop_mutex;
  ServingExitCode exit_code = ServingExitCode::Ok;
  /// The speech dispatch reported a release its backend never acknowledged:
  /// the worker stops, and its stop's unload resets the backend.
  std::atomic<bool> backend_stalled{false};
#if TP_ENABLE_STREAMING_GRPC
  /// The session's job bridge and the dispatch that runs stream sessions
  /// through it; both null unless a speech deployment serves streams.
  /// Destroying the bridge detaches the dispatch from the backend's events,
  /// so stop() unloads the session before either goes, and the order here
  /// destroys the listener, the dispatch, the bridge, then `session`.
  std::shared_ptr<BoundedJobBridge> job_bridge;
  std::unique_ptr<serving::SpeechJobDispatch> speech_dispatch;
  /// Why a start of the listener with the dispatch failed. The listener
  /// stops a dispatch it has attached and a dispatch is attached once, so
  /// start() does not try again.
  std::optional<Error> stream_start_failure;
  /// Guards `stream` against a shutdown() that runs while start() sets it.
  /// Once set it stays until the worker is destroyed.
  std::mutex stream_mutex;
  std::unique_ptr<serving::StreamListener> stream;
#endif
  /// The drain of streams and of HTTP requests share it; it begins with
  /// whichever is asked first.
  serving::DrainWindow drain_window;
  /// Held by every write of `health`, so that a retire's write never lands
  /// after a stop has moved the state on.
  std::mutex state_mu;
  // After everything a retire reaches, and before the channel, whose thread
  // stops first. The stream listener's admission check, which reads both,
  // goes with its manager in stop(), which the destructor always runs.
  serving::MemberControlTarget control_target{
      [this](std::chrono::milliseconds window, std::chrono::milliseconds asked) {
        begin_retirement(window, asked);
      }};
  std::unique_ptr<serving::ControlChannel> control;

  Result<void> attach_control();
  Result<void> create_session(const ModelSpec& model, const ExecutionSessionRuntimeHooks& hooks);
  Result<void> build();
  void publish_built_state();
  void set_serving_state(ServingState state) noexcept;
  [[nodiscard]] std::uint32_t stream_session_ceiling() const noexcept;
  Result<void> start_listener();
  Result<void> start_stream_listener();
#if TP_ENABLE_STREAMING_GRPC
  [[nodiscard]] serving::StreamListener* stream_listener() noexcept {
    const std::lock_guard guard(stream_mutex);
    return stream.get();
  }
#endif
  void stop_stream_admission() noexcept;
  void stop_stream_listener() noexcept;
  void on_speech_stall(const Error& cause) noexcept;
  void begin_retirement(std::chrono::milliseconds window, std::chrono::milliseconds asked) noexcept;
  // Dispatcher / evictor only call into the owned `unique_ptr` members, so
  // they could be marked const; we keep them non-const so future hooks
  // (e.g. recording dispatcher state on Impl) do not require an API change.
  void dispatcher_loop();  // NOLINT(readability-make-member-function-const)
  void evictor_loop();     // NOLINT(readability-make-member-function-const)
  void run_drain();
};

Result<void> ServingWorker::Impl::attach_control() {
  if (!config.deployment.generation) {
    return Result<void>{};
  }
  // fd 0 is taken only for a config that could be served.
  if (auto v = config.validate(); !v) {
    return v;
  }
  // The agent polls the channel while the model loads, so the control
  // thread is the first one this process starts.
  auto socket = serving::take_inherited_control_socket();
  if (!socket) {
    return unexpected(socket.error());
  }
  auto channel = serving::ControlChannel::start(
      std::move(socket).value(),
      ipc::WorkerMember{config.deployment.endpoint, *config.deployment.generation}, control_target);
  if (!channel) {
    return unexpected(channel.error());
  }
  control = std::move(channel).value();
  return Result<void>{};
}

/// Sets `session`, with its job bridge when stream sessions run as its jobs.
Result<void> ServingWorker::Impl::create_session(const ModelSpec& model,
                                                 const ExecutionSessionRuntimeHooks& hooks) {
#if TP_ENABLE_STREAMING_GRPC
  // A session built with its bridge loads with speech jobs, which a backend
  // refuses for a model that runs none: only a speech deployment asks.
  if (config.streaming.enabled && model.model_class() == ModelClass::Speech) {
    auto pair = registry->create_session_with_bridge(model.backend_hint(), hooks);
    if (pair) {
      session = std::move(pair->session);
      job_bridge = std::move(pair->bridge);
      return Result<void>{};
    }
    if (pair.error().code != Error::Code::Unsupported ||
        pair.error().context != "job_bridge_unsupported") {
      return unexpected(pair.error());
    }
    // The backend runs no jobs: the listener refuses every Open.
  }
#endif
  auto created = registry->create_session(model.backend_hint(), hooks);
  if (!created) {
    return unexpected(created.error());
  }
  session = std::move(created).value();
  return Result<void>{};
}

// NOLINTNEXTLINE(readability-function-cognitive-complexity)
Result<void> ServingWorker::Impl::build() {
  if (auto v = config.validate(); !v) {
    log_stderr(config, "error", "config validation failed",
               {{"code", std::string{to_string(v.error().code)}}, {"message", v.error().message}});
    return v;
  }
  // Buffer plane.
  {
    auto r = BufferManager::create(config.buffer);
    if (!r) {
      log_stderr(
          config, "error", "buffer manager construction failed",
          {{"code", std::string{to_string(r.error().code)}}, {"message", r.error().message}});
      return unexpected(r.error());
    }
    buffer_manager = std::move(r).value();
  }
  // Backend registry: install built-ins if no registry was provided.
  if (registry == nullptr) {
    owned_registry = std::make_unique<BackendRegistry>();
    registry = owned_registry.get();
    auto br = register_builtin_backends(*registry);
    if (!br) {
      log_stderr(
          config, "warn", "builtin backend registration partial",
          {{"code", std::string{to_string(br.error().code)}}, {"message", br.error().message}});
    }
  }
  // Session.
  if (config.deployment.use_mock_session) {
    async_policy_supported = true;
    serving::MockSessionConfig mock_cfg;
    auto sess = std::make_unique<serving::MockServingSession>(*buffer_manager, mock_cfg);
    if (auto lr = sess->load(ModelSpec::create("mock-model", ModelClass::Custom, "mock://", "mock",
                                               PrecisionHint::Auto)
                                 .value());
        !lr) {
      log_stderr(
          config, "error", "mock session load failed",
          {{"code", std::string{to_string(lr.error().code)}}, {"message", lr.error().message}});
      return unexpected(lr.error());
    }
    if (auto pr = sess->prime(); !pr) {
      log_stderr(
          config, "error", "mock session prime failed",
          {{"code", std::string{to_string(pr.error().code)}}, {"message", pr.error().message}});
      return unexpected(pr.error());
    }
    session = std::move(sess);
  } else {
    if (!config.deployment.model.has_value()) {
      return unexpected(Error::Code::ConfigInvalid,
                        "serving worker: deployment.model required when use_mock_session=false");
    }
    auto validate_r = registry->validate_backend_hint(*config.deployment.model);
    if (!validate_r) {
      log_stderr(config, "error", "backend validation failed",
                 {{"code", std::string{to_string(validate_r.error().code)}},
                  {"message", validate_r.error().message}});
      return unexpected(validate_r.error());
    }
    auto capability_r = registry->capability(config.deployment.model->backend_hint());
    if (!capability_r) {
      log_stderr(config, "error", "backend capability lookup failed",
                 {{"code", std::string{to_string(capability_r.error().code)}},
                  {"message", capability_r.error().message}});
      return unexpected(capability_r.error());
    }
    async_policy_supported = capability_r.value().supports_async();
    ExecutionSessionRuntimeHooks hooks;
    hooks.buffer_manager = buffer_manager.get();
    if (auto s_r = create_session(*config.deployment.model, hooks); !s_r) {
      log_stderr(
          config, "error", "session construction failed",
          {{"code", std::string{to_string(s_r.error().code)}}, {"message", s_r.error().message}});
      return unexpected(s_r.error());
    }
    if (auto lr = session->load(*config.deployment.model); !lr) {
      log_stderr(
          config, "error", "session load failed",
          {{"code", std::string{to_string(lr.error().code)}}, {"message", lr.error().message}});
      return unexpected(lr.error());
    }
    if (auto pr = session->prime(); !pr) {
      log_stderr(
          config, "error", "session prime failed",
          {{"code", std::string{to_string(pr.error().code)}}, {"message", pr.error().message}});
      return unexpected(pr.error());
    }
  }
  // Scheduler.
  clock = std::make_unique<SystemSchedulerClock>();
#if TP_ENABLE_STREAMING_GRPC
  if (job_bridge != nullptr) {
    serving::SpeechDispatchOptions options;
    if (config.deployment.generation) {
      options.generation = *config.deployment.generation;
    }
    options.on_stall = [this](const Error& cause) { on_speech_stall(cause); };
    auto dispatch =
        serving::SpeechJobDispatch::create(job_bridge, *buffer_manager, *clock, std::move(options));
    if (!dispatch) {
      log_stderr(config, "error", "speech dispatch construction failed",
                 {{"code", std::string{to_string(dispatch.error().code)}},
                  {"message", dispatch.error().message}});
      return unexpected(dispatch.error());
    }
    speech_dispatch = std::move(dispatch).value();
  }
#endif
  scheduler_sink = std::make_unique<SchedulerHealthSink>(health, metrics, scheduler_ptr);
  SchedulerRuntimeHooks sched_hooks;
  sched_hooks.event_sink = scheduler_sink.get();
  sched_hooks.buffer_manager = buffer_manager.get();
  sched_hooks.clock = clock.get();
  {
    auto r = make_scheduler(config.scheduler, sched_hooks);
    if (!r) {
      log_stderr(
          config, "error", "scheduler construction failed",
          {{"code", std::string{to_string(r.error().code)}}, {"message", r.error().message}});
      return unexpected(r.error());
    }
    scheduler = std::move(r).value();
    scheduler_ptr = scheduler.get();
  }
  // Metrics labels.
  MetricsLabels labels;
  labels.endpoint = config.deployment.endpoint;
  labels.backend = config.deployment.backend;
  if (config.deployment.model.has_value()) {
    labels.model_name = config.deployment.model->model_id();
    labels.model_class = std::string{to_string(config.deployment.model->model_class())};
  } else {
    labels.model_name = "mock-model";
    labels.model_class = "custom";
  }
  metrics.set_labels(labels);
  // Async store.
  async_store = std::make_unique<serving::AsyncPolicyStore>(config.async_policy, *buffer_manager,
                                                            clock.get(), &metrics);
  // Pipeline.
  serving::ServingPipelineDeps p_deps;
  p_deps.scheduler = scheduler.get();
  p_deps.session = session.get();
  p_deps.buffer_manager = buffer_manager.get();
  p_deps.metrics = &metrics;
  p_deps.backend_name = config.deployment.backend;
  p_deps.model_id = labels.model_name;
  p_deps.endpoint = config.deployment.endpoint;
  pipeline = std::make_unique<serving::ServingPipeline>(p_deps);
  // Router.
  serving::RequestRouterDeps r_deps;
  r_deps.pipeline = pipeline.get();
  r_deps.async_store = async_store.get();
  r_deps.buffer_manager = buffer_manager.get();
  r_deps.metrics = &metrics;
  r_deps.health = &health;
  r_deps.scheduler = scheduler.get();
  r_deps.max_body_bytes = config.http.max_body_bytes;
  r_deps.endpoint = config.deployment.endpoint;
  r_deps.async_policy_supported = async_policy_supported;
  router = std::make_unique<serving::RequestRouter>(r_deps);
  // HTTP server.
  server = std::make_unique<http::HttpServer>();

  // Identity / startup health.
  health.set_identity(config.deployment.endpoint, config.deployment.backend,
                      config.deployment.model.has_value()
                          ? std::optional<std::string>{config.deployment.model->model_id()}
                          : std::nullopt);
  // A fence or a retire that came during the load applies from here.
  control_target.set_session_ceiling(stream_session_ceiling());
  control_target.bind_router(*router);
  publish_built_state();
  return Result<void>{};
}

/// Ready, or Degraded for a session that is not, unless a retire during the
/// load already asked for the stop.
void ServingWorker::Impl::publish_built_state() {
  const std::lock_guard guard(state_mu);
  if (!shutdown.is_stopping()) {
    health.set_state(session->is_ready() ? ServingState::Ready : ServingState::Degraded);
  }
}

void ServingWorker::Impl::set_serving_state(ServingState state) noexcept {
  const std::lock_guard guard(state_mu);
  health.set_state(state);
}

/// The stream sessions the worker serves at one time: none without a dispatch.
// NOLINTNEXTLINE(readability-convert-member-functions-to-static): members with the feature only.
std::uint32_t ServingWorker::Impl::stream_session_ceiling() const noexcept {
#if TP_ENABLE_STREAMING_GRPC
  if (speech_dispatch != nullptr) {
    return kStreamSessions;
  }
#endif
  return 0;
}

Result<void> ServingWorker::Impl::start_listener() {
  // Wire routes against the router instance. Capturing `this` is
  // safe; the server is owned by the worker.
  ServingWorker::Impl* self = this;
  // /infer
  server->add_route("POST", "/infer",
                    [self](const http::Request& req) { return self->router->handle_infer(req); });
  // /policy/infer
  server->add_route("POST", "/policy/infer", [self](const http::Request& req) {
    return self->router->handle_policy_infer(req);
  });
  // /policy/result/<id>
  server->add_prefix_route("GET", "/policy/result/", [self](const http::Request& req) {
    std::string_view path = req.path;
    constexpr std::string_view prefix = "/policy/result/";
    std::string_view id = path.substr(prefix.size());
    return self->router->handle_policy_result(req, id);
  });
  // /policy/cancel/<id>
  server->add_prefix_route("POST", "/policy/cancel/", [self](const http::Request& req) {
    std::string_view path = req.path;
    constexpr std::string_view prefix = "/policy/cancel/";
    std::string_view id = path.substr(prefix.size());
    return self->router->handle_policy_cancel(req, id);
  });
  // /health
  server->add_route("GET", "/health", [self](const http::Request& /*req*/) {
    if (self->config.health_mode == HealthMode::Disabled) {
      return http::Response::plain(404, "health disabled");
    }
    auto snap = self->health.snapshot();
    auto resp = http::Response::json(health_http_status(snap.state), serialize_health_json(snap));
    return resp;
  });
  // /metrics
  server->add_route("GET", "/metrics", [self](const http::Request& /*req*/) {
    if (self->config.metrics_mode == MetricsMode::Disabled) {
      return http::Response::plain(404, "metrics disabled");
    }
    if (self->buffer_manager != nullptr) {
      const auto acc = self->buffer_manager->accounting();
      self->metrics.record_buffer_accounting(acc.in_use_bytes, acc.active_count,
                                             acc.high_water_bytes);
    }
    if (self->scheduler != nullptr) {
      const auto m = self->scheduler->metrics();
      self->metrics.record_scheduler_accounting(m);
    }
    auto snap = self->metrics.snapshot();
    if (self->config.metrics_mode == MetricsMode::PrometheusText) {
      auto resp = http::Response::plain(200, render_prometheus_text(snap));
      resp.set_header("content-type", "text/plain; version=0.0.4");
      return resp;
    }
    return http::Response::ok_json(render_metrics_json(snap));
  });

  http::HttpServerConfig hcfg;
  hcfg.bind_host = config.bind.host;
  hcfg.bind_port = config.bind.port;
  hcfg.max_body_bytes = config.http.max_body_bytes;
  hcfg.max_header_bytes = config.http.max_header_bytes;
  hcfg.request_timeout = config.http.request_timeout;
  hcfg.worker_thread_count = config.http.accept_thread_pool_size;
  hcfg.allow_non_loopback = config.bind.allow_non_loopback;
  auto sr = server->start(hcfg);
  if (!sr) {
    log_stderr(
        config, "error", "http server start failed",
        {{"code", std::string{to_string(sr.error().code)}}, {"message", sr.error().message}});
    return unexpected(sr.error());
  }
  log_stderr(config, "info", "http server bound",
             {{"host", config.bind.host}, {"port", server->bound_port()}});
  return Result<void>{};
}

// NOLINTNEXTLINE(readability-convert-member-functions-to-static): members with the feature only.
Result<void> ServingWorker::Impl::start_stream_listener() {
#if TP_ENABLE_STREAMING_GRPC
  if (!config.streaming.enabled) {
    return Result<void>{};
  }
  const auto defaults = serving::SessionLimits::defaults();
  auto limits = serving::SessionLimits::create(
      defaults.idle_timeout(), defaults.heartbeat_interval(), defaults.liveness_timeout(),
      defaults.max_duration(), kStreamSessions);
  if (!limits) {
    return unexpected(limits.error());
  }
  serving::StreamListenerOptions options;
  options.host = config.bind.host;
  options.port = config.streaming.port;
  options.allow_non_loopback = config.bind.allow_non_loopback;
  options.deployment_id = config.deployment.endpoint;
  options.generation = config.deployment.generation.value_or(0);
  options.descriptor_digest = config.deployment.descriptor_digest.value_or("");
  options.limits = *limits;
  if (control != nullptr) {
    // Asked under the session manager's lock; it reads atomics only.
    options.admission_check = [this] { return control_target.admit_session(*control); };
  }
  // Without a dispatch the listener refuses every Open.
  auto listener = serving::StreamListener::start(std::move(options), speech_dispatch.get(), *clock);
  if (!listener) {
    log_stderr(config, "error", "stream listener start failed",
               {{"code", std::string{to_string(listener.error().code)}},
                {"message", listener.error().message}});
    if (speech_dispatch != nullptr) {
      stream_start_failure = listener.error();
    }
    return unexpected(listener.error());
  }
  const auto port = (*listener)->bound_port();
  {
    const std::lock_guard guard(stream_mutex);
    stream = std::move(listener).value();
  }
  log_stderr(config, "info", "stream listener bound",
             {{"host", config.bind.host},
              {"port", port},
              {"serves_sessions", speech_dispatch != nullptr}});
#endif
  return Result<void>{};
}

// NOLINTNEXTLINE(readability-convert-member-functions-to-static): members with the feature only.
void ServingWorker::Impl::stop_stream_admission() noexcept {
#if TP_ENABLE_STREAMING_GRPC
  if (auto* listener = stream_listener(); listener != nullptr) {
    (void)drain_window.begin(std::chrono::steady_clock::now(), config.shutdown.drain_deadline);
    listener->stop_admission(drain_window.cause());
  }
#endif
}

// NOLINTNEXTLINE(readability-convert-member-functions-to-static): members with the feature only.
void ServingWorker::Impl::stop_stream_listener() noexcept {
#if TP_ENABLE_STREAMING_GRPC
  if (auto* listener = stream_listener(); listener != nullptr) {
    // Sessions drain inside the window that began when shutdown was asked.
    const auto end =
        drain_window.begin(std::chrono::steady_clock::now(), config.shutdown.drain_deadline);
    const auto cause = drain_window.cause();
    try {
      log_stderr(config, "info", "stream listener stopping", drain_end_fields(end, cause));
    } catch (...) {  // NOLINT(bugprone-empty-catch): the stop continues without the line.
    }
    listener->stop(end, cause);
    if (speech_dispatch != nullptr) {
      try {
        const auto stats = speech_dispatch->stats();
        log_stderr(
            config, "info", "speech dispatch stopped",
            {{"jobs_submitted", stats.jobs_submitted},
             {"jobs_completed", stats.jobs_completed},
             {"jobs_failed", stats.jobs_failed},
             {"jobs_discarded", stats.jobs_discarded},
             {"releases_refused", stats.releases_refused},
             {"jobs_past_deadline", stats.jobs_past_deadline},
             {"releases_past_bound", stats.releases_past_bound},
             {"longest_release_wait_ms",
              std::chrono::duration_cast<std::chrono::milliseconds>(stats.longest_release_wait)
                  .count()}});
      } catch (...) {  // NOLINT(bugprone-empty-catch): the stop continues without the line.
      }
    }
  }
#endif
}

// On the dispatch's thread. It asks for the worker's stop and does not wait:
// the stop runs on the thread that waits for shutdown. The request comes
// before `failed`, which the binary's main thread answers with a shutdown of
// its own that then finds this one.
void ServingWorker::Impl::on_speech_stall(const Error& cause) noexcept {
  backend_stalled.store(true);
  health.record_error(cause);
  try {
    log_stderr(config, "error", "speech backend stalled",
               {{"code", std::string{to_string(cause.code)}},
                {"reason", cause.context.value_or("")},
                {"message", cause.message}});
    if (!shutdown.is_stopping()) {
      metrics.increment_shutdown_started();
      log_stderr(config, "info", "shutdown requested", {{"reason", "speech backend stalled"}});
    }
    shutdown.request("speech backend stalled");
  } catch (...) {  // NOLINT(bugprone-empty-catch): the state below still stops the binary.
  }
  set_serving_state(ServingState::Failed);
}

// On the control thread, for the first retire. It records the drain and asks
// for the stop, which the thread that waits for shutdown runs: it neither
// stops anything itself nor takes a lock a stop holds while it waits.
void ServingWorker::Impl::begin_retirement(std::chrono::milliseconds window,
                                           std::chrono::milliseconds asked) noexcept {
  const std::lock_guard guard(state_mu);
  // A stop asked first keeps its drain window, cause and reason, whether or
  // not its window has begun, and a stopped worker keeps its state.
  if (shutdown.is_stopping()) {
    return;
  }
  const auto end = drain_window.begin(std::chrono::steady_clock::now(), window,
                                      serving::DrainCause::DeploymentRetired);
  try {
    auto fields = drain_end_fields(end, drain_window.cause());
    fields["asked_ms"] = asked.count();
    fields["window_ms"] = window.count();
    log_stderr(config, "info", "deployment retiring", fields);
    if (asked > window) {
      log_stderr(config, "warn", "retirement drain capped",
                 {{"asked_ms", asked.count()}, {"window_ms", window.count()}});
    }
    metrics.increment_shutdown_started();
    log_stderr(config, "info", "shutdown requested", {{"reason", "deployment retired"}});
  } catch (...) {  // NOLINT(bugprone-empty-catch): the stop is asked without the lines.
  }
  shutdown.request("deployment retired");
  health.set_state(ServingState::Stopping);
}

// Dispatcher / evictor only reach `Impl` members through their
// `unique_ptr` holders so clang-tidy concludes the methods can be
// const. We keep them non-const so a future hook on `Impl` (e.g.
// dispatcher state) does not force a signature churn.
// NOLINTNEXTLINE(readability-make-member-function-const)
void ServingWorker::Impl::dispatcher_loop() {
  while (!stop_workers.load()) {
    if (!pipeline->dispatch_one(*async_store)) {
      std::this_thread::sleep_for(std::chrono::milliseconds{2});
    }
    // Sweep deadlines.
    (void)scheduler->expire_due();
  }
}

// NOLINTNEXTLINE(readability-make-member-function-const)
void ServingWorker::Impl::evictor_loop() {
  while (!stop_workers.load()) {
    async_store->enforce_bounds();
    std::this_thread::sleep_for(std::chrono::milliseconds{50});
  }
}

void ServingWorker::Impl::run_drain() {
  shutdown.enter_draining();
  set_serving_state(ServingState::Draining);
  if (config.shutdown.cancel_queued_immediately) {
    (void)scheduler->shutdown();
    async_store->cancel_all();
  }
  // Wait for in-flight to settle, for what is left of the drain window: a
  // stream drain before this one has used part of it.
  const auto deadline =
      drain_window.begin(std::chrono::steady_clock::now(), config.shutdown.drain_deadline);
  log_stderr(config, "info", "draining requests", drain_end_fields(deadline, drain_window.cause()));
  while (std::chrono::steady_clock::now() < deadline) {
    const auto m = scheduler->metrics();
    if (m.queue_depth == 0 && m.in_flight == 0) {
      break;
    }
    std::this_thread::sleep_for(std::chrono::milliseconds{10});
  }
  // Final cleanup.
  (void)scheduler->shutdown();
  async_store->cancel_all();
  stop_workers.store(true);
  if (dispatcher.joinable()) {
    dispatcher.join();
  }
  if (evictor.joinable()) {
    evictor.join();
  }
  // Unload session.
  if (session) {
    (void)session->unload();
  }
  shutdown.enter_stopped();
  set_serving_state(ServingState::Stopped);
  metrics.increment_shutdown_completed();
}

ServingWorker::ServingWorker(std::unique_ptr<Impl> impl) noexcept : impl_(std::move(impl)) {}

ServingWorker::~ServingWorker() {
  // The destructor is implicitly noexcept; component teardown should not
  // throw, but a misbehaving sink or third-party allocator could. Swallow
  // any exception so destruction stays safe; details are non-recoverable
  // here.
  try {
    if (impl_) {
      (void)stop();
    }
  } catch (...) {  // NOLINT(bugprone-empty-catch): teardown is fire-and-forget.
  }
}

Result<std::unique_ptr<ServingWorker>> ServingWorker::create(ServingConfig config) {
  auto impl = std::make_unique<Impl>();
  impl->config = std::move(config);
  if (auto r = impl->attach_control(); !r) {
    return unexpected(r.error());
  }
  if (auto r = impl->build(); !r) {
    return unexpected(r.error());
  }
  return std::unique_ptr<ServingWorker>(new ServingWorker(std::move(impl)));
}

Result<std::unique_ptr<ServingWorker>> ServingWorker::create(ServingConfig config,
                                                             BackendRegistry& backend_registry) {
  auto impl = std::make_unique<Impl>();
  impl->config = std::move(config);
  impl->registry = &backend_registry;
  if (auto r = impl->attach_control(); !r) {
    return unexpected(r.error());
  }
  if (auto r = impl->build(); !r) {
    return unexpected(r.error());
  }
  return std::unique_ptr<ServingWorker>(new ServingWorker(std::move(impl)));
}

Result<void> ServingWorker::start() {
  if (!impl_) {
    return unexpected(Error::Code::Internal, "serving worker: impl null");
  }
  // Held throughout, as stop() holds it: a stop on another thread runs
  // wholly before this start or wholly after it.
  const std::lock_guard guard(impl_->stop_mutex);
  // A stop unloaded the session, and nothing loads it again.
  if (impl_->stopped.load()) {
    return unexpected(
        Error::make(Error::Code::Unavailable, "the worker was stopped", "worker_shutdown"));
  }
  if (impl_->started.exchange(true)) {
    return Result<void>{};
  }
  if (impl_->control && impl_->control->closed()) {
    // The agent went away while the worker was built: nothing is published.
    impl_->started.store(false);
    return unexpected(Error::make(Error::Code::Unavailable,
                                  "the control channel ended before the worker could serve",
                                  "control_channel_closed"));
  }
#if TP_ENABLE_STREAMING_GRPC
  if (const auto failed = impl_->stream_start_failure; failed.has_value()) {
    impl_->started.store(false);
    return unexpected(*failed);
  }
#endif
  if (auto r = impl_->start_listener(); !r) {
    impl_->started.store(false);
    return r;
  }
  if (auto r = impl_->start_stream_listener(); !r) {
    impl_->server->stop();
    impl_->started.store(false);
    return r;
  }
  impl_->stop_workers.store(false);
  impl_->dispatcher = std::thread([this] { impl_->dispatcher_loop(); });
  impl_->evictor = std::thread([this] { impl_->evictor_loop(); });
  return Result<void>{};
}

ServingExitCode ServingWorker::serve_forever() {
  if (!impl_) {
    return ServingExitCode::Internal;
  }
  if (auto r = start(); !r) {
    // Under the stop's lock: a stop in flight sets its code first, and a
    // stopped worker keeps the code its stop returned.
    const std::lock_guard guard(impl_->stop_mutex);
    if (!impl_->stopped.load()) {
      impl_->exit_code = r.error().code == Error::Code::ConfigInvalid ? ServingExitCode::ConfigError
                                                                      : ServingExitCode::ServeError;
    }
    return impl_->exit_code;
  }
  impl_->shutdown.wait_for_request();
  return stop();
}

void ServingWorker::shutdown(std::string_view reason) noexcept {
  // Logging and controller locks require ordinary thread context, never a
  // signal handler. Exceptions must not escape the shutdown caller.
  try {
    if (!impl_) {
      return;
    }
    if (!impl_->shutdown.is_stopping()) {
      impl_->metrics.increment_shutdown_started();
      log_stderr(impl_->config, "info", "shutdown requested", {{"reason", std::string{reason}}});
    }
    impl_->shutdown.request(std::string{reason});
    impl_->shutdown.notify_request();
    if (impl_->router != nullptr) {
      impl_->router->set_stopping(true);
    }
    if (impl_->pipeline != nullptr) {
      impl_->pipeline->set_stopping(true);
    }
    impl_->stop_stream_admission();
    impl_->set_serving_state(ServingState::Stopping);
  } catch (...) {  // NOLINT(bugprone-empty-catch): noexcept shutdown is fire-and-forget.
  }
}

ServingExitCode ServingWorker::stop() {
  if (!impl_) {
    return ServingExitCode::Ok;
  }
  std::lock_guard<std::mutex> g(impl_->stop_mutex);
  if (impl_->stopped.exchange(true)) {
    return impl_->exit_code;
  }
  // Ensure shutdown request flag is set so dispatcher exits cleanly.
  if (!impl_->shutdown.is_stopping()) {
    impl_->shutdown.request("stop()");
  }
  if (impl_->router != nullptr) {
    impl_->router->set_stopping(true);
  }
  if (impl_->pipeline != nullptr) {
    impl_->pipeline->set_stopping(true);
  }
  impl_->set_serving_state(ServingState::Stopping);
  // The stream sessions drain first: the HTTP stop waits for its handlers
  // with no deadline of its own. The router already refuses new requests.
  impl_->stop_stream_listener();
  if (impl_->server != nullptr) {
    impl_->server->stop();
  }
  // Drain + cleanup.
  impl_->run_drain();
  // Release async-store entries.
  impl_->async_store->cancel_all();
  if (impl_->backend_stalled.load()) {
    impl_->exit_code = ServingExitCode::Internal;
  }
  log_stderr(impl_->config, "info", "shutdown complete", {});
  return impl_->exit_code;
}

std::uint16_t ServingWorker::bound_port() const noexcept {
  if (!impl_ || !impl_->server) {
    return 0;
  }
  return impl_->server->bound_port();
}

const ServingConfig& ServingWorker::config() const noexcept {
  return impl_->config;
}
HealthState& ServingWorker::health() noexcept {
  return impl_->health;
}
ServingMetrics& ServingWorker::metrics() noexcept {
  return impl_->metrics;
}
BufferManager& ServingWorker::buffer_manager() noexcept {
  return *impl_->buffer_manager;
}
InferScheduler& ServingWorker::scheduler() noexcept {
  return *impl_->scheduler;
}
serving::AsyncPolicyStore& ServingWorker::async_store() noexcept {
  return *impl_->async_store;
}
serving::RequestRouter& ServingWorker::router() noexcept {
  return *impl_->router;
}

}  // namespace tensorplate
