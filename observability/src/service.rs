// SPDX-License-Identifier: Apache-2.0
//
// V01-E10-F01-T02: Observability service composition root.
//
// The `Service` type wires together every V01-E10 component:
//
//   - the bounded local listener (F02)
//   - the monotonic heartbeat evaluator (F03)
//   - the health-state aggregator + safe-state sink (F04)
//   - the optional ROS 2 health publisher stub (F05)
//   - the status snapshot writer with bounded diagnostics (F06)
//
// The service is `tick`-driven, mirroring the V01-E09 supervisor: the
// composition root advances state deterministically and the binary's
// main loop calls `tick` at a configured cadence. Tests inject a
// `FakeClock` and an in-memory listener so the whole pipeline is
// deterministic without any background thread.

#![allow(clippy::cast_precision_loss)]

use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tensorplate_protocol::error::ErrorCode;
use tensorplate_protocol::worker_status::ComponentState;
use tensorplate_protocol::{
    ControlLoopLabels, FailureReason, LogComponent, LogLevel, MetricLabels, MetricUnit,
};

use crate::clock::{MonotonicClock, SystemMonotonicClock};
use crate::config::{ControlLoopTelemetryConfig, ObservabilityConfig, SafeStateSinkKind};
use crate::control_loop::{ControlLoopAggregator, ControlLoopAggregatorConfig};
use crate::error::ObservabilityResult;
use crate::heartbeat::HeartbeatEvaluator;
use crate::listener::{
    EventListener, HealthInput, InputKind, InputSource, ListenerCountersSnapshot,
};
use crate::log_emitter::LogEmitter;
use crate::metrics::{MetricsRegistry, SeriesId};
use crate::retention::DiagnosticsRetention;
use crate::ros2::Ros2HealthPublisher;
use crate::sink::{sink_from_config, SafeStateSink};
use crate::snapshot::{
    ControlLoopStatus, DiagnosticsSinkStatus, MetricsExportStatus, PublisherStatus, RecentError,
    RecentTransition, SinkStatus, SnapshotWriter, StatusSnapshot,
};
use crate::state::{
    AggregateState, Aggregator, ObservabilityState, SafeStateEvent, SafeStateReason,
};

#[derive(Clone, Copy)]
struct ServiceMetricIds {
    queue_depth: SeriesId,
    observability_state: SeriesId,
    listener_malformed: SeriesId,
    export_failures: SeriesId,
}

/// Composition root. Hold one of these per observability process.
pub struct Service {
    cfg: ObservabilityConfig,
    clock: Arc<dyn MonotonicClock>,
    listener: Arc<EventListener>,
    heartbeat: Arc<HeartbeatEvaluator>,
    aggregator: Arc<Aggregator>,
    safe_state_sink: Arc<dyn SafeStateSink>,
    snapshot: Arc<SnapshotWriter>,
    ros2: Option<Arc<Ros2HealthPublisher>>,
    diagnostics_retention: Arc<DiagnosticsRetention>,
    log_emitter: Arc<LogEmitter>,
    metrics: Arc<MetricsRegistry>,
    metric_ids: ServiceMetricIds,
    control_loop: Option<Arc<Mutex<ControlLoopAggregator>>>,
    primary_source: InputSource,
    safe_state_sink_kind: SafeStateSinkKind,
}

fn component_labels(component: &str) -> MetricLabels {
    let mut labels = MetricLabels::new();
    let _ = labels.insert("component", component);
    labels
}

fn register_service_metrics(registry: &MetricsRegistry) -> ObservabilityResult<ServiceMetricIds> {
    let labels = component_labels("observability");
    let queue_depth = registry.register_gauge(
        "tp_observability_queue_depth",
        MetricUnit::Count,
        labels.clone(),
    )?;
    let observability_state =
        registry.register_gauge("tp_observability_state", MetricUnit::Count, labels.clone())?;
    let listener_malformed =
        registry.register_gauge("tp_listener_malformed", MetricUnit::Count, labels.clone())?;
    let export_failures =
        registry.register_counter("tp_export_failures_total", MetricUnit::Count, labels)?;
    Ok(ServiceMetricIds {
        queue_depth,
        observability_state,
        listener_malformed,
        export_failures,
    })
}

