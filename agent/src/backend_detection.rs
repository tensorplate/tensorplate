// SPDX-License-Identifier: Apache-2.0
//
// packaging: backend availability probing.
//
// The probe itself lives in [`tensorplate_protocol::backend_probe`] so
// the CLI doctor (packaging) and the agent share exactly one
// implementation. This module re-exports the public surface under the
// historical `tensorplate_agent::backend_detection` path so existing
// agent callers keep working.

pub use tensorplate_protocol::backend_probe::{
    probe_backend, probe_python_pytorch, BackendProbeReport, BackendProbeState, ProbeOptions,
    RunnerProfileProbe, ServingState,
};

/// Backends with no descriptor on disk. The agent never probes them, and
/// none can declare a runner profile.
/// Also decides which backend hints conflict with a runner profile at deploy.
pub const BACKENDS_WITHOUT_DESCRIPTOR: [&str; 4] = ["mock", "vitis_ai", "tensorrt", "libtorch"];
