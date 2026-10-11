// SPDX-License-Identifier: Apache-2.0
//
// What agent status says of the worker processes a member registry runs:
// the generation serving at `active.serving_url`, the generation a deploy
// is preparing, and how the process of a resident-set member's committed
// generation stands.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;
#[path = "support/registry_agent.rs"]
mod registry_agent;

use std::time::Duration;

use common::{vision_bundle, Harness};
use registry_agent::{RegistryAgent, HELD, MEMBER, PORTS, PREDECESSOR};
use tensorplate_agent::registry::ControlContact;
use tensorplate_agent::worker::WorkerControl;
use tensorplate_protocol::agent_control::{
    AgentStatus, ContactState, DeploymentSummary, MemberStatus, ProcessState, Readiness,
};
use tensorplate_protocol::worker_control::CandidateRef;

fn status(agent: &RegistryAgent) -> AgentStatus {
    agent.coord.status().expect("status")
}

fn active(agent: &RegistryAgent) -> DeploymentSummary {
    status(agent).active.expect("an active deployment")
}

fn url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/infer")
}

/// The only member of the committed set, as status reports it.
fn member(agent: &RegistryAgent) -> MemberStatus {
    let set = status(agent).resident_set.expect("a resident set");
    assert_eq!(set.members.len(), 1);
    assert_eq!(
        (
            set.members[0].deployment_id.as_str(),
            set.members[0].generation
        ),
        MEMBER
    );
    set.members[0].clone()
}

/// The four fields that describe a member's worker process.
fn process(
    member: &MemberStatus,
) -> (
    Option<ProcessState>,
    Option<Readiness>,
    Option<bool>,
    Option<ContactState>,
) {
    (
        member.process,
        member.readiness,
        member.draining,
        member.contact,
    )
}

const STOPPED: (
    Option<ProcessState>,
    Option<Readiness>,
    Option<bool>,
    Option<ContactState>,
) = (
    Some(ProcessState::Stopped),
    Some(Readiness::NotReady),
    Some(false),
    None,
);

#[test]
fn active_names_the_generation_serving_at_its_url_and_each_deploy_moves_both() {
    let h = Harness::new();
    let agent = RegistryAgent::start(&h);
    assert_eq!(status(&agent).active, None);

    agent.deploy_member(&h);

    let serving = active(&agent);
    assert_eq!(serving.deployment_id, MEMBER.0);
    assert_eq!(serving.generation, Some(MEMBER.1));
    assert_eq!(serving.serving_url, Some(url(PORTS[1])));
    let previous = status(&agent).previous_active.expect("the first deploy");
    assert_eq!(previous.deployment_id, PREDECESSOR.0);
    assert_eq!(
        previous.generation, None,
        "no process and no record names the generation of a retained deployment"
    );
}

#[test]
fn a_restarted_member_reports_its_new_generation_once_it_serves_again() {
    let h = Harness::new();
    let agent = RegistryAgent::start(&h);
    agent.deploy_member(&h);

    agent.workers.last().exit();
    agent.step(Duration::ZERO);
    let stopped = active(&agent);
    assert_eq!((stopped.generation, stopped.serving_url), (None, None));

    agent
        .workers
        .start_loading
        .store(true, std::sync::atomic::Ordering::SeqCst);
    agent.step(Duration::from_secs(1));
    agent.step(Duration::ZERO);
    assert_eq!(agent.workers.last().spec.member.generation, MEMBER.1 + 1);
    let loading = active(&agent);
    assert_eq!((loading.generation, loading.serving_url), (None, None));

    agent.workers.last().finish_loading();
    agent.step(Duration::from_millis(1));
    let restarted = active(&agent);
    assert_eq!(restarted.generation, Some(MEMBER.1 + 1));
    assert_eq!(restarted.serving_url, Some(url(PORTS[1])));
}

#[test]
fn the_generation_is_withheld_while_another_deployment_serves_than_the_recorded_one() {
    let h = Harness::new();
    let agent = RegistryAgent::start(&h);
    agent.deploy_member(&h);
    let recorded = h.store.snapshot().expect("state").active.expect("active");

    // A worker is promoted before the record that names it is written:
    // the registry as it stands between the two.
    let other = CandidateRef {
        deployment_id: "another".into(),
        staged_path: recorded.staged_path.clone(),
        bundle_digest: recorded.bundle_digest.clone(),
        generation: None,
        descriptor_digest: None,
        configuration_digest: None,
        backend_hint: recorded.backend_hint.clone(),
        model_class: recorded.model_class.clone(),
        bundle_name: None,
        bundle_version: None,
        artifact_relative_path: None,
    };
    agent.registry.prepare("tx", &other, HELD).expect("prepare");
    assert!(agent.registry.warm("tx", &other, HELD).expect("warm").ready);
    agent.registry.promote("tx", &other).expect("promote");

    let reported = active(&agent);
    assert_eq!(reported.deployment_id, MEMBER.0);
    assert_eq!(reported.generation, None);
    assert_eq!(reported.serving_url, Some(url(PORTS[0])));
}

