// SPDX-License-Identifier: Apache-2.0
//
// An agent whose coordinator runs the member registry over stand-in worker
// processes. A stand-in keeps a worker's rules: it starts only on a port no
// running worker is bound to, it exits when it is told to stop or killed
// and never runs again, and `/health` on its port answers only while it
// runs.

#![allow(
    dead_code,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc
)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use tensorplate_agent::coordinator::Coordinator;
use tensorplate_agent::error::{AgentError, AgentResult};
use tensorplate_agent::registry::{
    durable_generations, ControlContact, HealthProbe, LaunchSpec, MemberPorts, MemberProcess,
    MemberRegistry, ProcessLauncher, RegistrySettings,
};
use tensorplate_agent::supervision::FakeClock;
use tensorplate_agent::worker::WorkerControl;
use tensorplate_protocol::agent_state::decode_agent_state;

use super::common::{vision_bundle, Harness};

/// The two ports the agent's workers are given, in the order they are leased.
pub const PORTS: [u16; 2] = [41_280, 41_281];

/// The member of the resident set [`RegistryAgent::commit_member_as_a_set`]
/// commits, the deployment before it, and the generations they take.
pub const MEMBER: (&str, u64) = ("vision-v2", 2);
pub const PREDECESSOR: (&str, u64) = ("vision-v1", 1);

/// How long a test waits for something it holds to begin.
pub const HELD: Duration = Duration::from_secs(10);

type Hold = (mpsc::Sender<()>, mpsc::Receiver<()>);

/// Says it has begun, then waits to be let finish.
fn held(hold: &Mutex<Option<Hold>>) {
    let hold = hold.lock().unwrap().take();
    if let Some((started, release)) = hold {
        started.send(()).unwrap();
        // The test ending without a release lets it finish too.
        let _ = release.recv();
    }
}

fn hold(slot: &Mutex<Option<Hold>>) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (started, has_started) = mpsc::channel();
    let (release, released) = mpsc::channel();
    *slot.lock().unwrap() = Some((started, released));
    (has_started, release)
}

/// One started worker.
pub struct Worker {
    pub spec: LaunchSpec,
    pub port: u16,
    model: String,
    exited: AtomicBool,
    loading: AtomicBool,
    contact: Mutex<ControlContact>,
    hold_stop: Mutex<Option<Hold>>,
}

impl Worker {
    pub fn exit(&self) {
        self.exited.store(true, Ordering::SeqCst);
    }
    pub fn running(&self) -> bool {
        !self.exited.load(Ordering::SeqCst)
    }
    pub fn finish_loading(&self) {
        self.loading.store(false, Ordering::SeqCst);
    }
    pub fn set_contact(&self, contact: ControlContact) {
        *self.contact.lock().unwrap() = contact;
    }
    /// Hold this worker's answer to a stop request; returns what tells
    /// that the request arrived and what lets the worker exit.
    pub fn hold_stop(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        hold(&self.hold_stop)
    }
}

struct Process(Arc<Worker>);

impl MemberProcess for Process {
    fn exited(&mut self) -> AgentResult<bool> {
        Ok(!self.0.running())
    }
    fn startup_failure(&mut self) -> AgentError {
        AgentError::WorkerExited("exit status: 1".into())
    }
    fn control(&self) -> ControlContact {
        *self.0.contact.lock().unwrap()
    }
    fn terminate(&mut self) {
        held(&self.0.hold_stop);
        self.0.exit();
    }
    fn kill(&mut self) {
        self.0.exit();
    }
}

/// Starts the stand-in workers and answers for their `/health`.
#[derive(Default)]
pub struct Workers {
    started: Mutex<Vec<Arc<Worker>>>,
    /// Workers started from now on are loading until told otherwise.
    pub start_loading: AtomicBool,
    hold_probe: Mutex<Option<Hold>>,
}

impl Workers {
    pub fn count(&self) -> usize {
        self.started.lock().unwrap().len()
    }
    pub fn last(&self) -> Arc<Worker> {
        self.started
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("a worker was started")
    }
    fn running_on(&self, port: u16) -> Option<Arc<Worker>> {
        let started = self.started.lock().unwrap();
        started
            .iter()
            .find(|worker| worker.port == port && worker.running())
            .cloned()
    }
    /// Hold the next `/health` read; returns what tells that it began and
    /// what lets it answer.
    pub fn hold_probe(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        hold(&self.hold_probe)
    }
}

