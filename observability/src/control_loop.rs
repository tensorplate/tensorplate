// SPDX-License-Identifier: Apache-2.0
//
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

// V01-E12-F05: Control-loop jitter and frequency-stability aggregator.
//
// The aggregator consumes successful action output events (timestamped
// monotonically by the producer) and exports a rolling 60 s summary
// matching `protocol/schemas/control_loop_metrics.json`. The formulas
// are pinned to the v0.1 roadmap:
//
//   - `target_period_ms = 1000 / control_frequency_hz`
//   - `interval_ms = t_i - t_{i-1}` for consecutive outputs
//   - `jitter_ms = abs(interval_ms - target_period_ms)`
//   - `instant_frequency_hz = 1000 / interval_ms`
//   - `frequency_error_pct = abs(mean - control) / control * 100`
//
// All timing is monotonic. Tests drive the aggregator with
// `observability::clock::FakeClock` so the rolling-window eviction
// behaviour is deterministic.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use tensorplate_protocol::{ControlLoopEvent, ControlLoopLabels, ControlLoopSummary};

use crate::clock::MonotonicClock;
use crate::error::{ObservabilityError, ObservabilityResult};
/// Maximum number of samples retained in the rolling window. Bounded
/// so the aggregator memory cost is fixed even under high-frequency
/// producers; samples older than `window` are evicted from the front.
pub const MAX_CONTROL_LOOP_SAMPLES: usize = 4_096;

/// Configuration for [`ControlLoopAggregator`].
#[derive(Clone, Debug)]
pub struct ControlLoopAggregatorConfig {
    /// Target control frequency. Must be finite and `> 0` for the
    /// aggregator to record samples.
    pub control_frequency_hz: f64,
    /// Length of the rolling window.
    pub window: Duration,
    /// Grace window added on top of `target_period_ms` before an
    /// interval is counted as a missed deadline. Defaults to 25% of the
    /// target period.
    pub grace_ms: Option<f64>,
    /// Bounded label set carried on emitted events.
    pub labels: ControlLoopLabels,
}