#[test]
fn a_candidate_summary_names_the_generation_being_prepared_beside_the_one_serving() {
    let h = Harness::new();
    let agent = RegistryAgent::start(&h);
    let bundle = vision_bundle(h.td.path(), "stt");
    let deploy = || {
        agent.coord.deploy(
            "stt",
            &bundle,
            std::collections::BTreeMap::new(),
            None,
            None,
        )
    };
    deploy().expect("the first deploy");
    let (is_warming, release) = agent.workers.hold_probe();

    std::thread::scope(|scope| {
        // Dropped if this closure panics, which lets what is held end.
        let release = release;
        let deploying = scope.spawn(deploy);
        is_warming
            .recv_timeout(HELD)
            .expect("the candidate is probed");

        let during = status(&agent);
        let candidate = during.candidate.expect("a candidate");
        assert_eq!(candidate.deployment_id, "stt");
        assert_eq!(candidate.generation, Some(2));
        assert_eq!(during.active.expect("active").generation, Some(1));

        release.send(()).unwrap();
        deploying.join().unwrap().expect("the second deploy");
    });

    let after = status(&agent);
    assert_eq!(after.candidate, None);
    assert_eq!(after.active.expect("active").generation, Some(2));
}

#[test]
fn a_committed_member_whose_generation_runs_is_running_ready_and_in_contact() {
    let h = Harness::new();
    let agent = RegistryAgent::start(&h);
    agent.deploy_member(&h);
    agent.commit_member_as_a_set(&h);

    assert_eq!(
        process(&member(&agent)),
        (
            Some(ProcessState::Running),
            Some(Readiness::Ready),
            Some(false),
            Some(ContactState::InContact),
        )
    );
    // The set of one projects its committed generation into `active`.
    let projected = active(&agent);
    assert_eq!(projected.generation, Some(MEMBER.1));
    assert_eq!(projected.serving_url, Some(url(PORTS[1])));
    let previous = status(&agent).previous_active.expect("the retained one");
    assert_eq!(previous.generation, Some(PREDECESSOR.1));
}

#[test]
fn a_member_whose_polls_go_unanswered_is_running_and_out_of_contact() {
    let h = Harness::new();
    let agent = RegistryAgent::start(&h);
    agent.deploy_member(&h);
    agent.commit_member_as_a_set(&h);

    agent.workers.last().set_contact(ControlContact::Unanswered);

    assert_eq!(
        process(&member(&agent)),
        (
            Some(ProcessState::Running),
            Some(Readiness::Ready),
            Some(false),
            Some(ContactState::OutOfContact),
        )
    );
}

#[test]
fn a_member_whose_process_exited_is_stopped_and_stays_so_under_its_successor() {
    let h = Harness::new();
    let agent = RegistryAgent::start(&h);
    agent.deploy_member(&h);
    agent.commit_member_as_a_set(&h);

    agent.workers.last().exit();
    assert_eq!(process(&member(&agent)), STOPPED, "before it is noticed");

    agent.step(Duration::ZERO);
    assert_eq!(
        process(&member(&agent)),
        STOPPED,
        "once taken out of service"
    );

    // The successor is another generation than the committed one.
    agent.step(Duration::from_secs(1));
    agent.step(Duration::from_millis(1));
    let live = agent.registry.live_members().expect("the registry reports");
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].member.generation, MEMBER.1 + 1);
    assert!(live[0].running && live[0].ready);
    assert_eq!(process(&member(&agent)), STOPPED);
}

#[test]
fn a_member_asked_to_stop_is_draining_until_its_process_is_reaped() {
    let h = Harness::new();
    let agent = RegistryAgent::start(&h);
    agent.deploy_member(&h);
    agent.commit_member_as_a_set(&h);
    let worker = agent.workers.last();
    let (is_stopping, release) = worker.hold_stop();

    std::thread::scope(|scope| {
        // Dropped if this closure panics, which lets what is held end.
        let release = release;
        let unloading = scope.spawn(|| agent.registry.unload(MEMBER.0));
        is_stopping
            .recv_timeout(HELD)
            .expect("the member is asked to stop");

        assert_eq!(
            process(&member(&agent)),
            (
                Some(ProcessState::Running),
                Some(Readiness::Ready),
                Some(true),
                None,
            )
        );

        release.send(()).unwrap();
        unloading.join().unwrap();
    });

    assert!(!worker.running());
    assert_eq!(process(&member(&agent)), STOPPED);
}

#[test]
fn a_worker_control_that_reports_no_processes_leaves_the_fields_absent() {
    let h = Harness::new();
    let agent = RegistryAgent::start(&h);
    agent.deploy_member(&h);
    agent.commit_member_as_a_set(&h);
    assert_ne!(process(&member(&agent)), (None, None, None, None));

    // The same state, through the harness's coordinator over the mock
    // worker control.
    let reported = h.coord.status().expect("status");
    let set = reported.resident_set.expect("a resident set");
    assert_eq!(process(&set.members[0]), (None, None, None, None));
    assert_eq!(
        reported.active.expect("the projected member").generation,
        Some(MEMBER.1),
        "the committed generation is the record's, whatever runs"
    );
}
