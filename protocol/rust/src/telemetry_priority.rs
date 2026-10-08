// SPDX-License-Identifier: Apache-2.0
//
// Delivery class shared by `log_event.json` and `metric_event.json`.

use serde::{Deserialize, Serialize};

/// Delivery class of a log or metric event, independent of its level or
/// kind. A producer that sheds telemetry by class drops only
/// [`TelemetryPriority::Diagnostic`], and never an event with no class.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryPriority {
    Fatal,
    Safety,
    State,
    Diagnostic,
}

impl TelemetryPriority {
    /// Every class, highest first, in schema order.
    pub const ALL: [Self; 4] = [Self::Fatal, Self::Safety, Self::State, Self::Diagnostic];

    /// Lowercase wire name; identical to the `serde` representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fatal => "fatal",
            Self::Safety => "safety",
            Self::State => "state",
            Self::Diagnostic => "diagnostic",
        }
    }
}
