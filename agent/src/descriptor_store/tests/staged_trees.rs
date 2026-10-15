// SPDX-License-Identifier: Apache-2.0

use super::deploy::{started_after_an_update_that_did_not_complete, FORMAT_0_1, STT, TTS};
use super::one_pass::another_format_0_1_bundle;
use super::start_again::{host, Worker};
use super::*;

/// The digest of the bundle staged where `reference` says.
fn staged_digest(reference: &CandidateRef) -> String {
    let staged = crate::bundle::staged_manifest(Path::new(&reference.staged_path));
    staged.unwrap().bundle_digest
}

fn digest_of(bundle: &str) -> String {
    parse_bundle(&repo(bundle)).unwrap().manifest_digest
}

#[test]
fn a_rollback_after_a_deploy_of_another_bundle_under_the_same_id_starts_the_earlier_bundle() {
    let host = host();
    let (agent, _) = host.start(Worker::default());
    agent.deploy("stt", STT).unwrap();
    drop(agent.asked());
    agent.deploy("stt", TTS).unwrap();
    drop(agent.asked());

    agent.coordinator.rollback(None).unwrap();

    let asked = agent.asked();
    assert_eq!(staged_digest(&asked.reference), digest_of(STT));
    let resolved = asked.resolved.unwrap();
    assert_eq!(resolved.configuration.bundle.bundle_digest, digest_of(STT));
    assert_eq!(host.view(), ["stt@3", "stt@2", "-", "4"]);
    assert_eq!(host.last_error(), None);
}

#[test]
fn a_rollback_to_a_format_0_1_record_after_an_update_under_its_id_starts_the_earlier_bundle() {
    let host = host();
    let (agent, _) = host.start(Worker::default());
    agent.deploy("smoke", FORMAT_0_1).unwrap();
    drop(agent.asked());
    let update = another_format_0_1_bundle(&host);
    let coordinator = &agent.coordinator;
    let updated = coordinator.deploy("smoke", &update, BTreeMap::new(), None, None);
    updated.unwrap();
    drop(agent.asked());

    agent.coordinator.rollback(None).unwrap();

    let reference = agent.asked().reference;
    assert_eq!(reference.bundle_digest, digest_of(FORMAT_0_1));
    assert_eq!(staged_digest(&reference), digest_of(FORMAT_0_1));
}

#[test]
fn a_start_after_a_same_id_update_that_did_not_complete_restores_the_record_s_own_bundle() {
    let host = host();

    let next = started_after_an_update_that_did_not_complete(&host);

    let asked = next.asked();
    assert_eq!(staged_digest(&asked.reference), digest_of(STT));
    assert_eq!(host.view(), ["stt@2", "-", "-", "3"]);
    assert_eq!(host.last_error(), None);
}
