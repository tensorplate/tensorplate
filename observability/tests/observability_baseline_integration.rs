// SPDX-License-Identifier: Apache-2.0
//
// V01-E12-F08: Observability baseline integration and failure-injection
// tests.
//
// The tests exercise the V01-E12 telemetry surface end-to-end through
// deterministic fixtures:
//
//   - a fake monotonic clock so timing decisions are reproducible;
//   - the bounded log emitter (V01-E12-F01) feeding the diagnostics
//     retention sink (V01-E12-F06);
//   - the metrics registry (V01-E12-F04) recording counters / gauges /
//     histograms with bounded labels and exporting to a file sink;
//   - the control-loop aggregator (V01-E12-F05) building a rolling
//     window summary against a fake monotonic clock;
//   - the snapshot writer (V01-E12-F07) projecting all of the above
//     plus the V01-E10 aggregator state, the last correlation id, and
//     the last failure reason.
//
// The matrix mirrors the V01-E12 acceptance criteria: failed deploy
// reason, failed inference typed error/log, metrics export, correlation
// propagation, bounded retention under event storm, sink backpressure
// drop accounting, invalid label rejection, control-loop metric
// formulas, and no payload/secret leakage.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::default_trait_access,
    clippy::missing_panics_doc,
    clippy::too_many_lines,
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::unnecessary_get_then_check,
    clippy::unchecked_duration_subtraction
)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tensorplate_observability::{
    clock::{FakeClock, MonotonicClock as _},
    control_loop::{ControlLoopAggregator, ControlLoopAggregatorConfig},
    log_emitter::LogEmitter,
    metrics::{
        default_latency_buckets_ms, endpoint_backend_labels, MetricSinkConfig, MetricsExportConfig,
        MetricsRegistry,
    },
    retention::{DiagnosticsRetention, RetentionConfig, RetentionDropPolicy},
    snapshot::{ControlLoopStatus, DiagnosticsSinkStatus, MetricsExportStatus, SnapshotWriter},
};
use tensorplate_protocol::{
    ControlLoopLabels, CorrelationId, ErrorCode, FailureReason, FailureReasonRecord, LogComponent,
    LogContextValue, LogEvent, LogLevel, MetricEvent, MetricKind, MetricUnit, ValidatePayload,
};

#[test]
fn failed_deploy_emits_correlated_log_and_failure_reason() {
    let clock = FakeClock::new();
    let retention = Arc::new(DiagnosticsRetention::new(RetentionConfig::default()));
    let emitter = LogEmitter::new(LogComponent::Agent, retention.clone(), &clock);

    let correlation = CorrelationId::from_seed(0xDEAD_BEEF);
    clock.advance(Duration::from_millis(5));

    // Build the failure reason taxonomy entry and emit a correlated
    // failure log.
    let record = FailureReasonRecord::new(FailureReason::BundleIntegrityFailed)
        .with_detail("manifest digest mismatch")
        .with_correlation_id(correlation.as_str());
    assert_eq!(record.error_code, ErrorCode::ConfigInvalid);

    assert!(emitter.emit_with(
        "deploy.failed",
        LogLevel::Error,
        &clock,
        |event: &mut LogEvent| {
            event.transaction_id = Some("txn-1".into());
            event.deployment_id = Some("deploy-7".into());
            event.correlation_id = Some(correlation.as_str().to_string());
            event.error_code = Some(record.error_code);
            event.failure_reason = Some(record.reason);
            event.insert_context("phase", "verify");
        },
    ));

    let drained = retention.drain();
    assert_eq!(drained.len(), 1);
    let log = &drained[0];
    assert_eq!(log.event, "deploy.failed");
    assert_eq!(
        log.failure_reason,
        Some(FailureReason::BundleIntegrityFailed)
    );
    assert_eq!(log.error_code, Some(ErrorCode::ConfigInvalid));
    assert_eq!(log.correlation_id.as_deref(), Some(correlation.as_str()));
    assert_eq!(log.deployment_id.as_deref(), Some("deploy-7"));
    assert_eq!(
        log.context.get("phase"),
        Some(&LogContextValue::String("verify".into()))
    );

    // The failure reason record is JSON-round-trippable.
    let json = serde_json::to_string(&record).expect("serialise");
    let back: FailureReasonRecord = serde_json::from_str(&json).expect("deserialise");
    assert_eq!(back, record);
}

