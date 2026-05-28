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
};
