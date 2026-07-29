// SPDX-License-Identifier: Apache-2.0

//! `tensorplate-agent` (V01-E08) — Rust device agent.
//!
//! This crate owns the management-plane: the durable desired-state store,
//! the deploy transaction state machine, bundle verification, and the
//! agent <-> serving-worker control client. The CLI (V01-E11) and any
//! other operator tooling speaks the local control API documented in
//! [`tensorplate_protocol::agent_control`]. The agent never serves
//! inference traffic and never mutates the serving worker's data path
//! directly — it stages a verified bundle and asks the worker to
//! prepare/warm/promote through the typed [`worker::WorkerControl`]
//! interface.
//!
//! The crate exposes its modules through a thin `lib.rs` so integration
//! tests under `agent/tests/` can drive the agent without spawning the
//! binary.
//!
//! ## Layering
//!
//! ```text
//!   cli  ─── ControlRequest/Response over UDS ───>  agent  ─── WorkerControl  ───>  serving_worker
//!                                                     │
//!                                                     ├── DurableStateStore
//!                                                     ├── BundleVerifier
//!                                                     ├── TransactionCoordinator
//!                                                     └── RecoveryPlanner
//! ```
//!
//! All durable mutation flows through the `state::StateStore` so a
//! mid-transaction crash is recoverable from disk at the next startup.

#![forbid(unsafe_code)]

pub mod backend_detection;
pub mod bundle;
pub mod config;
pub mod control;
pub mod coordinator;
pub mod error;
pub mod platform_admission;
pub mod quarantine;
pub mod recovery;
pub mod rollback;
pub mod server;
pub mod state;
pub mod supervision;
pub mod transaction;
pub mod worker;

pub use config::{AgentConfig, BackendCapability, ControlTransport};
pub use coordinator::Coordinator;
pub use error::{AgentError, AgentResult};
pub use platform_admission::PlatformAdmission;
pub use server::Server;
pub use state::{StateStore, StateUpdate};
pub use supervision::{
    DesiredWorker, SupervisionFault, SupervisionPhase, SupervisionStatus, SupervisorConfig,
    TickOutcome, WorkerSupervisor,
};
pub use worker::{MockWorkerControl, WorkerControl, WorkerEvent};