#[test]
fn failed_inference_records_metric_and_log_with_correlation() {
    let clock = FakeClock::new();
    let retention = Arc::new(DiagnosticsRetention::new(RetentionConfig::default()));
    let emitter = LogEmitter::new(LogComponent::ServingWorker, retention.clone(), &clock);
    let registry = MetricsRegistry::new(MetricsExportConfig::default(), &clock);

    let infer_failures = registry
        .register_counter(
            "tp_infer_failures_total",
            MetricUnit::Count,
            endpoint_backend_labels("/v1/infer", "tensorrt"),
        )
        .expect("register");
    let infer_latency = registry
        .register_histogram(
            "tp_infer_latency_ms",
            MetricUnit::Milliseconds,
            endpoint_backend_labels("/v1/infer", "tensorrt"),
            default_latency_buckets_ms(),
        )
        .expect("register");

    // Stage 1: one successful inference (3 ms).
    clock.advance(Duration::from_millis(3));
    registry.observe(infer_latency, 3.0).expect("observe");

    // Stage 2: one failed inference (timeout).
    let correlation = CorrelationId::from_seed(0xCAFE_F00D);
    clock.advance(Duration::from_millis(50));
    registry.inc_counter(infer_failures, 1).expect("inc");
    assert!(emitter.emit_failure(
        "infer.timeout",
        FailureReason::Timeout,
        Some(&correlation),
        &clock,
    ));

    // The metric and the log share the correlation id only at the log
    // layer (metric labels are bounded). The registry recorded one
    // failure and one latency observation.
    let snapshot = registry.take_snapshot(&clock);
    assert_eq!(snapshot.len(), 2);
    let counter = snapshot
        .iter()
        .find(|m| m.name == "tp_infer_failures_total")
        .expect("counter present");
    assert_eq!(counter.kind, MetricKind::Counter);
    assert_eq!(counter.sample.value, Some(1.0));
    let histogram = snapshot
        .iter()
        .find(|m| m.name == "tp_infer_latency_ms")
        .expect("histogram present");
    assert_eq!(histogram.kind, MetricKind::Histogram);
    assert_eq!(histogram.sample.count, Some(1));

    let drained = retention.drain();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].failure_reason, Some(FailureReason::Timeout));
    assert_eq!(
        drained[0].correlation_id.as_deref(),
        Some(correlation.as_str())
    );

    // Snapshot serialised wire form contains the correlation id.
    let wire = serde_json::to_string(&drained[0]).expect("serialise");
    assert!(wire.contains(correlation.as_str()));
}

#[test]
fn metrics_export_to_file_runs_without_platform_connectivity() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("metrics.jsonl");
    let clock = FakeClock::new();
    let registry = MetricsRegistry::new(
        MetricsExportConfig {
            sink: MetricSinkConfig::File { path: path.clone() },
            ..MetricsExportConfig::default()
        },
        &clock,
    );
    let id = registry
        .register_counter(
            "tp_requests_total",
            MetricUnit::Count,
            endpoint_backend_labels("/v1/infer", "mock"),
        )
        .expect("register");
    for _ in 0..5 {
        registry.inc_counter(id, 1).expect("inc");
    }
    registry.export(&clock).expect("export");
    let body = std::fs::read_to_string(&path).expect("read");
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 1);
    let parsed: MetricEvent = serde_json::from_str(lines[0]).expect("parse");
    assert_eq!(parsed.name, "tp_requests_total");
    assert_eq!(parsed.sample.value, Some(5.0));
}

