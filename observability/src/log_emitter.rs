// SPDX-License-Identifier: Apache-2.0
//
#![allow(clippy::cast_possible_truncation)]

// V01-E12-F01 / F06: Bounded, non-blocking log emitter wrapper.
//
// `LogEmitter` is the producer-side helper used by the agent, the
// serving worker (eventually, through the C++ -> JSON IPC), the
// observability service itself, and the CLI for the v0.1.0 telemetry
// surface. The emitter:
//
//   - tags every event with a monotonic timestamp derived from the
//     supplied [`MonotonicClock`];
//   - runs the bounded-context sanitiser through
//     `tensorplate_protocol::LogEvent::insert_context`;
//   - rejects events that violate the bounded-context policy without
//     panicking — the producer never sees a logging-related crash;
//   - drops events into a [`DiagnosticsRetention`] sink without
//     blocking; counters surface drops through the status projection.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use tensorplate_protocol::{
    CorrelationId, FailureReason, LogComponent, LogEvent, LogLevel, ValidatePayload,
};

use crate::clock::MonotonicClock;
use crate::retention::DiagnosticsRetention;

/// Bounded counters surfaced through the status projection.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct LogEmitterCounters {
    /// Events accepted by the sanitiser and forwarded to retention.
    pub emitted: u64,
    /// Events rejected because the producer violated the bounded
    /// context policy or the event name policy.
    pub rejected_validation: u64,
}

/// V01-E12 log emitter. Wraps a [`DiagnosticsRetention`] sink and a
/// monotonic clock so producer call sites do not have to reach for
/// either explicitly.
pub struct LogEmitter {
    component: LogComponent,
    retention: Arc<DiagnosticsRetention>,
    epoch: Instant,
    emitted: AtomicU64,
    rejected_validation: AtomicU64,
}

impl LogEmitter {
    #[must_use]
    pub fn new(
        component: LogComponent,
        retention: Arc<DiagnosticsRetention>,
        clock: &dyn MonotonicClock,
    ) -> Self {
        Self {
            component,
            retention,
            epoch: clock.now(),
            emitted: AtomicU64::new(0),
            rejected_validation: AtomicU64::new(0),
        }
    }

    /// Emit a minimal event with no optional fields.
    pub fn emit(&self, event_name: &str, level: LogLevel, clock: &dyn MonotonicClock) -> bool {
        let event = LogEvent::new(self.component, event_name, level, self.timestamp(clock));
        self.emit_event(event)
    }

    /// Emit an event with correlation, deployment, and failure fields
    /// pre-populated through the canonical helpers.
    pub fn emit_with(
        &self,
        event_name: &str,
        level: LogLevel,
        clock: &dyn MonotonicClock,
        builder: impl FnOnce(&mut LogEvent),
    ) -> bool {
        let mut event = LogEvent::new(self.component, event_name, level, self.timestamp(clock));
        builder(&mut event);
        self.emit_event(event)
    }

    /// Emit a failure event with the canonical reason mapping applied.
    pub fn emit_failure(
        &self,
        event_name: &str,
        reason: FailureReason,
        correlation: Option<&CorrelationId>,
        clock: &dyn MonotonicClock,
    ) -> bool {
        let mut event = LogEvent::new(
            self.component,
            event_name,
            LogLevel::Error,
            self.timestamp(clock),
        )
        .with_failure(reason);
        if let Some(c) = correlation {
            event = event.with_correlation_id(c.as_str().to_string());
        }
        self.emit_event(event)
    }

    fn timestamp(&self, clock: &dyn MonotonicClock) -> u64 {
        clock
            .now()
            .saturating_duration_since(self.epoch)
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64
    }

    fn emit_event(&self, event: LogEvent) -> bool {
        if let Ok(event) = event.validate_payload() {
            let _ = self.retention.enqueue(event);
            self.emitted.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            self.rejected_validation.fetch_add(1, Ordering::Relaxed);
            false
        }
    }

    /// Counters snapshot.
    pub fn counters(&self) -> LogEmitterCounters {
        LogEmitterCounters {
            emitted: self.emitted.load(Ordering::Relaxed),
            rejected_validation: self.rejected_validation.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::LogEmitter;
    use crate::clock::FakeClock;
    use crate::retention::{DiagnosticsRetention, RetentionConfig};
    use std::sync::Arc;
    use tensorplate_protocol::{
        CorrelationId, FailureReason, LogComponent, LogContextValue, LogLevel,
    };

    fn setup() -> (Arc<DiagnosticsRetention>, FakeClock) {
        let retention = Arc::new(DiagnosticsRetention::new(RetentionConfig::default()));
        let clock = FakeClock::new();
        (retention, clock)
    }

    #[test]
    fn emit_appends_to_retention_and_advances_counter() {
        let (retention, clock) = setup();
        let emitter = LogEmitter::new(LogComponent::Agent, retention.clone(), &clock);
        assert!(emitter.emit("deploy.received", LogLevel::Info, &clock));
        assert_eq!(emitter.counters().emitted, 1);
        let drained = retention.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].event, "deploy.received");
    }

    #[test]
    fn emit_failure_attaches_reason_and_correlation() {
        let (retention, clock) = setup();
        let emitter = LogEmitter::new(LogComponent::ServingWorker, retention.clone(), &clock);
        let id = CorrelationId::from_seed(0x42);
        assert!(emitter.emit_failure("infer.timeout", FailureReason::Timeout, Some(&id), &clock));
        let drained = retention.drain();
        assert_eq!(drained.len(), 1);
        let event = &drained[0];
        assert_eq!(event.failure_reason, Some(FailureReason::Timeout));
        assert_eq!(event.correlation_id.as_deref(), Some(id.as_str()));
    }

    #[test]
    fn out_of_policy_events_are_rejected_not_panicked() {
        let (retention, clock) = setup();
        let emitter = LogEmitter::new(LogComponent::Cli, retention.clone(), &clock);
        // Event name violates policy (uppercase).
        assert!(!emitter.emit("BAD", LogLevel::Info, &clock));
        assert_eq!(emitter.counters().rejected_validation, 1);
        assert!(retention.drain().is_empty());
    }

    #[test]
    fn builder_callback_can_add_bounded_context() {
        let (retention, clock) = setup();
        let emitter = LogEmitter::new(LogComponent::Agent, retention.clone(), &clock);
        assert!(
            emitter.emit_with("deploy.warmup", LogLevel::Debug, &clock, |event| {
                event.insert_context("attempt", LogContextValue::Integer(2));
            })
        );
        let drained = retention.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(
            drained[0].context.get("attempt"),
            Some(&LogContextValue::Integer(2))
        );
    }
}
