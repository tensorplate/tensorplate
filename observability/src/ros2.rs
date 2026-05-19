// SPDX-License-Identifier: Apache-2.0
//
// V01-E10-F05: ROS 2 health topic stub.
//
// The v0.1.0 baseline publishes a `diagnostic_msgs::DiagnosticArray`
// containing exactly one `DiagnosticStatus` named `tensorplate/runtime`
// per state-change and on the configured interval. The full
// `rclrs` / native ROS 2 transport is reserved for a post-v0.1.0
// release; this module ships:
//
//   - a structured DiagnosticArray data type so consumers in Rust can
//     assert level / key-value mapping without depending on the ROS 2
//     code generator,
//   - a mock publisher that records every emission for tests and for
//     the V01-E10-F07 integration harness,
//   - a `publish_if_due` helper that observes the aggregator state and
//     emits a DiagnosticArray when the state changes or the publish
//     interval elapses.
//
// The optional publisher is gated by `cfg.ros2_health.enabled`; when
// disabled the composition root constructs no publisher at all.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use tensorplate_protocol::error::ErrorCode;
use tensorplate_protocol::worker_status::ComponentState;

use crate::config::Ros2HealthConfig;
use crate::state::{AggregateState, ObservabilityState};

/// ROS 2 `diagnostic_msgs::DiagnosticStatus::Level` mirror. We do not
/// import `rclrs` so the stub stays compilable in environments without
/// a ROS 2 distribution.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum DiagnosticLevel {
    Ok = 0,
    Warn = 1,
    Error = 2,
    Stale = 3,
}

impl DiagnosticLevel {
    #[must_use]
    pub fn from_state(state: ObservabilityState) -> Self {
        match state {
            ObservabilityState::Ready => DiagnosticLevel::Ok,
            ObservabilityState::Degraded => DiagnosticLevel::Warn,
            ObservabilityState::Failed => DiagnosticLevel::Error,
            ObservabilityState::NoHeartbeat => DiagnosticLevel::Stale,
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            DiagnosticLevel::Ok => "ok",
            DiagnosticLevel::Warn => "warn",
            DiagnosticLevel::Error => "error",
            DiagnosticLevel::Stale => "stale",
        }
    }
}

/// `diagnostic_msgs::KeyValue` mirror.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticKeyValue {
    pub key: String,
    pub value: String,
}

/// `diagnostic_msgs::DiagnosticStatus` mirror. v0.1.0 emits exactly one
/// of these with `name = "tensorplate/runtime"`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticStatus {
    pub level: DiagnosticLevel,
    pub name: String,
    pub message: String,
    pub hardware_id: String,
    pub values: Vec<DiagnosticKeyValue>,
}

/// `diagnostic_msgs::DiagnosticArray` mirror.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticArray {
    pub topic: String,
    pub status: Vec<DiagnosticStatus>,
}

/// Trait every publisher implementation satisfies. The composition
/// root installs the mock implementation by default; the native
/// implementation lands post-v0.1.0.
pub trait HealthPublisher: Send + Sync {
    fn publish(&self, array: &DiagnosticArray);
    fn published(&self) -> u64;
    fn errors(&self) -> u64;
    fn topic(&self) -> &str;
}

/// In-process mock publisher. Records every published DiagnosticArray
/// so tests can assert the level/value mapping and so the snapshot can
/// surface the published count.
pub struct MockHealthPublisher {
    topic: String,
    inner: Mutex<Vec<DiagnosticArray>>,
    published: AtomicU64,
}

impl MockHealthPublisher {
    #[must_use]
    pub fn new(topic: impl Into<String>) -> Self {
        Self {
            topic: topic.into(),
            inner: Mutex::new(Vec::new()),
            published: AtomicU64::new(0),
        }
    }

    /// Drain captured DiagnosticArray messages for assertions.
    #[allow(clippy::expect_used)]
    pub fn drain(&self) -> Vec<DiagnosticArray> {
        self.inner
            .lock()
            .expect("mock ros2 publisher poisoned")
            .drain(..)
            .collect()
    }

    /// Number of captured DiagnosticArray messages currently buffered.
    #[allow(clippy::expect_used)]
    pub fn pending_len(&self) -> usize {
        self.inner
            .lock()
            .expect("mock ros2 publisher poisoned")
            .len()
    }
}

impl HealthPublisher for MockHealthPublisher {
    fn publish(&self, array: &DiagnosticArray) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.push(array.clone());
            self.published.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn published(&self) -> u64 {
        self.published.load(Ordering::Relaxed)
    }

    fn errors(&self) -> u64 {
        0
    }

    fn topic(&self) -> &str {
        &self.topic
    }
}

/// Stateful publisher coordinator. Holds the publisher implementation
/// plus the last-published bookkeeping required to decide when the
/// next publish is due.
pub struct Ros2HealthPublisher {
    pub publisher: Box<dyn HealthPublisher>,
    pub topic: String,
    pub interval: Duration,
    last_published_at: Mutex<Option<Instant>>,
    last_state: Mutex<Option<ObservabilityState>>,
}