#[test]
fn retention_event_storm_stays_bounded_and_logs_drops() {
    let clock = FakeClock::new();
    let retention = Arc::new(DiagnosticsRetention::new(RetentionConfig {
        queue_capacity: 8,
        drop_policy: RetentionDropPolicy::DropOldest,
        ..RetentionConfig::default()
    }));
    let emitter = LogEmitter::new(LogComponent::Cli, retention.clone(), &clock);
    for i in 0..128 {
        emitter.emit_with(
            "doctor.check",
            LogLevel::Debug,
            &clock,
            |event: &mut LogEvent| {
                event.insert_context("seq", LogContextValue::Integer(i as i64));
            },
        );
    }
    let counters = retention.counters();
    assert_eq!(counters.enqueued, 128);
    assert_eq!(counters.dropped_queue_full, 128 - 8);
    let drained = retention.drain();
    assert_eq!(drained.len(), 8);
}

#[test]
fn invalid_metric_labels_are_rejected_without_expanding_cardinality() {
    let clock = FakeClock::new();
    let registry = MetricsRegistry::new(MetricsExportConfig::default(), &clock);
    let mut labels = tensorplate_protocol::MetricLabels::new();
    // Direct map insertion bypasses MetricLabels::insert's allowed-key
    // policy; the registry must catch it.
    labels.0.insert("hostname".into(), "device-1".into());
    let err = registry
        .register_counter("tp_x", MetricUnit::Count, labels)
        .expect_err("rejected");
    assert!(err.to_string().contains("not in the bounded"));
    let counters = registry.counters();
    assert_eq!(counters.series_registered, 0);
    assert_eq!(counters.series_rejected_unknown_label, 1);
}

#[test]
fn unknown_log_schema_version_is_rejected_with_typed_error() {
    let json = r#"{"schema_version":"99.99","component":"agent","event":"deploy.received","level":"info","monotonic_timestamp_ns":1}"#;
    let err =
        tensorplate_protocol::decode_with_version_check::<LogEvent>(json).expect_err("rejected");
    assert!(
        matches!(
            err,
            tensorplate_protocol::DecodeError::UnsupportedSchemaVersion { .. }
        ),
        "expected UnsupportedSchemaVersion, got {err:?}"
    );
}

#[test]
fn no_payload_or_secret_leaks_into_log_context() {
    let clock = FakeClock::new();
    let retention = Arc::new(DiagnosticsRetention::new(RetentionConfig::default()));
    let emitter = LogEmitter::new(LogComponent::Agent, retention.clone(), &clock);

    // Attempt to leak a binary blob and a control-character "secret".
    assert!(emitter.emit_with(
        "agent.startup",
        LogLevel::Info,
        &clock,
        |event: &mut LogEvent| {
            event.insert_context("tensor", "x".repeat(10_000));
            event.insert_context("secret", "\u{0001}TOKEN=abcd");
            event.insert_context("ok_key", "ok_value");
        },
    ));
    let drained = retention.drain();
    assert_eq!(drained.len(), 1);
    let event = &drained[0];
    // Oversize string is truncated, not dropped, and contains no NUL
    // / control bytes.
    let trimmed = event.context.get("tensor").expect("tensor present");
    if let LogContextValue::String(s) = trimmed {
        assert!(s.len() <= tensorplate_protocol::MAX_LOG_CONTEXT_STRING_BYTES);
        assert!(!s.bytes().any(|b| b == 0));
    } else {
        panic!("unexpected context value variant");
    }
    // Control-byte string is dropped.
    assert!(event.context.get("secret").is_none());
    // Safe key/value is preserved.
    assert_eq!(
        event.context.get("ok_key"),
        Some(&LogContextValue::String("ok_value".into()))
    );

    // The wire-format serialisation does not contain the raw NUL byte
    // or the unredacted "TOKEN=" substring.
    let wire = serde_json::to_string(event).expect("serialise");
    assert!(!wire.contains('\u{0001}'));
    assert!(!wire.contains("TOKEN=abcd"));
}

