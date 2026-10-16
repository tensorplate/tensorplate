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

use common::{vision_bundle, write_bundle, BundleSpec, Harness};
use registry_agent::{RegistryAgent, HELD, MEMBER, PORTS, PREDECESSOR};
use tensorplate_agent::registry::ControlContact;
use tensorplate_agent::worker::{MemberRole, WorkerControl};
use tensorplate_protocol::agent_control::{
    AgentStatus, ContactState, DeploymentSummary, MemberStatus, ProcessState, Readiness,
};
use tensorplate_protocol::agent_state::DeploymentRecord;
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

/// A candidate started from what `recorded` was staged from, under `id`.
fn staged_as(id: &str, recorded: &DeploymentRecord) -> CandidateRef {
    CandidateRef {
        deployment_id: id.into(),
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
        streaming: false,
    }
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

const DRAINING: (
    Option<ProcessState>,
    Option<Readiness>,
    Option<bool>,
    Option<ContactState>,
) = (
    Some(ProcessState::Running),
    Some(Readiness::Ready),
    Some(true),
    None,
);

#[test]
fn active_names_the_generation_serving_at_its_url_and_each_deploy_moves_both() {
    let h = Harness::new();
    let agent = RegistryAgent::start(&h);
    assert_eq!(status(&agent).active, None);

    agent
        .deploy(PREDECESSOR.0, &vision_bundle(h.td.path(), PREDECESSOR.0))
        .expect("the first deploy");
    let first = active(&agent);
    assert_eq!(first.deployment_id, PREDECESSOR.0);
    assert_eq!(first.generation, Some(PREDECESSOR.1));
    assert_eq!(first.serving_url, Some(url(PORTS[0])));

    agent
        .deploy(MEMBER.0, &vision_bundle(h.td.path(), MEMBER.0))
        .expect("the second deploy");

    let serving = active(&agent);
    assert_eq!(serving.deployment_id, MEMBER.0);
    assert_eq!(serving.generation, Some(MEMBER.1));
    assert_eq!(serving.serving_url, Some(url(PORTS[1])));
    let previous = status(&agent).previous_active.expect("the first deploy");
    assert_eq!(previous.deployment_id, PREDECESSOR.0);
    assert_eq!(
        previous.generation, None,
        "nothing runs for a retained deployment, and status reads no generation from a record"
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
    agent.reap_retired();
    let recorded = h.store.snapshot().expect("state").active.expect("active");

    // Another deployment serves than the recorded one, as the outgoing
    // member does between a transaction's commit and its activation.
    let other = staged_as("another", &recorded);
    agent.registry.prepare("tx", &other, HELD).expect("prepare");
    assert!(agent.registry.warm("tx", &other, HELD).expect("warm").ready);
    agent.registry.promote("tx", &other).expect("promote");

    let reported = active(&agent);
    assert_eq!(reported.deployment_id, MEMBER.0);
    assert_eq!(reported.generation, None);
    assert_eq!(reported.serving_url, Some(url(PORTS[0])));
}

#[test]
fn a_same_id_update_is_recorded_before_the_replaced_worker_is_told_to_retire() {
    let h = Harness::new();
    let agent = RegistryAgent::start(&h);
    agent
        .deploy("stt", &vision_bundle(h.td.path(), "stt"))
        .expect("the first deploy");
    let first = active(&agent);
    assert_eq!(first.generation, Some(1));
    let replacement = write_bundle(
        h.td.path(),
        "stt",
        BundleSpec {
            memory_estimate_bytes: Some(1),
            ..BundleSpec::default()
        },
    );
    let (is_retiring, release) = agent.workers.last().hold_retire();

    std::thread::scope(|scope| {
        // Dropped if this closure panics, which lets what is held end.
        let release = release;
        let deploying = scope.spawn(|| agent.deploy("stt", &replacement));
        is_retiring
            .recv_timeout(HELD)
            .expect("the replaced worker is told to retire");

        // The commit is written before the replaced worker hears of it.
        let during = active(&agent);
        assert_ne!(during.bundle_digest, first.bundle_digest);
        assert_eq!(during.generation, Some(2));
        assert_eq!(during.serving_url, Some(url(PORTS[1])));

        release.send(()).unwrap();
        deploying.join().unwrap().expect("the second deploy");
    });

    let after = active(&agent);
    assert_ne!(after.bundle_digest, first.bundle_digest);
    assert_eq!(after.generation, Some(2));
    assert_eq!(after.serving_url, Some(url(PORTS[1])));
}

#[test]
fn a_redeploy_of_the_recorded_bundle_under_its_id_reads_as_the_recorded_deployment() {
    let h = Harness::new();
    let agent = RegistryAgent::start(&h);
    let bundle = vision_bundle(h.td.path(), "stt");
    agent.deploy("stt", &bundle).expect("the first deploy");
    let first = active(&agent);
    assert_eq!(first.generation, Some(1));
    let (is_retiring, release) = agent.workers.last().hold_retire();

    std::thread::scope(|scope| {
        // Dropped if this closure panics, which lets what is held end.
        let release = release;
        let deploying = scope.spawn(|| agent.deploy("stt", &bundle));
        is_retiring
            .recv_timeout(HELD)
            .expect("the replaced worker is told to retire");

        // The record is the second deploy's, written before the replaced
        // worker hears of it.
        let during = active(&agent);
        assert_ne!(during.promoted_monotonic_ns, first.promoted_monotonic_ns);
        assert_eq!(during.generation, Some(2));
        assert_eq!(during.serving_url, Some(url(PORTS[1])));

        release.send(()).unwrap();
        deploying.join().unwrap().expect("the second deploy");
    });

    assert_eq!(active(&agent).generation, Some(2));
}

#[test]
fn a_candidate_summary_names_the_generation_being_prepared_beside_the_one_serving() {
    let h = Harness::new();
    let agent = RegistryAgent::start(&h);
    let bundle = vision_bundle(h.td.path(), "stt");
    let deploy = || agent.deploy("stt", &bundle);
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
fn a_candidate_summary_does_not_name_a_generation_an_earlier_deploy_abandoned() {
    let h = Harness::new();
    let agent = RegistryAgent::start(&h);
    agent
        .deploy("stt", &vision_bundle(h.td.path(), "stt"))
        .expect("deploy");
    let recorded = h.store.snapshot().expect("state").active.expect("active");
    // A deploy records its candidate before it prepares a worker for it,
    // and the worker of a deploy that failed runs until then.
    let retried = staged_as("stt", &recorded);
    agent
        .registry
        .prepare("tx", &retried, HELD)
        .expect("prepare");
    h.store.record_candidate(recorded).expect("record");
    let candidate = |agent: &RegistryAgent| status(agent).candidate.expect("a candidate");
    assert_eq!(candidate(&agent).generation, Some(2));

    agent.registry.abandon(&retried);

    assert_eq!(candidate(&agent).generation, None);
    let live = agent.registry.live_members().expect("the registry reports");
    assert_eq!(
        (live[1].member.generation, live[1].role, live[1].running),
        (2, MemberRole::Abandoned, true)
    );
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
fn a_set_member_whose_process_cannot_be_inspected_reads_running_and_status_answers() {
    let h = Harness::new();
    let agent = RegistryAgent::start(&h);
    agent.deploy_member(&h);
    agent.workers.last().cannot_be_inspected();
    assert!(agent.registry.serving_member().is_err());
    // Beside the singleton record the serving lookup's error is the status's.
    assert!(agent.coord.status().is_err());

    agent.commit_member_as_a_set(&h);

    // Without an active record nothing is asked of the serving member.
    assert!(agent.registry.serving_member().is_err());
    assert_eq!(
        process(&member(&agent)),
        (
            Some(ProcessState::Running),
            Some(Readiness::Ready),
            Some(false),
            Some(ContactState::InContact),
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
fn a_member_told_to_retire_is_draining_until_its_process_is_reaped() {
    let h = Harness::new();
    let agent = RegistryAgent::start(&h);
    agent.deploy_member(&h);
    agent.commit_member_as_a_set(&h);
    let worker = agent.workers.last();
    let (is_retiring, release) = worker.hold_retire();

    std::thread::scope(|scope| {
        // Dropped if this closure panics, which lets what is held end.
        let release = release;
        let unloading = scope.spawn(|| agent.registry.unload(MEMBER.0));
        is_retiring
            .recv_timeout(HELD)
            .expect("the member is told to retire");

        assert_eq!(process(&member(&agent)), DRAINING);

        release.send(()).unwrap();
        unloading.join().unwrap();
    });

    assert!(!worker.running());
    assert_eq!(process(&member(&agent)), DRAINING, "until it is reaped");
    agent.reap_retired();
    assert_eq!(process(&member(&agent)), STOPPED);
}

#[test]
fn a_committed_member_promoted_over_is_draining_then_stopped_and_its_successor_is_not_listed() {
    let h = Harness::new();
    let agent = RegistryAgent::start(&h);
    agent.deploy_member(&h);
    agent.reap_retired();
    let recorded = h.store.snapshot().expect("state").active.expect("active");
    agent.commit_member_as_a_set(&h);
    let committed = agent.workers.last();
    let (is_retiring, release) = committed.hold_retire();
    let successor = staged_as(MEMBER.0, &recorded);
    agent
        .registry
        .prepare("tx", &successor, HELD)
        .expect("prepare");
    assert!(
        agent
            .registry
            .warm("tx", &successor, HELD)
            .expect("warm")
            .ready
    );

    let projected = |agent: &RegistryAgent| {
        let active = active(agent);
        (active.generation, active.serving_url)
    };
    let committed_slot = (Some(MEMBER.1), Some(url(committed.port)));
    std::thread::scope(|scope| {
        // Dropped if this closure panics, which lets what is held end.
        let release = release;
        let promoting = scope.spawn(|| agent.registry.promote("tx", &successor));
        is_retiring
            .recv_timeout(HELD)
            .expect("the committed generation is told to retire");

        assert_eq!(process(&member(&agent)), DRAINING);
        assert_eq!(projected(&agent), committed_slot);

        release.send(()).unwrap();
        promoting.join().unwrap().expect("promote");
    });

    // The successor serves; no record names it, so status does not.
    let serving = agent.registry.serving_member().expect("read").expect("one");
    assert_eq!(serving.member.generation, MEMBER.1 + 1);
    assert_ne!(Some(serving.serving_url), committed_slot.1);
    assert_eq!(process(&member(&agent)), DRAINING, "until it is reaped");
    agent.reap_retired();
    assert_eq!(process(&member(&agent)), STOPPED);
    assert_eq!(projected(&agent), committed_slot);
    assert_eq!(status(&agent).candidate, None);
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