impl Ros2HealthPublisher {
    /// Wrap an existing publisher implementation.
    #[must_use]
    pub fn new(publisher: Box<dyn HealthPublisher>, cfg: &Ros2HealthConfig) -> Self {
        Self {
            topic: cfg.topic.clone(),
            interval: Duration::from_millis(cfg.interval_ms),
            publisher,
            last_published_at: Mutex::new(None),
            last_state: Mutex::new(None),
        }
    }

    /// Build the v0.1.0 mock-backed publisher described by `cfg`.
    /// Returns `None` when the publisher is disabled.
    #[must_use]
    pub fn from_config(cfg: &Ros2HealthConfig) -> Option<Self> {
        if !cfg.enabled {
            return None;
        }
        let mock = MockHealthPublisher::new(cfg.topic.clone());
        Some(Self::new(Box::new(mock), cfg))
    }

    /// Publish a fresh DiagnosticArray when the state changes or the
    /// configured interval has elapsed since the last emit. Returns
    /// `true` when an emission occurred so the snapshot writer can
    /// update its `published` counter eagerly.
    pub fn publish_if_due(&self, state: &AggregateState, now: Instant) -> bool {
        let array = build_diagnostic_array(&self.topic, state);
        let state_changed = {
            #[allow(clippy::expect_used)]
            let guard = self.last_state.lock().expect("ros2 last state poisoned");
            guard.map_or(true, |s| s != state.state)
        };
        let interval_due = {
            #[allow(clippy::expect_used)]
            let guard = self
                .last_published_at
                .lock()
                .expect("ros2 last published poisoned");
            guard.map_or(true, |t| now.saturating_duration_since(t) >= self.interval)
        };
        if !state_changed && !interval_due {
            return false;
        }
        self.publisher.publish(&array);
        {
            #[allow(clippy::expect_used)]
            let mut guard = self
                .last_published_at
                .lock()
                .expect("ros2 last published poisoned");
            *guard = Some(now);
        }
        {
            #[allow(clippy::expect_used)]
            let mut guard = self.last_state.lock().expect("ros2 last state poisoned");
            *guard = Some(state.state);
        }
        true
    }

    /// Number of successful publishes recorded by the publisher.
    pub fn published(&self) -> u64 {
        self.publisher.published()
    }

    /// Number of publish errors recorded by the publisher.
    pub fn errors(&self) -> u64 {
        self.publisher.errors()
    }
}

/// Helper used by tests and by `publish_if_due`: project the aggregate
/// state into a v0.1.0-compliant DiagnosticArray payload.
#[must_use]
pub fn build_diagnostic_array(topic: &str, state: &AggregateState) -> DiagnosticArray {
    let level = DiagnosticLevel::from_state(state.state);
    let status = DiagnosticStatus {
        level,
        name: "tensorplate/runtime".to_string(),
        message: state_message(state.state),
        hardware_id: state.active_deployment.clone(),
        values: vec![
            kv("agent_state", component_state_label(state.agent_state)),
            kv("serving_state", component_state_label(state.serving_state)),
            kv("observability_state", state.state.as_str().to_string()),
            kv("active_deployment", state.active_deployment.clone()),
            kv("backend", state.backend.clone()),
            kv(
                "missed_heartbeat_count",
                state.missed_heartbeat_count.to_string(),
            ),
            kv(
                "missed_deadline_rate",
                format_float(state.missed_deadline_rate),
            ),
            kv("queue_depth", state.queue_depth.to_string()),
            kv("last_error_code", error_code_label(state.last_error_code)),
        ],
    };
    DiagnosticArray {
        topic: topic.to_string(),
        status: vec![status],
    }
}

fn kv(key: &str, value: String) -> DiagnosticKeyValue {
    DiagnosticKeyValue {
        key: key.to_string(),
        value,
    }
}

fn state_message(state: ObservabilityState) -> String {
    match state {
        ObservabilityState::Ready => "ready".into(),
        ObservabilityState::Degraded => "degraded".into(),
        ObservabilityState::Failed => "failed".into(),
        ObservabilityState::NoHeartbeat => "no_heartbeat".into(),
    }
}

fn component_state_label(s: ComponentState) -> String {
    match s {
        ComponentState::Ready => "ready".into(),
        ComponentState::Degraded => "degraded".into(),
        ComponentState::Failed => "failed".into(),
        ComponentState::Unknown => "unknown".into(),
    }
}

fn error_code_label(code: Option<ErrorCode>) -> String {
    match code {
        Some(ErrorCode::ConfigInvalid) => "config_invalid".into(),
        Some(ErrorCode::LoadFailed) => "load_failed".into(),
        Some(ErrorCode::NotReady) => "not_ready".into(),
        Some(ErrorCode::ShapeMismatch) => "shape_mismatch".into(),
        Some(ErrorCode::Unsupported) => "unsupported".into(),
        Some(ErrorCode::OomError) => "oom_error".into(),
        Some(ErrorCode::Timeout) => "timeout".into(),
        Some(ErrorCode::InferenceFailed) => "inference_failed".into(),
        Some(ErrorCode::Internal) => "internal".into(),
        None => String::new(),
    }
}