impl ControlLoopAggregatorConfig {
    /// Construct a config with the default 60s rolling window.
    #[must_use]
    pub fn new(control_frequency_hz: f64, labels: ControlLoopLabels) -> Self {
        Self {
            control_frequency_hz,
            window: Duration::from_secs(60),
            grace_ms: None,
            labels,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Sample {
    /// Absolute monotonic instant at which the action output landed.
    at: Instant,
    /// Inter-output interval in milliseconds (vs. previous sample).
    interval_ms: f64,
    /// Jitter in milliseconds.
    jitter_ms: f64,
    /// Whether this interval exceeded the deadline (`target + grace`).
    missed_deadline: bool,
}

/// V01-E12-F05 control-loop aggregator.
pub struct ControlLoopAggregator {
    config: ControlLoopAggregatorConfig,
    target_period_ms: f64,
    grace_ms: f64,
    last: Option<Instant>,
    samples: VecDeque<Sample>,
    /// Counter of intervals that were rejected because they were zero,
    /// negative, or otherwise invalid (e.g. clock skew). The
    /// aggregator never panics on bad input.
    invalid_intervals: u64,
}

impl ControlLoopAggregator {
    /// Build an aggregator. Returns
    /// [`ObservabilityError::Config`] when the configured frequency is
    /// not finite or `> 0`.
    pub fn new(config: ControlLoopAggregatorConfig) -> ObservabilityResult<Self> {
        if !config.control_frequency_hz.is_finite() || config.control_frequency_hz <= 0.0 {
            return Err(ObservabilityError::Config(
                "control_frequency_hz must be finite and > 0".into(),
            ));
        }
        let target_period_ms = 1000.0 / config.control_frequency_hz;
        let grace_ms = config.grace_ms.unwrap_or(target_period_ms * 0.25);
        Ok(Self {
            config,
            target_period_ms,
            grace_ms,
            last: None,
            samples: VecDeque::with_capacity(MAX_CONTROL_LOOP_SAMPLES),
            invalid_intervals: 0,
        })
    }

    /// Record a successful action output that landed at `at`. `at`
    /// must be monotonic; samples with a non-positive interval to the
    /// previous sample increment the invalid counter and are ignored.
    pub fn record_output(&mut self, at: Instant) {
        if let Some(prev) = self.last {
            let Some(interval) = at.checked_duration_since(prev) else {
                self.invalid_intervals = self.invalid_intervals.saturating_add(1);
                return;
            };
            let interval_ms = duration_to_ms(interval);
            if !interval_ms.is_finite() || interval_ms <= 0.0 {
                self.invalid_intervals = self.invalid_intervals.saturating_add(1);
                return;
            }
            let jitter_ms = (interval_ms - self.target_period_ms).abs();
            let missed_deadline = interval_ms > self.target_period_ms + self.grace_ms;
            if self.samples.len() == MAX_CONTROL_LOOP_SAMPLES {
                self.samples.pop_front();
            }
            self.samples.push_back(Sample {
                at,
                interval_ms,
                jitter_ms,
                missed_deadline,
            });
            self.evict_outside_window(at);
        }
        self.last = Some(at);
    }

    fn evict_outside_window(&mut self, now: Instant) {
        let Some(cutoff) = now.checked_sub(self.config.window) else {
            return;
        };
        while let Some(front) = self.samples.front() {
            if front.at < cutoff {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    /// Summarise the current rolling window. Returns
    /// `(summary, samples_in_window)`.
    #[must_use]
    pub fn summary(&self, clock: &dyn MonotonicClock) -> ControlLoopSummary {
        // Snapshot the relevant samples without mutating the
        // aggregator; eviction happens on `record_output`.
        let now = clock.now();
        let cutoff = now.checked_sub(self.config.window);
        let in_window: Vec<Sample> = self
            .samples
            .iter()
            .copied()
            .filter(|s| cutoff.map_or(true, |c| s.at >= c))
            .collect();
        let samples = in_window.len() as u64;
        let missed_deadlines = in_window.iter().filter(|s| s.missed_deadline).count() as u64;
        let missed_deadline_rate = if samples == 0 {
            0.0
        } else {
            (missed_deadlines as f64) / (samples as f64)
        };
        if samples == 0 {
            return ControlLoopSummary {
                samples: 0,
                missed_deadlines: 0,
                missed_deadline_rate: 0.0,
                ..ControlLoopSummary::default()
            };
        }
        let mut jitter: Vec<f64> = in_window.iter().map(|s| s.jitter_ms).collect();
        jitter.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let jitter_p50 = percentile(&jitter, 50.0);
        let jitter_p95 = percentile(&jitter, 95.0);
        let jitter_p99 = percentile(&jitter, 99.0);
        let jitter_max = jitter
            .last()
            .copied()
            .unwrap_or_else(|| jitter.first().copied().unwrap_or(0.0));

        // Mean frequency from the sample intervals. Equivalent to
        // `output_count / elapsed_seconds` when the window is full;
        // this interval-based formulation also yields the expected
        // control_frequency for sparse windows during validation.
        let mean_interval_ms =
            in_window.iter().map(|s| s.interval_ms).sum::<f64>() / (in_window.len() as f64);
        let mean_frequency_hz = if mean_interval_ms > 0.0 {
            1000.0 / mean_interval_ms
        } else {
            0.0
        };
        let instant: Vec<f64> = in_window.iter().map(|s| 1000.0 / s.interval_ms).collect();
        let frequency_stddev = stddev(&instant);
        let frequency_error_pct = if self.config.control_frequency_hz > 0.0 {
            (mean_frequency_hz - self.config.control_frequency_hz).abs()
                / self.config.control_frequency_hz
                * 100.0
        } else {
            0.0
        };

        ControlLoopSummary {
            samples,
            missed_deadlines,
            missed_deadline_rate,
            jitter_p50_ms: Some(jitter_p50),
            jitter_p95_ms: Some(jitter_p95),
            jitter_p99_ms: Some(jitter_p99),
            jitter_max_ms: Some(jitter_max),
            mean_frequency_hz: Some(mean_frequency_hz),
            frequency_stddev_hz: Some(frequency_stddev),
            frequency_error_pct: Some(frequency_error_pct),
        }
    }

    /// Build a wire-format event for the current window using the
    /// supplied clock for the timestamp.
    #[must_use]
    pub fn event(&self, clock: &dyn MonotonicClock, epoch: Instant) -> ControlLoopEvent {
        let ts = clock
            .now()
            .saturating_duration_since(epoch)
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64;
        ControlLoopEvent::new(
            ts,
            self.config.control_frequency_hz,
            self.config.labels.clone(),
            self.summary(clock),
        )
    }

    /// Number of intervals dropped because they were zero, negative,
    /// or otherwise invalid. Operator-visible through the snapshot
    /// projection.
    pub fn invalid_intervals(&self) -> u64 {
        self.invalid_intervals
    }

    /// Number of samples retained in the rolling window.
    pub fn samples_in_window(&self) -> usize {
        self.samples.len()
    }

    /// Pinned target period in milliseconds.
    pub fn target_period_ms(&self) -> f64 {
        self.target_period_ms
    }
}

fn duration_to_ms(d: Duration) -> f64 {
    let secs = d.as_secs() as f64;
    let nanos = f64::from(d.subsec_nanos());
    secs * 1_000.0 + nanos / 1_000_000.0
}

fn percentile(sorted: &[f64], pct: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let rank = (pct / 100.0) * ((sorted.len() - 1) as f64);
    let lower = rank.floor() as usize;
    let upper = rank.ceil() as usize;
    if lower == upper {
        return sorted[lower];
    }
    let weight = rank - lower as f64;
    sorted[lower] * (1.0 - weight) + sorted[upper] * weight
}

fn stddev(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let mean: f64 = values.iter().sum::<f64>() / values.len() as f64;
    let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64;
    var.sqrt()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{ControlLoopAggregator, ControlLoopAggregatorConfig};
    use crate::clock::{FakeClock, MonotonicClock};
    use std::time::Duration;
    use tensorplate_protocol::ControlLoopLabels;

    fn labels() -> ControlLoopLabels {
        ControlLoopLabels::new("/v1/act", "vla", "smolvla-tiny", "tensorrt")
    }

    fn build(freq_hz: f64) -> (ControlLoopAggregator, FakeClock) {
        let config = ControlLoopAggregatorConfig::new(freq_hz, labels());
        let aggregator = ControlLoopAggregator::new(config).expect("valid");
        (aggregator, FakeClock::new())
    }

    #[test]
    fn rejects_non_positive_frequency() {
        let cfg = ControlLoopAggregatorConfig::new(0.0, labels());
        assert!(ControlLoopAggregator::new(cfg).is_err());
        let cfg = ControlLoopAggregatorConfig::new(f64::NAN, labels());
        assert!(ControlLoopAggregator::new(cfg).is_err());
    }

    #[test]
    fn stable_frequency_yields_zero_jitter_and_error() {
        let (mut agg, clock) = build(30.0);
        // 30Hz => 33.333ms period. Emit 20 samples on a stable cadence.
        for _ in 0..20 {
            agg.record_output(clock.now());
            clock.advance(Duration::from_micros(33_333));
        }
        let summary = agg.summary(&clock);
        assert_eq!(summary.samples, 19);
        assert!(summary.jitter_p99_ms.unwrap() < 0.5);
        assert!(summary.frequency_error_pct.unwrap() < 1.0);
        assert!(summary.missed_deadline_rate < 0.01);
    }

    #[test]
    fn jittered_frequency_increases_p95_and_max() {
        let (mut agg, clock) = build(30.0);
        let period_us = 33_333u64;
        for i in 0..40 {
            agg.record_output(clock.now());
            // Inject 10ms jitter on every fourth sample.
            let delay = if i % 4 == 0 {
                period_us + 10_000
            } else {
                period_us
            };
            clock.advance(Duration::from_micros(delay));
        }
        let summary = agg.summary(&clock);
        let p95 = summary.jitter_p95_ms.unwrap();
        let max = summary.jitter_max_ms.unwrap();
        assert!(max >= 9.0, "max jitter expected >=9ms, got {max}");
        assert!(p95 >= 1.0, "p95 jitter expected >=1ms, got {p95}");
    }

    #[test]
    fn missed_deadline_counter_advances_when_interval_exceeds_grace() {
        let (mut agg, clock) = build(30.0);
        // Two stable outputs.
        agg.record_output(clock.now());
        clock.advance(Duration::from_micros(33_333));
        agg.record_output(clock.now());
        // Then a 200ms gap — well past the 25% grace.
        clock.advance(Duration::from_millis(200));
        agg.record_output(clock.now());
        let summary = agg.summary(&clock);
        assert_eq!(summary.missed_deadlines, 1);
    }

    #[test]
    fn rolling_window_evicts_old_samples() {
        let cfg = ControlLoopAggregatorConfig {
            control_frequency_hz: 10.0,
            window: Duration::from_millis(500),
            grace_ms: Some(20.0),
            labels: labels(),
        };
        let mut agg = ControlLoopAggregator::new(cfg).expect("valid");
        let clock = FakeClock::new();
        for _ in 0..20 {
            agg.record_output(clock.now());
            clock.advance(Duration::from_millis(50));
        }
        // After ~1s of activity, only the most recent 500ms are kept.
        let summary = agg.summary(&clock);
        assert!(summary.samples <= 12);
    }

    #[test]
    fn invalid_intervals_are_counted_not_recorded() {
        let (mut agg, clock) = build(30.0);
        agg.record_output(clock.now());
        // Recording at the same instant produces a 0-ms interval.
        agg.record_output(clock.now());
        assert_eq!(agg.invalid_intervals(), 1);
        assert_eq!(agg.samples_in_window(), 0);
        clock.advance(Duration::from_micros(33_333));
        agg.record_output(clock.now());
        assert_eq!(agg.samples_in_window(), 1);
        let summary = agg.summary(&clock);
        assert_eq!(summary.samples, 1);
        assert!(summary.jitter_max_ms.unwrap() < 0.001);
    }
}
