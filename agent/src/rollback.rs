// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F06 helpers: previous-active eligibility and revalidation.
//
// The rollback transaction itself lives in [`crate::coordinator`]; this
// module exposes the small "is rollback eligible right now?" helpers
// shared between the coordinator and the control-API handlers.

use std::path::Path;

use tensorplate_protocol::agent_state::DeploymentRecord;

use crate::error::AgentResult;
use crate::state::StateStore;

/// Outcome of an eligibility check.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Eligibility {
    Eligible(DeploymentRecord),
    NoPreviousActive,
    MissingStagedPath(String),
    MissingManifest(String),
}

/// Inspect the current desired state and decide whether a rollback can
/// proceed. Does not mutate any state.
///
/// # Errors
///
/// Returns [`AgentError::Internal`] if the store's mutex is poisoned.
pub fn check(store: &StateStore) -> AgentResult<Eligibility> {
    let s = store.snapshot()?;
    let Some(prev) = s.previous_active.clone() else {
        return Ok(Eligibility::NoPreviousActive);
    };
    let staged = Path::new(&prev.staged_path);
    if !staged.is_dir() {
        return Ok(Eligibility::MissingStagedPath(prev.staged_path));
    }
    let manifest = staged.join("manifest.json");
    if !manifest.is_file() {
        return Ok(Eligibility::MissingManifest(manifest.display().to_string()));
    }
    Ok(Eligibility::Eligible(prev))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_used,
        clippy::default_trait_access
    )]
    use super::{check, Eligibility};
    use crate::state::StateStore;
    use std::fs;
    use tempfile::TempDir;
    use tensorplate_protocol::agent_state::{DeploymentRecord, TransactionKind, TransactionRecord};
    use tensorplate_protocol::deploy_transaction::DeployState;

    fn record(id: &str, staged_path: String) -> DeploymentRecord {
        DeploymentRecord {
            deployment_id: id.into(),
            bundle_digest: "sha256:cafe".into(),
            bundle_name: "n".into(),
            bundle_version: "1".into(),
            backend_hint: "mock".into(),
            model_class: "vision".into(),
            staged_path,
            promoted_monotonic_ns: Some(1),
            labels: Default::default(),
        }
    }

    fn tx(id: &str, deployment_id: &str) -> TransactionRecord {
        TransactionRecord {
            transaction_id: id.into(),
            deployment_id: deployment_id.into(),
            phase: DeployState::Received,
            kind: TransactionKind::Deploy,
            bundle_digest: None,
            bundle_path: None,
            correlation_id: None,
            started_monotonic_ns: Some(1),
            last_transition_monotonic_ns: Some(1),
            failure: None,
        }
    }

    #[test]
    fn no_previous_active_is_reported() {
        let td = TempDir::new().expect("td");
        let store = StateStore::open(td.path()).expect("open");
        assert_eq!(check(&store).expect("check"), Eligibility::NoPreviousActive);
    }

    #[test]
    fn missing_staged_path_is_reported() {
        let td = TempDir::new().expect("td");
        let store = StateStore::open(td.path()).expect("open");
        store.begin_transaction(tx("tx", "d")).expect("begin");
        store
            .record_candidate(record("d", td.path().join("missing").display().to_string()))
            .expect("cand");
        store.promote_candidate(10).expect("promote");
        store.clear_transaction().expect("clear");
        // Set up a previous_active by performing a second promote.
        store.begin_transaction(tx("tx2", "d2")).expect("begin");
        store
            .record_candidate(record(
                "d2",
                td.path().join("missing2").display().to_string(),
            ))
            .expect("cand");
        store.promote_candidate(20).expect("promote");

        // Previous active is `d`, whose staged_path doesn't exist.
        assert!(matches!(
            check(&store).expect("check"),
            Eligibility::MissingStagedPath(_)
        ));
    }

    #[test]
    fn missing_manifest_is_reported() {
        let td = TempDir::new().expect("td");
        let store = StateStore::open(td.path()).expect("open");
        let staged_dir = td.path().join("staged-d");
        fs::create_dir_all(&staged_dir).expect("mkdir");
        store.begin_transaction(tx("tx", "d")).expect("begin");
        store
            .record_candidate(record("d", staged_dir.display().to_string()))
            .expect("cand");
        store.promote_candidate(10).expect("promote");
        store.clear_transaction().expect("clear");
        store.begin_transaction(tx("tx2", "d2")).expect("begin");
        store
            .record_candidate(record(
                "d2",
                td.path().join("staged-d2").display().to_string(),
            ))
            .expect("cand");
        // Need d2 staged path to exist for the second promote.
        fs::create_dir_all(td.path().join("staged-d2")).expect("mkdir");
        store.promote_candidate(20).expect("promote");

        // Previous active staged_path exists but lacks manifest.json.
        assert!(matches!(
            check(&store).expect("check"),
            Eligibility::MissingManifest(_)
        ));
    }

    #[test]
    fn eligible_when_files_present() {
        let td = TempDir::new().expect("td");
        let store = StateStore::open(td.path()).expect("open");
        let staged_dir = td.path().join("staged-d");
        fs::create_dir_all(&staged_dir).expect("mkdir");
        fs::write(staged_dir.join("manifest.json"), b"{}").expect("manifest");
        store.begin_transaction(tx("tx", "d")).expect("begin");
        store
            .record_candidate(record("d", staged_dir.display().to_string()))
            .expect("cand");
        store.promote_candidate(10).expect("promote");
        store.clear_transaction().expect("clear");

        let staged_dir2 = td.path().join("staged-d2");
        fs::create_dir_all(&staged_dir2).expect("mkdir");
        fs::write(staged_dir2.join("manifest.json"), b"{}").expect("manifest");
        store.begin_transaction(tx("tx2", "d2")).expect("begin");
        store
            .record_candidate(record("d2", staged_dir2.display().to_string()))
            .expect("cand");
        store.promote_candidate(20).expect("promote");

        match check(&store).expect("check") {
            Eligibility::Eligible(rec) => assert_eq!(rec.deployment_id, "d"),
            other => panic!("expected eligible, got {other:?}"),
        }
    }
}