fn format_float(v: f64) -> String {
    if v.is_finite() {
        format!("{v:.6}")
    } else {
        "0.000000".to_string()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{
        build_diagnostic_array, DiagnosticLevel, HealthPublisher, MockHealthPublisher,
        Ros2HealthPublisher,
    };
    use crate::config::Ros2HealthConfig;
    use crate::state::{AggregateState, ObservabilityState};
    use std::time::{Duration, Instant};
    use tensorplate_protocol::error::ErrorCode;
    use tensorplate_protocol::worker_status::ComponentState;

    fn cfg() -> Ros2HealthConfig {
        Ros2HealthConfig {
            enabled: true,
            topic: "/tensorplate/health".into(),
            interval_ms: 1_000,
            runtime: super::super::config::Ros2Runtime::Mock,
        }
    }

    fn agg(state: ObservabilityState) -> AggregateState {
        AggregateState {
            state,
            previous_state: ObservabilityState::Ready,
            agent_state: ComponentState::Ready,
            serving_state: ComponentState::Ready,
            active_deployment: "deploy-1".into(),
            backend: "mock".into(),
            missed_heartbeat_count: 0,
            missed_deadline_rate: 0.0,
            queue_depth: 0,
            last_error_code: None,
            last_transition_at: None,
            last_event_sequence: None,
            last_heartbeat_age_ms: None,
            last_periodic_emit_at: None,
        }
    }

    #[test]
    fn level_mapping_matches_roadmap() {
        assert_eq!(
            DiagnosticLevel::from_state(ObservabilityState::Ready),
            DiagnosticLevel::Ok
        );
        assert_eq!(
            DiagnosticLevel::from_state(ObservabilityState::Degraded),
            DiagnosticLevel::Warn
        );
        assert_eq!(
            DiagnosticLevel::from_state(ObservabilityState::Failed),
            DiagnosticLevel::Error
        );
        assert_eq!(
            DiagnosticLevel::from_state(ObservabilityState::NoHeartbeat),
            DiagnosticLevel::Stale
        );
    }

    #[test]
    fn diagnostic_array_contains_required_keys() {
        let arr = build_diagnostic_array("/tensorplate/health", &agg(ObservabilityState::Ready));
        assert_eq!(arr.topic, "/tensorplate/health");
        assert_eq!(arr.status.len(), 1);
        let s = &arr.status[0];
        assert_eq!(s.name, "tensorplate/runtime");
        let keys: Vec<&str> = s.values.iter().map(|kv| kv.key.as_str()).collect();
        for required in [
            "agent_state",
            "serving_state",
            "observability_state",
            "active_deployment",
            "backend",
            "missed_heartbeat_count",
            "missed_deadline_rate",
            "queue_depth",
            "last_error_code",
        ] {
            assert!(keys.contains(&required), "missing key {required}");
        }
    }

    #[test]
    fn publish_if_due_emits_on_state_change_then_interval() {
        let publisher = Ros2HealthPublisher::from_config(&cfg()).expect("enabled");
        let now = Instant::now();
        assert!(publisher.publish_if_due(&agg(ObservabilityState::Ready), now));
        // Same state immediately: not due.
        assert!(!publisher.publish_if_due(&agg(ObservabilityState::Ready), now));
        // Different state: due.
        assert!(publisher.publish_if_due(&agg(ObservabilityState::Failed), now));
        // Same state again: not due until interval elapses.
        let later = now + Duration::from_millis(500);
        assert!(!publisher.publish_if_due(&agg(ObservabilityState::Failed), later));
        let interval_later = now + Duration::from_millis(1_100);
        assert!(publisher.publish_if_due(&agg(ObservabilityState::Failed), interval_later));
    }

    #[test]
    fn disabled_config_yields_no_publisher() {
        let mut c = cfg();
        c.enabled = false;
        assert!(Ros2HealthPublisher::from_config(&c).is_none());
    }

    #[test]
    fn last_error_code_is_serialized() {
        let mut a = agg(ObservabilityState::Failed);
        a.last_error_code = Some(ErrorCode::OomError);
        let arr = build_diagnostic_array("/t", &a);
        let kv = arr.status[0]
            .values
            .iter()
            .find(|v| v.key == "last_error_code")
            .expect("last_error_code present");
        assert_eq!(kv.value, "oom_error");
    }

    #[test]
    fn mock_publisher_records_emissions() {
        let mock = MockHealthPublisher::new("/topic");
        let arr = build_diagnostic_array("/topic", &agg(ObservabilityState::Ready));
        mock.publish(&arr);
        mock.publish(&arr);
        assert_eq!(mock.published(), 2);
        assert_eq!(mock.drain().len(), 2);
    }
}
