// SPDX-License-Identifier: Apache-2.0
//
// V01-E09: Agent worker supervision.
//
// The supervision module owns the serving-worker lifecycle:
//
//   - `config`    — validated supervisor configuration (V01-E09-F01).
//   - `process`   — process launch/stop primitives + mock (V01-E09-F01).
//   - `readiness` — readiness probe + mock (V01-E09-F02).
//   - `policy`    — backoff + crash-loop policy (V01-E09-F03).
//   - `state`     — supervision state + status projection (V01-E09-F04).
//   - `event`     — bounded supervision-event sink (V01-E09-F05).
//   - `supervisor` — the `WorkerSupervisor` state machine that ties it
//                   together (V01-E09-F04 / F06 / F07).
//
// The supervisor is *driven* by `tick(now)`. The agent main binary calls
// `tick` on every iteration of its event loop; tests use a fake clock
// and a mock worker process to drive `tick` deterministically.

pub mod clock;
pub mod config;
pub mod event;
pub mod policy;
pub mod process;
pub mod readiness;
pub mod state;
pub mod supervisor;

pub use clock::{FakeClock, MonotonicClock, SystemMonotonicClock};
pub use config::{
    BackoffConfig, EventSinkConfig, RestartPolicy, RestartPolicyKind, SupervisorConfig,
    WorkerStdioMode,
};
pub use event::{NoopEventSink, RingEventSink, SupervisionEventPayload, SupervisionEventSink};
pub use policy::{BackoffDecision, BackoffScheduler, FailureClass, RestartCounters};
pub use process::{
    command_digest, ensure_supervisor_directories, wait_for_exit, ExitStatus, MockCall,
    MockProcessBehavior, MockWorkerProcess, PollOutcome, SystemWorkerProcess, WorkerHandle,
    WorkerProcess,
};
pub use readiness::{
    wait_until_ready, HttpReadinessProbe, MockReadinessProbe, ReadinessProbe, ReadinessSample,
};
pub use state::{
    pending_delay_ms, plan_reconcile, LastFailure, SupervisionPhase, SupervisionReconcileAction,
    SupervisionState, SupervisionStatus,
};
pub use supervisor::{
    fault_agent_state, fault_from_error, DesiredWorker, SupervisionFault, TickOutcome,
    WorkerSupervisor,
};