#[test]
fn control_loop_metrics_match_formulas_under_stable_cadence() {
    let labels = ControlLoopLabels::new("/v1/act", "vla", "smolvla-tiny", "tensorrt");
    let mut aggregator =
        ControlLoopAggregator::new(ControlLoopAggregatorConfig::new(30.0, labels.clone()))
            .expect("config");
    let clock = FakeClock::new();
    for _ in 0..120 {
        aggregator.record_output(clock.now());
        clock.advance(Duration::from_micros(33_333));
    }
    let summary = aggregator.summary(&clock);
    let event = aggregator.event(&clock, clock.now() - Duration::from_secs(120));

    assert_eq!(summary.samples, 119);
    assert!(summary.missed_deadline_rate < 0.05);
    assert!(summary.jitter_p99_ms.unwrap() < 1.0);
    assert!(summary.frequency_error_pct.unwrap() < 1.0);

    // Wire form passes the schema validation including the bounded
    // labels and the strictly positive frequency.
    let json = serde_json::to_string(&event).expect("serialise");
    let parsed = tensorplate_protocol::decode_with_version_check::<
        tensorplate_protocol::ControlLoopEvent,
    >(&json)
    .expect("decode");
    assert_eq!(parsed.labels, labels);
    assert!(parsed.target_period_ms.unwrap() > 0.0);
}

#[test]
fn snapshot_projection_aggregates_v12_fields() {
    let writer = SnapshotWriter::new(&tensorplate_observability::StatusSnapshotConfig::default());
    let diagnostics = DiagnosticsSinkStatus {
        enqueued: 50,
        dropped_queue_full: 4,
        ..DiagnosticsSinkStatus::default()
    };
    let metrics = MetricsExportStatus {
        series_registered: 6,
        samples_recorded: 200,
        ..MetricsExportStatus::default()
    };
    let summary = tensorplate_protocol::ControlLoopSummary {
        samples: 110,
        missed_deadlines: 1,
        missed_deadline_rate: 0.009,
        jitter_p50_ms: Some(0.3),
        jitter_p95_ms: Some(1.1),
        jitter_p99_ms: Some(2.0),
        jitter_max_ms: Some(2.5),
        mean_frequency_hz: Some(30.0),
        frequency_stddev_hz: Some(0.1),
        frequency_error_pct: Some(0.05),
    };
    let control = ControlLoopStatus::from_summary(
        30.0,
        60,
        "/v1/act",
        "vla",
        "smolvla-tiny",
        "tensorrt",
        &summary,
    );
    writer.update_v12(diagnostics.clone(), metrics.clone(), Some(control.clone()));
    writer.update_last_failure(Some("req-1".into()), Some(FailureReason::Timeout));
    let snapshot = writer.current();
    assert_eq!(snapshot.diagnostics_sink, diagnostics);
    assert_eq!(snapshot.metrics_export, metrics);
    assert_eq!(snapshot.control_loop, Some(control));
    assert_eq!(snapshot.last_correlation_id.as_deref(), Some("req-1"));
    assert_eq!(snapshot.last_failure_reason, Some(FailureReason::Timeout));

    // The full snapshot is round-trippable as a CLI-consumed payload.
    let json = serde_json::to_string(&snapshot).expect("serialise");
    let _: tensorplate_observability::StatusSnapshot =
        serde_json::from_str(&json).expect("CLI must parse");
}

#[test]
fn log_event_validate_payload_rejects_unsafe_context() {
    // Bypass the bounded helpers to construct an out-of-policy event;
    // `validate_payload` must reject it without panicking.
    let mut context = BTreeMap::new();
    context.insert("bad key".into(), LogContextValue::String("v".into()));
    let event = LogEvent {
        schema_version: tensorplate_protocol::SCHEMA_VERSION.to_string(),
        component: LogComponent::Adapter,
        event: "infer.timeout".into(),
        level: LogLevel::Error,
        monotonic_timestamp_ns: 1,
        wall_time_iso8601: None,
        correlation_id: None,
        request_id: None,
        transaction_id: None,
        deployment_id: None,
        model_name: None,
        model_class: None,
        backend: None,
        error_code: Some(ErrorCode::Timeout),
        failure_reason: Some(FailureReason::Timeout),
        duration_ms: None,
        context,
    };
    let err = event.validate_payload().expect_err("rejected");
    assert!(matches!(
        err,
        tensorplate_protocol::DecodeError::InvalidPayload(_)
    ));
}
