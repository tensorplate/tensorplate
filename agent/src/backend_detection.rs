// SPDX-License-Identifier: Apache-2.0
//
// V01-E14-F05: backend availability probing.
//
// The probe itself lives in [`tensorplate_protocol::backend_probe`] so
// the CLI doctor (V01-E14-F06) and the agent share exactly one
// implementation. This module re-exports the public surface under the
// historical `tensorplate_agent::backend_detection` path so existing
// agent callers keep working.

pub use tensorplate_protocol::backend_probe::{
    probe_backend, probe_python_pytorch, BackendProbeReport, BackendProbeState, ProbeOptions,
};