fn build_control_loop(
    cfg: &ControlLoopTelemetryConfig,
) -> ObservabilityResult<Option<Arc<Mutex<ControlLoopAggregator>>>> {
    if !cfg.enabled {
        return Ok(None);
    }
    let labels = ControlLoopLabels::new(
        cfg.endpoint.clone(),
        cfg.model_class.clone(),
        cfg.model_name.clone(),
        cfg.backend.clone(),
    );
    let aggregator = ControlLoopAggregator::new(ControlLoopAggregatorConfig {
        control_frequency_hz: cfg.control_frequency_hz,
        window: Duration::from_secs(u64::from(cfg.window_seconds)),
        grace_ms: cfg.grace_ms,
        labels,
    })?;
    Ok(Some(Arc::new(Mutex::new(aggregator))))
}

fn u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn observability_state_value(state: ObservabilityState) -> f64 {
    match state {
        ObservabilityState::Ready => 0.0,
        ObservabilityState::Degraded => 1.0,
        ObservabilityState::Failed => 2.0,
        ObservabilityState::NoHeartbeat => 3.0,
    }
}

fn component_state_value(state: ComponentState) -> f64 {
    match state {
        ComponentState::Ready => 0.0,
        ComponentState::Degraded => 1.0,
        ComponentState::Failed => 2.0,
        ComponentState::Unknown => 3.0,
    }
}

fn failure_reason_for_error_code(code: ErrorCode) -> FailureReason {
    match code {
        ErrorCode::ConfigInvalid => FailureReason::ConfigInvalid,
        ErrorCode::LoadFailed => FailureReason::BackendUnavailable,
        ErrorCode::NotReady => FailureReason::WorkerNotReady,
        ErrorCode::ShapeMismatch => FailureReason::ShapeMismatch,
        ErrorCode::Unsupported => FailureReason::BackendUnsupportedCapability,
        ErrorCode::OomError => FailureReason::Oom,
        ErrorCode::Timeout => FailureReason::Timeout,
        ErrorCode::InferenceFailed | ErrorCode::Internal => FailureReason::Internal,
    }
}

fn failure_reason_for_safe_event(event: &SafeStateEvent) -> Option<FailureReason> {
    match event.reason {
        SafeStateReason::HeartbeatMissing => Some(FailureReason::NoHeartbeat),
        SafeStateReason::CrashLoop => Some(FailureReason::WorkerCrashLoop),
        SafeStateReason::WorkerExit => Some(FailureReason::WorkerExit),
        SafeStateReason::Overload => Some(FailureReason::DeadlineMissed),
        SafeStateReason::ServingFailed => Some(
            event
                .last_error_code
                .map_or(FailureReason::Internal, failure_reason_for_error_code),
        ),
        SafeStateReason::ServingDegraded => Some(FailureReason::WorkerNotReady),
        SafeStateReason::Transition | SafeStateReason::Periodic | SafeStateReason::Recovery => {
            event.last_error_code.map(failure_reason_for_error_code)
        }
    }
}

impl Service {
    /// Build the service from a validated config. The composition root
    /// constructs each component eagerly so startup failures surface as
    /// typed errors before any listener binds or any file is opened.
    ///
    /// # Errors
    ///
    /// Returns [`ObservabilityError::Config`] when the config has not
    /// been validated, or [`ObservabilityError::Internal`] when an
    /// internal invariant is violated.
    pub fn new(cfg: ObservabilityConfig) -> ObservabilityResult<Self> {
        let cfg = cfg.validate()?;
        Self::with_clock(cfg, Arc::new(SystemMonotonicClock))
    }

