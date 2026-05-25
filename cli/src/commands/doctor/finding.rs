// SPDX-License-Identifier: Apache-2.0
//
// V01-E11-F03-T01: stable doctor finding taxonomy.
//
// Finding IDs are part of the v0.1.0 CLI contract — the release validation
// validation harness scripts assert on these strings. New checks may
// be added; existing IDs must not be repurposed, renamed, or have
// their severity semantics changed.

use serde::Serialize;

/// Stable identifier for a doctor check. Maps to the `id` field in the
/// JSON output payload.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingId {
    CliVersion,
    ProfileMode,
    AgentSocket,
    AgentReachable,
    AgentStatusShape,
    AgentState,
    ActiveDeployment,
    WorkerState,
    WorkerCrashLoop,
    HostFacts,
    HostOs,
    PythonPytorchBackend,
    /// packaging: PyTorch runtime probe status, reported separately
    /// from the backend descriptor so operators can distinguish a
    /// missing backend package from a present package whose Python
    /// environment lacks PyTorch.
    PythonPytorchRuntime,
    TensorrtRuntime,
    LibtorchRuntime,
    /// packaging: CUDA runtime presence on the device.
    CudaRuntime,
    Ros2HealthStub,
    ObservabilitySnapshot,
    /// packaging install probes:
    CorePackages,
    PathLayout,
    ConfigFiles,
    ConfigEndpoints,
    AgentSystemdUnit,
    AgentServiceState,
    ObservabilitySystemdUnit,
    ObservabilityServiceState,
    ServingSystemdAbsent,
    ServingBinaryInstalled,
    BackendDescriptor,
}

impl FindingId {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        // We serialize via serde to keep the JSON form consistent. The
        // string form is exposed here so the human renderer can show
        // the same identifier.
        match self {
            Self::CliVersion => "cli_version",
            Self::ProfileMode => "profile_mode",
            Self::AgentSocket => "agent_socket",
            Self::AgentReachable => "agent_reachable",
            Self::AgentStatusShape => "agent_status_shape",
            Self::AgentState => "agent_state",
            Self::ActiveDeployment => "active_deployment",
            Self::WorkerState => "worker_state",
            Self::WorkerCrashLoop => "worker_crash_loop",
            Self::HostFacts => "host_facts",
            Self::HostOs => "host_os",
            Self::PythonPytorchBackend => "python_pytorch_backend",
            Self::PythonPytorchRuntime => "python_pytorch_runtime",
            Self::TensorrtRuntime => "tensorrt_runtime",
            Self::LibtorchRuntime => "libtorch_runtime",
            Self::CudaRuntime => "cuda_runtime",
            Self::Ros2HealthStub => "ros2_health_stub",
            Self::ObservabilitySnapshot => "observability_snapshot",
            Self::CorePackages => "core_packages",
            Self::PathLayout => "path_layout",
            Self::ConfigFiles => "config_files",
            Self::ConfigEndpoints => "config_endpoints",
            Self::AgentSystemdUnit => "agent_systemd_unit",
            Self::AgentServiceState => "agent_service_state",
            Self::ObservabilitySystemdUnit => "observability_systemd_unit",
            Self::ObservabilityServiceState => "observability_service_state",
            Self::ServingSystemdAbsent => "serving_systemd_absent",
            Self::ServingBinaryInstalled => "serving_binary_installed",
            Self::BackendDescriptor => "backend_descriptor",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingStatus {
    /// Check succeeded. Serialized as `"ok"` for release validation scripts
    /// that grep on a single literal token.
    #[serde(rename = "ok")]
    Pass,
    Fail,
    Missing,
    Unsupported,
    Skipped,
    Warning,
}

impl FindingStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "ok",
            Self::Fail => "fail",
            Self::Missing => "missing",
            Self::Unsupported => "unsupported",
            Self::Skipped => "skipped",
            Self::Warning => "warning",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Finding {
    pub id: FindingId,
    pub status: FindingStatus,
    pub severity: Severity,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Finding {
    fn make(
        id: FindingId,
        status: FindingStatus,
        severity: Severity,
        message: impl Into<String>,
        hint: Option<String>,
    ) -> Self {
        Self {
            id,
            status,
            severity,
            message: message.into(),
            hint,
        }
    }

    pub fn ok(
        id: FindingId,
        severity: Severity,
        message: impl Into<String>,
        hint: Option<String>,
    ) -> Self {
        Self::make(id, FindingStatus::Pass, severity, message, hint)
    }

    pub fn fail(
        id: FindingId,
        severity: Severity,
        message: impl Into<String>,
        hint: Option<String>,
    ) -> Self {
        Self::make(id, FindingStatus::Fail, severity, message, hint)
    }

    pub fn warn(
        id: FindingId,
        severity: Severity,
        message: impl Into<String>,
        hint: Option<String>,
    ) -> Self {
        Self::make(id, FindingStatus::Warning, severity, message, hint)
    }

    pub fn missing(
        id: FindingId,
        severity: Severity,
        message: impl Into<String>,
        hint: Option<String>,
    ) -> Self {
        Self::make(id, FindingStatus::Missing, severity, message, hint)
    }

    pub fn unsupported(
        id: FindingId,
        severity: Severity,
        message: impl Into<String>,
        hint: Option<String>,
    ) -> Self {
        Self::make(id, FindingStatus::Unsupported, severity, message, hint)
    }

    pub fn skipped(
        id: FindingId,
        severity: Severity,
        message: impl Into<String>,
        hint: Option<String>,
    ) -> Self {
        Self::make(id, FindingStatus::Skipped, severity, message, hint)
    }

    #[must_use]
    pub fn status_label(&self) -> &'static str {
        self.status.as_str()
    }

    #[must_use]
    pub fn severity_label(&self) -> &'static str {
        self.severity.as_str()
    }

    #[must_use]
    pub fn id_label(&self) -> &'static str {
        self.id.as_str()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn finding_id_strings_are_stable() {
        let cases = [
            (FindingId::CliVersion, "cli_version"),
            (FindingId::AgentReachable, "agent_reachable"),
            (FindingId::Ros2HealthStub, "ros2_health_stub"),
        ];
        for (id, expected) in cases {
            assert_eq!(id.as_str(), expected);
        }
    }

    #[test]
    fn finding_status_strings_match_schema() {
        assert_eq!(FindingStatus::Pass.as_str(), "ok");
        assert_eq!(FindingStatus::Fail.as_str(), "fail");
        assert_eq!(FindingStatus::Missing.as_str(), "missing");
    }

    #[test]
    fn finding_serializes_with_expected_shape() {
        let f = Finding::warn(
            FindingId::AgentSocket,
            Severity::Warning,
            "socket missing",
            Some("start the agent".into()),
        );
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["id"], "agent_socket");
        assert_eq!(v["status"], "warning");
        assert_eq!(v["severity"], "warning");
        assert_eq!(v["hint"], "start the agent");
    }
}
