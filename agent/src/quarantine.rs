// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F07 helpers: quarantine summaries shared with the control API.
//
// The coordinator's `fail` path already calls
// [`crate::state::StateStore::quarantine_in_flight`]. This module exposes
// the read-only projection used by the recovery planner and the status
// endpoint.

use tensorplate_protocol::agent_state::QuarantineRecord;

use crate::error::AgentResult;
use crate::state::StateStore;

/// Return the bounded list of quarantined candidates, most-recent first.
///
/// # Errors
///
/// Propagates [`crate::error::AgentError::Internal`] when the store's
/// mutex is poisoned.
pub fn list(store: &StateStore) -> AgentResult<Vec<QuarantineRecord>> {
    let mut s = store.snapshot()?;
    s.quarantined.reverse();
    Ok(s.quarantined)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_used,
        clippy::default_trait_access
    )]
    use super::list;
    use crate::state::StateStore;
    use tempfile::TempDir;
    use tensorplate_protocol::agent_state::{
        DeploymentRecord, ErrorRecord, TransactionKind, TransactionRecord,
    };
    use tensorplate_protocol::deploy_transaction::DeployState;
    use tensorplate_protocol::ErrorCode;

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

    fn record(id: &str) -> DeploymentRecord {
        DeploymentRecord {
            deployment_id: id.into(),
            bundle_digest: "sha256:cafe".into(),
            bundle_name: "n".into(),
            bundle_version: "1".into(),
            backend_hint: "mock".into(),
            model_class: "vision".into(),
            staged_path: format!("/staging/{id}"),
            promoted_monotonic_ns: None,
            labels: Default::default(),
        }
    }

    #[test]
    fn most_recent_first() {
        let td = TempDir::new().expect("td");
        let store = StateStore::open(td.path()).expect("open");
        for (i, dep) in ["d1", "d2", "d3"].iter().enumerate() {
            store
                .begin_transaction(tx(&format!("tx-{i}"), dep))
                .expect("begin");
            store.record_candidate(record(dep)).expect("cand");
            store
                .quarantine_in_flight(
                    ErrorRecord::new(ErrorCode::Internal, format!("fail-{i}")),
                    i as u64,
                )
                .expect("quarantine");
        }
        let entries = list(&store).expect("list");
        assert_eq!(entries[0].deployment_id, "d3");
        assert_eq!(entries[2].deployment_id, "d1");
    }
}