    /// Build the service with a caller-supplied monotonic clock. Used
    /// by tests with `FakeClock`.
    ///
    /// # Errors
    ///
    /// See [`Service::new`].
    pub fn with_clock(
        cfg: ObservabilityConfig,
        clock: Arc<dyn MonotonicClock>,
    ) -> ObservabilityResult<Self> {
        let cfg = cfg.validate()?;
        let listener = Arc::new(EventListener::new(&cfg.listener, clock.clone()));
        let heartbeat = Arc::new(HeartbeatEvaluator::new(
            cfg.heartbeat.clone(),
            clock.clone(),
        ));
        let primary_source = match cfg.primary_source.as_str() {
            "agent_supervisor" => InputSource::AgentSupervisor,
            "internal" => InputSource::Internal,
            _ => InputSource::ServingWorker,
        };
        heartbeat.register_source(primary_source);
        let aggregator = Arc::new(Aggregator::new(cfg.heartbeat.clone(), primary_source));
        let safe_state_sink: Arc<dyn SafeStateSink> = Arc::from(sink_from_config(&cfg.safe_state));
        let snapshot = Arc::new(SnapshotWriter::new(&cfg.snapshot));
        let ros2 = Ros2HealthPublisher::from_config(&cfg.ros2_health).map(Arc::new);
        let diagnostics_retention =
            Arc::new(DiagnosticsRetention::new(cfg.diagnostics_retention.clone()));
        let log_emitter = Arc::new(LogEmitter::new(
            LogComponent::Observability,
            diagnostics_retention.clone(),
            clock.as_ref(),
        ));
        let metrics = Arc::new(MetricsRegistry::new(cfg.metrics.clone(), clock.as_ref()));
        let metric_ids = register_service_metrics(&metrics)?;
        let control_loop = build_control_loop(&cfg.control_loop)?;
        let safe_state_sink_kind = cfg.safe_state.kind;
        let _ = log_emitter.emit("service.startup", LogLevel::Info, clock.as_ref());
        Ok(Self {
            cfg,
            clock,
            listener,
            heartbeat,
            aggregator,
            safe_state_sink,
            snapshot,
            ros2,
            diagnostics_retention,
            log_emitter,
            metrics,
            metric_ids,
            control_loop,
            primary_source,
            safe_state_sink_kind,
        })
    }

    pub fn config(&self) -> &ObservabilityConfig {
        &self.cfg
    }

    /// Listener handle exposed for test fixtures and for the in-process
    /// minimal serving-worker heartbeat source.
    pub fn listener(&self) -> Arc<EventListener> {
        self.listener.clone()
    }

    /// Heartbeat evaluator handle exposed for tests.
    pub fn heartbeat(&self) -> Arc<HeartbeatEvaluator> {
        self.heartbeat.clone()
    }

    /// Aggregator handle exposed for tests.
    pub fn aggregator(&self) -> Arc<Aggregator> {
        self.aggregator.clone()
    }

    /// Snapshot writer handle exposed for tests and the binary entry
    /// point.
    pub fn snapshot_writer(&self) -> Arc<SnapshotWriter> {
        self.snapshot.clone()
    }

    /// Safe-state sink handle. Tests downcast as appropriate to assert
    /// emitted events.
    pub fn safe_state_sink(&self) -> Arc<dyn SafeStateSink> {
        self.safe_state_sink.clone()
    }

    /// Optional ROS 2 publisher handle.
    pub fn ros2_publisher(&self) -> Option<Arc<Ros2HealthPublisher>> {
        self.ros2.clone()
    }

    /// V01-E12 structured-log retention sink.
    pub fn diagnostics_retention(&self) -> Arc<DiagnosticsRetention> {
        self.diagnostics_retention.clone()
    }

    /// V01-E12 observability-service log emitter.
    pub fn log_emitter(&self) -> Arc<LogEmitter> {
        self.log_emitter.clone()
    }

    /// V01-E12 metrics registry.
    pub fn metrics_registry(&self) -> Arc<MetricsRegistry> {
        self.metrics.clone()
    }

    /// Configured primary heartbeat source.
    pub fn primary_source(&self) -> InputSource {
        self.primary_source
    }

    /// Record a successful VLA action output for configured
    /// control-loop timing metrics.
    pub fn record_control_loop_output(&self) {
        self.record_control_loop_output_at(self.clock.now());
    }

    /// Record a successful VLA action output at a supplied monotonic
    /// instant. Test harnesses use this to drive deterministic samples.
    pub fn record_control_loop_output_at(&self, at: Instant) {
        let Some(control_loop) = &self.control_loop else {
            return;
        };
        if let Ok(mut aggregator) = control_loop.lock() {
            aggregator.record_output(at);
        }
    }

