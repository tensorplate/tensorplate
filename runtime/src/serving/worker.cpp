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
#include <string>
#include <string_view>
#include <thread>
#include <utility>
#include <vector>

#include "tensorplate/backend/builtin.hpp"
#include "tensorplate/backend/registry.hpp"
#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/http/http_server.hpp"
#include "tensorplate/scheduler/factory.hpp"
#include "tensorplate/scheduler/scheduler.hpp"
#include "tensorplate/serving/async_policy.hpp"
#include "tensorplate/serving/health.hpp"
#include "tensorplate/serving/metrics.hpp"
#include "tensorplate/serving/pipeline.hpp"
#include "tensorplate/serving/router.hpp"
#include "tensorplate/serving/shutdown.hpp"
#include "tensorplate/version.hpp"

#include "serving/mock_session.hpp"

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
      metrics_.record_scheduler_accounting(m.queue_depth, m.in_flight, m.admitted_total,
                                           m.completed_success, m.completed_failure);
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

  std::atomic<bool> started{false};
  std::atomic<bool> stopped{false};
  std::thread dispatcher;
  std::thread evictor;
  std::atomic<bool> stop_workers{false};
  std::mutex stop_mutex;
  ServingExitCode exit_code = ServingExitCode::Ok;

  Result<void> build();
  Result<void> start_listener();
  // Dispatcher / evictor only call into the owned `unique_ptr` members, so
  // they could be marked const; we keep them non-const so future hooks
  // (e.g. recording dispatcher state on Impl) do not require an API change.
  void dispatcher_loop();  // NOLINT(readability-make-member-function-const)
  void evictor_loop();     // NOLINT(readability-make-member-function-const)
  void run_drain();
};

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
    auto cap_r = registry->validate_backend_hint(*config.deployment.model);
    if (!cap_r) {
      log_stderr(config, "error", "backend validation failed",
                 {{"code", std::string{to_string(cap_r.error().code)}},
                  {"message", cap_r.error().message}});
      return unexpected(cap_r.error());
    }
    ExecutionSessionRuntimeHooks hooks;
    hooks.buffer_manager = buffer_manager.get();
    auto s_r = registry->create_session(config.deployment.model->backend_hint(), hooks);
    if (!s_r) {
      log_stderr(
          config, "error", "session construction failed",
          {{"code", std::string{to_string(s_r.error().code)}}, {"message", s_r.error().message}});
      return unexpected(s_r.error());
    }
    session = std::move(s_r).value();
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
  router = std::make_unique<serving::RequestRouter>(r_deps);
  // HTTP server.
  server = std::make_unique<http::HttpServer>();

  // Identity / startup health.
  health.set_identity(config.deployment.endpoint, config.deployment.backend,
                      config.deployment.model.has_value()
                          ? std::optional<std::string>{config.deployment.model->model_id()}
                          : std::nullopt);
  health.set_state(session->is_ready() ? ServingState::Ready : ServingState::Degraded);
  return Result<void>{};
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
      self->metrics.record_scheduler_accounting(m.queue_depth, m.in_flight, m.admitted_total,
                                                m.completed_success, m.completed_failure);
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
  health.set_state(ServingState::Draining);
  if (config.shutdown.cancel_queued_immediately) {
    (void)scheduler->shutdown();
  }
  // Wait up to drain_deadline for in-flight to settle.
  const auto deadline = std::chrono::steady_clock::now() + config.shutdown.drain_deadline;
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
  // Unload session.
  if (session) {
    (void)session->unload();
  }
  shutdown.enter_stopped();
  health.set_state(ServingState::Stopped);
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
  if (auto r = impl->build(); !r) {
    return unexpected(r.error());
  }
  return std::unique_ptr<ServingWorker>(new ServingWorker(std::move(impl)));
}

Result<void> ServingWorker::start() {
  if (!impl_) {
    return unexpected(Error::Code::Internal, "serving worker: impl null");
  }
  if (impl_->started.exchange(true)) {
    return Result<void>{};
  }
  if (auto r = impl_->start_listener(); !r) {
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
    impl_->exit_code = r.error().code == Error::Code::ConfigInvalid ? ServingExitCode::ConfigError
                                                                    : ServingExitCode::ServeError;
    return impl_->exit_code;
  }
  impl_->shutdown.wait_for_request();
  return stop();
}

void ServingWorker::shutdown(std::string_view reason) noexcept {
  // The public shutdown method is noexcept so signal handlers and agent
  // RPC paths can call it without unwinding. Swallow exceptions from
  // logging / string construction so the shutdown intent always reaches
  // the controller.
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
    impl_->health.set_state(ServingState::Stopping);
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
  impl_->health.set_state(ServingState::Stopping);
  // Stop the HTTP listener first so new admissions stop.
  if (impl_->server != nullptr) {
    impl_->server->stop();
  }
  // Drain + cleanup.
  impl_->run_drain();
  // Stop worker threads.
  impl_->stop_workers.store(true);
  if (impl_->dispatcher.joinable()) {
    impl_->dispatcher.join();
  }
  if (impl_->evictor.joinable()) {
    impl_->evictor.join();
  }
  // Release async-store entries.
  impl_->async_store->cancel_all();
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
