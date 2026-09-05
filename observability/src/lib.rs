// SPDX-License-Identifier: Apache-2.0
//
// V01-E10: `tensorplate-observability` — the independent health monitor.
//
// The crate exposes:
//
//   - [`config`]: validated [`ObservabilityConfig`] schema, mirrored at
//     `config/schemas/observability.json`.
//   - [`error`]: typed [`ObservabilityError`] surfaced through the
//     status snapshot and the V01-E11 CLI.
//   - [`clock`]: monotonic clock abstraction with a [`FakeClock`] for
//     deterministic tests.
//   - [`listener`]: bounded local event ingestion, schema validation,
//     and sequence handling.
//   - [`heartbeat`]: monotonic heartbeat evaluator and no-heartbeat
//     detector.
//   - [`state`]: health-state aggregator and safe-state event
//     definition.
//   - [`sink`]: bounded safe-state sinks (in-memory ring + file).
//   - [`ros2`]: optional `DiagnosticArray` ROS 2 publisher stub.
//   - [`snapshot`]: versioned status snapshot writer + bounded
//     diagnostics ring.
//   - [`service`]: composition root.
//
// The crate exposes a binary entry point through `src/main.rs`; tests
// drive the same composition root through the public `Service` API so
// behaviour stays identical across the two surfaces.

#![forbid(unsafe_code)]

pub mod clock;
pub mod config;
pub mod control_loop;
pub mod error;
pub mod heartbeat;
pub mod listener;
pub mod log_emitter;
pub mod metrics;
pub mod retention;
pub mod ros2;
pub mod service;
pub mod sink;
pub mod snapshot;
pub mod state;

pub use clock::{FakeClock, MonotonicClock, SystemMonotonicClock};
pub use config::{
    ControlLoopTelemetryConfig, HeartbeatPolicy, ListenerConfig, ListenerTransport,
    ObservabilityConfig, Ros2HealthConfig, Ros2Runtime, SafeStateSinkConfig, SafeStateSinkKind,
    StatusSnapshotConfig, StatusSnapshotKind,
};
pub use control_loop::{ControlLoopAggregator, ControlLoopAggregatorConfig};
pub use error::{ObservabilityError, ObservabilityResult};
pub use heartbeat::{HeartbeatEvaluator, HeartbeatHealth, SourceState};
pub use listener::{
    EventListener, HealthInput, InputKind, InputSource, ListenerCounters, ListenerCountersSnapshot,
};
pub use log_emitter::{LogEmitter, LogEmitterCounters};
pub use metrics::{
    default_latency_buckets_ms, endpoint_backend_labels, MetricSinkConfig, MetricsCounters,
    MetricsExportConfig, MetricsRegistry, SeriesId, SinkBackpressurePolicy,
};
pub use retention::{
    DiagnosticsRetention, RetentionConfig, RetentionCounters, RetentionDropPolicy,
};
pub use ros2::{
    build_diagnostic_array, DiagnosticArray, DiagnosticKeyValue, DiagnosticLevel, DiagnosticStatus,
    HealthPublisher, MockHealthPublisher, Ros2HealthPublisher,
};
pub use service::Service;
pub use sink::{
    sink_from_config, to_wire, FileSafeStateSink, InMemorySafeStateSink, NoopSafeStateSink,
    SafeStateSink, WireSafeStateEvent,
};
pub use snapshot::{
    BoundedDiagnostics, ControlLoopStatus, DiagnosticsSinkStatus, ListenerStatus,
    MetricsExportStatus, PublisherStatus, RecentError, RecentTransition, SinkStatus,
    SnapshotWriter, StatusSnapshot,
};
pub use state::{AggregateState, Aggregator, ObservabilityState, SafeStateEvent, SafeStateReason};

/// Crate version string compiled from Cargo metadata.
#[must_use]
// A release build may carry an identity Cargo does not: a candidate is
// built from the same tree as the release it is a candidate for, so
// CARGO_PKG_VERSION reports `0.2.1` for both and `--version` cannot tell
// them apart. The release build supplies TP_RELEASE_VERSION; everything
// else falls back to the crate version.
pub fn version() -> &'static str {
    match option_env!("TP_RELEASE_VERSION") {
        Some(version) => version,
        None => env!("CARGO_PKG_VERSION"),
    }
}