    /// Minimal in-process heartbeat emitter. Tests and deliberately
    /// internal deployments can feed the listener directly with a
    /// synthetic heartbeat input, but the production binary only calls
    /// this when `primary_source=internal` so the default
    /// `serving_worker` source cannot be masked by self-heartbeats.
    pub fn emit_internal_heartbeat(&self) {
        let input = HealthInput::heartbeat(self.primary_source, self.clock.now());
        self.listener.submit_input(input);
    }

    fn log_health_input(&self, input: &HealthInput) {
        let level = match input.kind {
            InputKind::Failed | InputKind::NoHeartbeat | InputKind::CrashLoop => LogLevel::Error,
            InputKind::Degraded
            | InputKind::MissedDeadline
            | InputKind::Overload
            | InputKind::WorkerExit
            | InputKind::WorkerNotReady => LogLevel::Warn,
            InputKind::Heartbeat | InputKind::Ready | InputKind::Recovery => LogLevel::Debug,
        };
        let _ =
            self.log_emitter
                .emit_with("health.accepted", level, self.clock.as_ref(), |event| {
                    event.insert_context("source", input.source.as_str());
                    event.insert_context("kind", input.kind.as_str());
                    if let Some(sequence) = input.sequence {
                        event.insert_context("sequence", u64_to_i64(sequence));
                    }
                    if let Some(depth) = input.queue_depth {
                        event.insert_context("queue_depth", u64_to_i64(depth));
                    }
                    if let Some(rate) = input.missed_deadline_rate {
                        event.insert_context("missed_deadline_rate", rate);
                    }
                    if !input.active_deployment.is_empty() {
                        event.deployment_id = Some(input.active_deployment.clone());
                    }
                    if !input.backend.is_empty() {
                        event.backend = Some(input.backend.clone());
                    }
                    event.error_code = input.error_code;
                    event.failure_reason = input.error_code.map(failure_reason_for_error_code);
                });
    }

    fn log_safe_state_event(&self, safe_event: &SafeStateEvent) {
        let level = match safe_event.state {
            ObservabilityState::Ready => LogLevel::Info,
            ObservabilityState::Degraded => LogLevel::Warn,
            ObservabilityState::Failed | ObservabilityState::NoHeartbeat => LogLevel::Error,
        };
        let failure_reason = failure_reason_for_safe_event(safe_event);
        let _ =
            self.log_emitter
                .emit_with("state.transition", level, self.clock.as_ref(), |event| {
                    event.deployment_id = if safe_event.active_deployment.is_empty() {
                        None
                    } else {
                        Some(safe_event.active_deployment.clone())
                    };
                    event.backend = if safe_event.backend.is_empty() {
                        None
                    } else {
                        Some(safe_event.backend.clone())
                    };
                    event.error_code = safe_event.last_error_code;
                    event.failure_reason = failure_reason;
                    event.insert_context("previous_state", safe_event.previous_state.as_str());
                    event.insert_context("state", safe_event.state.as_str());
                    event.insert_context("reason", safe_event.reason.as_str());
                    event.insert_context(
                        "missed_heartbeat_count",
                        u64_to_i64(safe_event.missed_heartbeat_count),
                    );
                    event.insert_context("queue_depth", u64_to_i64(safe_event.queue_depth));
                    event.insert_context("missed_deadline_rate", safe_event.missed_deadline_rate);
                });
        if failure_reason.is_some() {
            self.snapshot.update_last_failure(None, failure_reason);
        }
    }

    fn record_service_metrics(
        &self,
        state: &AggregateState,
        listener_counters: ListenerCountersSnapshot,
    ) {
        let _ = self
            .metrics
            .set_gauge(self.metric_ids.queue_depth, state.queue_depth as f64);
        let state_value =
            observability_state_value(state.state).max(component_state_value(state.serving_state));
        let _ = self
            .metrics
            .set_gauge(self.metric_ids.observability_state, state_value);
        let malformed = listener_counters
            .malformed
            .saturating_add(listener_counters.unknown_version);
        let _ = self
            .metrics
            .set_gauge(self.metric_ids.listener_malformed, malformed as f64);
    }

    fn control_loop_status(&self) -> Option<ControlLoopStatus> {
        let control_loop = self.control_loop.as_ref()?;
        let Ok(aggregator) = control_loop.lock() else {
            return None;
        };
        Some(ControlLoopStatus::from_summary(
            self.cfg.control_loop.control_frequency_hz,
            self.cfg.control_loop.window_seconds,
            self.cfg.control_loop.endpoint.clone(),
            self.cfg.control_loop.model_class.clone(),
            self.cfg.control_loop.model_name.clone(),
            self.cfg.control_loop.backend.clone(),
            &aggregator.summary(self.clock.as_ref()),
        ))
    }