impl ProcessLauncher for Workers {
    fn launch(&self, spec: &LaunchSpec) -> AgentResult<Box<dyn MemberProcess>> {
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&spec.config_path)?).expect("a JSON config");
        let port =
            u16::try_from(config["bind"]["port"].as_u64().expect("bind.port")).expect("a port");
        if self.running_on(port).is_some() {
            return Err(AgentError::WorkerControl(format!(
                "port {port} is bound by a running worker"
            )));
        }
        let worker = Arc::new(Worker {
            spec: spec.clone(),
            port,
            model: config["deployment"]["model"]["model_id"]
                .as_str()
                .expect("model_id")
                .into(),
            exited: AtomicBool::new(false),
            loading: AtomicBool::new(self.start_loading.load(Ordering::SeqCst)),
            contact: Mutex::new(ControlContact::Reporting),
            hold_stop: Mutex::new(None),
        });
        self.started.lock().unwrap().push(worker.clone());
        Ok(Box::new(Process(worker)))
    }
}

impl HealthProbe for Workers {
    fn ready_model(&self, port: u16, _timeout: Duration) -> AgentResult<Option<String>> {
        held(&self.hold_probe);
        let Some(worker) = self.running_on(port) else {
            return Err(AgentError::WorkerControl(format!(
                "connection refused on port {port}"
            )));
        };
        Ok((!worker.loading.load(Ordering::SeqCst)).then(|| worker.model.clone()))
    }
}

/// A coordinator over the harness's state, running a member registry.
pub struct RegistryAgent {
    pub workers: Arc<Workers>,
    pub clock: Arc<FakeClock>,
    pub registry: Arc<MemberRegistry>,
    pub coord: Arc<Coordinator>,
}

impl RegistryAgent {
    pub fn start(h: &Harness) -> Self {
        let workers = Arc::new(Workers::default());
        let clock = Arc::new(FakeClock::new());
        let settings = RegistrySettings {
            bind_host: "127.0.0.1".into(),
            ports: PORTS
                .map(|http| MemberPorts { http, stream: None })
                .to_vec(),
            config_dir: h.td.path().join("worker-configs"),
            use_mock_session: true,
            status_poll_interval: Duration::from_millis(1),
            worker_drain: Duration::from_millis(30),
            stop_deadline: Duration::from_millis(60),
            launch_grace: HELD,
            restart_warm_timeout: Duration::from_secs(30),
        };
        let registry = Arc::new(
            MemberRegistry::new(
                settings,
                workers.clone(),
                workers.clone(),
                durable_generations(h.store.clone()),
            )
            .with_clock(clock.clone()),
        );
        let coord = Arc::new(Coordinator::new(
            h.config.clone(),
            h.store.clone(),
            registry.clone(),
        ));
        Self {
            workers,
            clock,
            registry,
            coord,
        }
    }

    /// One supervision step, `after` later on the registry's clock.
    pub fn step(&self, after: Duration) {
        self.clock.advance(after);
        self.registry.supervise();
    }

    /// Deploy [`PREDECESSOR`], then [`MEMBER`] over it.
    pub fn deploy_member(&self, h: &Harness) {
        for (id, _) in [PREDECESSOR, MEMBER] {
            let bundle = vision_bundle(h.td.path(), id);
            self.coord
                .deploy(id, &bundle, std::collections::BTreeMap::new(), None, None)
                .expect("deploy");
        }
    }

    /// Replace the singleton record of what [`Self::deploy_member`] left
    /// by the resident set a state fixture records for the same history:
    /// [`MEMBER`] serving, [`PREDECESSOR`] retained. The member's unary
    /// endpoint is the port its worker was given.
    pub fn commit_member_as_a_set(&self, h: &Harness) {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../protocol/rust/tests/fixtures/agent_state_0_2_restore_step.json");
        let recorded = decode_agent_state(&std::fs::read_to_string(path).expect("read fixture"))
            .expect("fixture decodes");
        let mut set = recorded.resident_set.expect("the fixture records a set");
        assert_eq!(
            (
                set.members[0].deployment_id.as_str(),
                set.members[0].generation
            ),
            MEMBER
        );
        let port = self.workers.last().port;
        set.endpoint_map[0].unary_endpoint = Some(format!("http://127.0.0.1:{port}"));
        h.store
            .update(|state| {
                assert_eq!(state.next_generation, recorded.next_generation);
                state.active = None;
                state.previous_active = None;
                state.resident_set = Some(set);
                Ok(())
            })
            .expect("commit the set");
    }
}