    fn flush_e12_sinks(&self) {
        if let Err(err) = self.metrics.export(self.clock.as_ref()) {
            let _ = self.metrics.inc_counter(self.metric_ids.export_failures, 1);
            self.snapshot.record_error(RecentError {
                component: "metrics".into(),
                code: ErrorCode::Internal,
                message: err.to_string(),
            });
            let _ = self.log_emitter.emit_with(
                "export.failure",
                LogLevel::Warn,
                self.clock.as_ref(),
                |event| {
                    event.error_code = Some(ErrorCode::Internal);
                    event.failure_reason = Some(FailureReason::Internal);
                    event.insert_context("sink", "metrics");
                },
            );
        }
        if let Err(err) = self.diagnostics_retention.flush_to_file() {
            self.snapshot.record_error(RecentError {
                component: "diagnostics_retention".into(),
                code: ErrorCode::Internal,
                message: err.to_string(),
            });
        }
    }

    /// Drive one tick of the pipeline: drain the listener, recompute
    /// heartbeat freshness, update the aggregator, emit safe-state
    /// events, and refresh the snapshot. The binary main loop calls
    /// this on every wakeup; the integration harness calls it directly
    /// after advancing the fake clock.
    ///
    /// Returns the safe-state events that were emitted during this
    /// tick so callers (tests, the V01-E11 CLI) can assert progress
    /// without scraping the in-memory sink.
    pub fn tick(&self) -> Vec<SafeStateEvent> {
        let inputs = self.listener.try_drain();
        for input in &inputs {
            self.log_health_input(input);
        }
        // Fold heartbeats into the evaluator before we ask it to
        // recompute every source's freshness. We do this here, not in
        // the listener, so the heartbeat source / freshness coupling
        // lives in one place.
        for input in &inputs {
            if matches!(input.kind, InputKind::Heartbeat) {
                self.heartbeat
                    .observe_heartbeat(input.source, input.received_at);
            }
        }
        let heartbeat = self.heartbeat.evaluate();
        let now = self.clock.now();
        let periodic = self.cfg.safe_state.periodic_ms.map(Duration::from_millis);
        let events = self.aggregator.apply(&inputs, &heartbeat, now, periodic);
        for event in &events {
            self.log_safe_state_event(event);
            self.safe_state_sink.emit(event);
            self.snapshot.record_transition(RecentTransition {
                previous_state: event.previous_state.as_str().into(),
                state: event.state.as_str().into(),
                reason: event.reason.as_str().into(),
                monotonic_age_ms: event.monotonic_age_ms,
            });
            if let Some(code) = event.last_error_code {
                self.snapshot.record_error(RecentError {
                    component: "aggregator".into(),
                    code,
                    message: format!(
                        "transition {} -> {} ({})",
                        event.previous_state.as_str(),
                        event.state.as_str(),
                        event.reason.as_str()
                    ),
                });
            }
        }
        if let Some(ros2) = self.ros2.as_ref() {
            let state = self.aggregator.current();
            let _ = ros2.publish_if_due(&state, now);
        }
        let listener_counters = self.listener.counters.snapshot();
        if listener_counters.unknown_version != 0 || listener_counters.malformed != 0 {
            self.snapshot.record_error(RecentError {
                component: "listener".into(),
                code: ErrorCode::Unsupported,
                message: format!(
                    "unknown_version={} malformed={}",
                    listener_counters.unknown_version, listener_counters.malformed
                ),
            });
        }
        let state = self.aggregator.current();
        self.record_service_metrics(&state, listener_counters);
        self.flush_e12_sinks();
        let sink_status = SinkStatus {
            enabled: true,
            dropped: self.safe_state_sink.dropped(),
            errors: self.safe_state_sink.errors(),
        };
        let publisher_status = match self.ros2.as_ref() {
            Some(p) => PublisherStatus {
                enabled: true,
                topic: p.topic.clone(),
                published: p.published(),
                errors: p.errors(),
            },
            None => PublisherStatus::default(),
        };
        self.snapshot.update(
            &state,
            listener_counters.into(),
            sink_status,
            publisher_status,
        );
        self.snapshot.update_v12(
            DiagnosticsSinkStatus::from(self.diagnostics_retention.counters()),
            MetricsExportStatus::from(self.metrics.counters()),
            self.control_loop_status(),
        );
        events
    }

    /// Persist the current snapshot to disk if file-backed.
    ///
    /// # Errors
    ///
    /// Returns [`ObservabilityError::SnapshotSink`] for IO failures.
    pub fn flush_snapshot(&self) -> ObservabilityResult<()> {
        self.snapshot.flush()
    }

    /// Read the most recent snapshot.
    pub fn snapshot(&self) -> StatusSnapshot {
        self.snapshot.current()
    }

    /// Drain the in-memory safe-state sink. Returns `None` when a
    /// file-backed sink is configured.
    pub fn drain_in_memory_sink(&self) -> Option<Vec<SafeStateEvent>> {
        if !matches!(self.safe_state_sink_kind, SafeStateSinkKind::InMemory) {
            return None;
        }
        // We constructed the sink through `sink_from_config`, which
        // returns a boxed trait object. The owning Arc cannot be
        // downcast directly, so we keep the trait surface and ask the
        // caller to construct services with `with_sinks` when they
        // need direct access.
        None
    }

    /// Test-only constructor that lets fixtures inject specific sink
    /// instances. Production code uses [`Service::new`] /
    /// [`Service::with_clock`].
    #[doc(hidden)]
    pub fn with_components(
        cfg: ObservabilityConfig,
        clock: Arc<dyn MonotonicClock>,
        safe_state_sink: Arc<dyn SafeStateSink>,
        ros2: Option<Arc<Ros2HealthPublisher>>,
    ) -> ObservabilityResult<Self> {
        let cfg = cfg.validate()?;
        let listener = Arc::new(EventListener::new(&cfg.listener, clock.clone()));
        let heartbeat = Arc::new(HeartbeatEvaluator::new(
            cfg.heartbeat.clone(),
            clock.clone(),
        ));
        let primary_source = match cfg.primary_source.as_str() {
            "agent_supervisor" => InputSource::AgentSupervisor,
            "internal" => InputSource::Internal,
            _ => InputSource::ServingWorker,
        };
        heartbeat.register_source(primary_source);
        let aggregator = Arc::new(Aggregator::new(cfg.heartbeat.clone(), primary_source));
        let snapshot = Arc::new(SnapshotWriter::new(&cfg.snapshot));
        let diagnostics_retention =
            Arc::new(DiagnosticsRetention::new(cfg.diagnostics_retention.clone()));
        let log_emitter = Arc::new(LogEmitter::new(
            LogComponent::Observability,
            diagnostics_retention.clone(),
            clock.as_ref(),
        ));
        let metrics = Arc::new(MetricsRegistry::new(cfg.metrics.clone(), clock.as_ref()));
        let metric_ids = register_service_metrics(&metrics)?;
        let control_loop = build_control_loop(&cfg.control_loop)?;
        let safe_state_sink_kind = cfg.safe_state.kind;
        let _ = log_emitter.emit("service.startup", LogLevel::Info, clock.as_ref());
        Ok(Self {
            cfg,
            clock,
            listener,
            heartbeat,
            aggregator,
            safe_state_sink,
            snapshot,
            ros2,
            diagnostics_retention,
            log_emitter,
            metrics,
            metric_ids,
            control_loop,
            primary_source,
            safe_state_sink_kind,
        })
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.flush_e12_sinks();
        // Best-effort snapshot persistence on shutdown. The composition
        // root never blocks shutdown on the snapshot; failures are
        // logged and ignored.
        if let Err(err) = self.snapshot.flush() {
            #[allow(clippy::print_stderr)]
            {
                eprintln!("tensorplate-observability: snapshot flush failed at shutdown: {err}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::Service;
    use crate::clock::FakeClock;
    use crate::config::{
        HeartbeatPolicy, ListenerConfig, ObservabilityConfig, Ros2HealthConfig,
        SafeStateSinkConfig, StatusSnapshotConfig,
    };
    use crate::sink::InMemorySafeStateSink;
    use crate::state::ObservabilityState;
    use std::sync::Arc;
    use std::time::Duration;
    use tensorplate_protocol::HealthEvent;

    fn fast_policy() -> HeartbeatPolicy {
        HeartbeatPolicy {
            expected_interval_ms: 50,
            grace_ms: 10,
            missed_threshold: 2,
            recovery_heartbeats: 1,
        }
    }

    fn config() -> ObservabilityConfig {
        ObservabilityConfig {
            heartbeat: fast_policy(),
            ..ObservabilityConfig::default()
        }
    }

    fn service_with_sinks(clock: Arc<FakeClock>) -> (Service, Arc<InMemorySafeStateSink>) {
        let cfg = config();
        let sink = Arc::new(InMemorySafeStateSink::new(&cfg.safe_state));
        let svc = Service::with_components(cfg, clock, sink.clone(), None).expect("service");
        (svc, sink)
    }

    #[test]
    fn fresh_heartbeat_keeps_state_ready() {
        let clock = Arc::new(FakeClock::new());
        let (svc, _sink) = service_with_sinks(clock.clone());
        svc.emit_internal_heartbeat();
        svc.tick();
        assert_eq!(svc.snapshot().observability_state, "ready");
    }

    #[test]
    fn tick_projects_v12_diagnostics_and_metrics() {
        let clock = Arc::new(FakeClock::new());
        let (svc, _sink) = service_with_sinks(clock.clone());
        svc.emit_internal_heartbeat();
        svc.tick();
        let snap = svc.snapshot();
        assert!(snap.diagnostics_sink.enqueued >= 2);
        assert!(snap.metrics_export.series_registered >= 4);
        assert!(snap.metrics_export.samples_recorded >= 3);
        assert!(svc.diagnostics_retention().buffered() >= 2);
        assert!(svc.metrics_registry().take_snapshot(clock.as_ref()).len() >= 4);
    }

    #[test]
    fn enabled_control_loop_projects_summary() {
        let clock = Arc::new(FakeClock::new());
        let cfg = ObservabilityConfig {
            heartbeat: fast_policy(),
            control_loop: crate::config::ControlLoopTelemetryConfig {
                enabled: true,
                control_frequency_hz: 30.0,
                model_name: "smolvla-tiny".into(),
                backend: "mock".into(),
                ..crate::config::ControlLoopTelemetryConfig::default()
            },
            ..ObservabilityConfig::default()
        };
        let sink = Arc::new(InMemorySafeStateSink::new(&cfg.safe_state));
        let svc = Service::with_components(cfg, clock.clone(), sink, None).expect("svc");
        svc.record_control_loop_output();
        clock.advance(Duration::from_micros(33_333));
        svc.record_control_loop_output();
        svc.tick();
        let control = svc.snapshot().control_loop.expect("control loop status");
        assert_eq!(control.samples, 1);
        assert_eq!(control.model_name, "smolvla-tiny");
        assert!(control.frequency_error_pct.unwrap() < 1.0);
    }

    #[test]
    fn missing_heartbeat_transitions_to_no_heartbeat_without_agent_input() {
        let clock = Arc::new(FakeClock::new());
        let (svc, sink) = service_with_sinks(clock.clone());
        svc.emit_internal_heartbeat();
        svc.tick();
        clock.advance(Duration::from_millis(500));
        svc.tick();
        let snap = svc.snapshot();
        assert_eq!(snap.observability_state, "no_heartbeat");
        // V01-E10 acceptance: no agent input required.
        let drained = sink.drain();
        assert!(drained.iter().any(|e| {
            matches!(e.state, ObservabilityState::NoHeartbeat)
                || matches!(e.state, ObservabilityState::Degraded)
        }));
    }

    #[test]
    fn registered_source_without_initial_heartbeat_transitions_to_no_heartbeat() {
        let clock = Arc::new(FakeClock::new());
        let (svc, sink) = service_with_sinks(clock.clone());
        svc.tick();
        assert_eq!(svc.snapshot().observability_state, "degraded");
        clock.advance(Duration::from_millis(500));
        svc.tick();
        assert_eq!(svc.snapshot().observability_state, "no_heartbeat");
        assert!(sink
            .drain()
            .iter()
            .any(|e| matches!(e.state, ObservabilityState::NoHeartbeat)));
    }

    #[test]
    fn safe_state_sink_records_transitions() {
        let clock = Arc::new(FakeClock::new());
        let (svc, sink) = service_with_sinks(clock.clone());
        svc.emit_internal_heartbeat();
        svc.tick();
        // Push a `failed` event.
        svc.listener()
            .submit_health_event(&HealthEvent::state(
                tensorplate_protocol::health_event::HealthEventKind::Failed,
                0,
            ))
            .expect("submit");
        svc.tick();
        let drained = sink.drain();
        assert!(!drained.is_empty());
        assert!(drained
            .iter()
            .any(|e| matches!(e.state, ObservabilityState::Failed)));
        assert_eq!(svc.snapshot().observability_state, "failed");
    }

    #[test]
    fn config_invalid_fails_startup() {
        let mut cfg = ObservabilityConfig::default();
        cfg.heartbeat.missed_threshold = 0;
        assert!(Service::new(cfg).is_err());
    }

    #[test]
    fn ros2_publisher_is_constructed_when_enabled() {
        let clock = Arc::new(FakeClock::new());
        let cfg = ObservabilityConfig {
            heartbeat: fast_policy(),
            ros2_health: Ros2HealthConfig {
                enabled: true,
                ..Ros2HealthConfig::default()
            },
            ..ObservabilityConfig::default()
        };
        let svc = Service::with_clock(cfg, clock.clone()).expect("svc");
        assert!(svc.ros2_publisher().is_some());
        svc.emit_internal_heartbeat();
        svc.tick();
        let snap = svc.snapshot();
        assert!(snap.ros2_publisher.enabled);
        assert!(snap.ros2_publisher.published >= 1);
    }

    #[test]
    fn snapshot_includes_listener_counters() {
        let clock = Arc::new(FakeClock::new());
        let (svc, _sink) = service_with_sinks(clock.clone());
        let _ = svc
            .listener()
            .submit_json("not json")
            .expect_err("malformed");
        svc.tick();
        let snap = svc.snapshot();
        assert!(snap.listener.malformed >= 1);
    }

    #[test]
    fn flush_snapshot_is_noop_for_in_memory_writer() {
        let clock = Arc::new(FakeClock::new());
        let (svc, _sink) = service_with_sinks(clock.clone());
        svc.emit_internal_heartbeat();
        svc.tick();
        svc.flush_snapshot().expect("noop ok");
    }

    #[test]
    fn periodic_safe_state_emission_continues_until_recovery() {
        let clock = Arc::new(FakeClock::new());
        let cfg = ObservabilityConfig {
            heartbeat: fast_policy(),
            safe_state: SafeStateSinkConfig {
                periodic_ms: Some(20),
                ..SafeStateSinkConfig::default()
            },
            ..ObservabilityConfig::default()
        };
        let sink = Arc::new(InMemorySafeStateSink::new(&cfg.safe_state));
        let svc = Service::with_components(cfg, clock.clone(), sink.clone(), None).expect("svc");
        svc.emit_internal_heartbeat();
        svc.tick();
        clock.advance(Duration::from_millis(500));
        svc.tick();
        clock.advance(Duration::from_millis(30));
        svc.tick();
        let drained = sink.drain();
        // At least one transition event + one periodic event.
        assert!(drained.len() >= 2);
    }

    #[test]
    fn snapshot_writer_can_be_file_backed() {
        let clock = Arc::new(FakeClock::new());
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("status.json");
        let cfg = ObservabilityConfig {
            heartbeat: fast_policy(),
            snapshot: StatusSnapshotConfig {
                kind: crate::config::StatusSnapshotKind::File,
                path: Some(path.clone()),
                diagnostics_capacity: 8,
            },
            ..ObservabilityConfig::default()
        };
        let svc = Service::with_clock(cfg, clock.clone()).expect("svc");
        svc.emit_internal_heartbeat();
        svc.tick();
        svc.flush_snapshot().expect("flush");
        let body = std::fs::read_to_string(&path).expect("read");
        let parsed: crate::snapshot::StatusSnapshot = serde_json::from_str(&body).expect("parse");
        assert_eq!(parsed.observability_state, "ready");
        let _ = ListenerConfig::default();
    }
}
