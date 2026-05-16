// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F02: Durable desired-state store and transaction journal.
//
// The agent persists exactly one file (`state.json`) plus a same-directory
// backup (`state.json.bak`). Every mutation:
//
//   1. Bumps `store_version` and clones the previous state as backup.
//   2. Writes the new state to a sibling temp file in the same directory.
//   3. Calls `fsync` on the temp file, then `rename(2)` over `state.json`,
//      then `fsync` on the parent directory.
//
// `rename(2)` is atomic on POSIX filesystems, so a crash mid-write either
// leaves the previous `state.json` untouched (rename never started) or
// leaves the new `state.json` in place (rename committed). The backup file
// catches the rare corruption-after-rename case (e.g. disk-level bit rot)
// and is consulted only when the primary file fails to decode.
//
// State updates are serialized through an in-process `Mutex`; the agent
// has exactly one active control thread mutating state, but the mutex
// guards against future concurrent mutators and against test harnesses
// that share a store across threads.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tensorplate_protocol::agent_state::{
    AgentState, DeploymentRecord, ErrorRecord, QuarantineRecord, TransactionRecord,
};
use tensorplate_protocol::deploy_transaction::DeployState;
use tensorplate_protocol::{decode_with_version_check, SCHEMA_VERSION};

use crate::error::{AgentError, AgentResult};

/// Update closure: mutate the in-flight clone, return Ok to commit.
pub type StateUpdate<'a> = Box<dyn FnOnce(&mut AgentState) -> AgentResult<()> + Send + Sync + 'a>;

/// Durable desired-state store. Owns the primary state file and a sibling
/// backup; serializes mutations through a Mutex.
#[derive(Debug)]
pub struct StateStore {
    state_dir: PathBuf,
    primary: PathBuf,
    backup: PathBuf,
    inner: Mutex<AgentState>,
}

impl StateStore {
    /// Open the store rooted at `state_dir`. The directory is created if
    /// missing. If the primary file is absent or unparseable, the backup
    /// is consulted; if both fail, the store starts from a fresh
    /// [`AgentState::fresh`].
    ///
    /// Unparseable-and-no-backup is an explicit corruption error so the
    /// agent does not silently overwrite operator data.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Io`] for filesystem failures and
    /// [`AgentError::CorruptState`] when both files are present but neither
    /// decodes.
    pub fn open(state_dir: impl Into<PathBuf>) -> AgentResult<Self> {
        let state_dir = state_dir.into();
        fs::create_dir_all(&state_dir)?;
        let primary = state_dir.join("state.json");
        let backup = state_dir.join("state.json.bak");
        let inner = match load_one(&primary) {
            Ok(Some(s)) => s,
            Ok(None) => match load_one(&backup)? {
                Some(s) => s,
                None => AgentState::fresh(),
            },
            Err(primary_err) => match load_one(&backup) {
                Ok(Some(s)) => s,
                _ => return Err(primary_err),
            },
        };
        Ok(Self {
            state_dir,
            primary,
            backup,
            inner: Mutex::new(inner),
        })
    }

    /// Path of the durable directory the store is rooted at.
    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Clone of the currently-loaded state. Cheap; intended for status
    /// projection.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Internal`] if the internal mutex is poisoned.
    pub fn snapshot(&self) -> AgentResult<AgentState> {
        let guard = self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("state mutex poisoned: {e}")))?;
        Ok(guard.clone())
    }

    /// Apply `update` under the store mutex. The closure mutates an
    /// in-memory clone of the current state; on success the clone is
    /// persisted atomically and replaces the in-memory copy. On failure
    /// the in-memory state is untouched.
    ///
    /// # Errors
    ///
    /// Propagates the closure's error, plus [`AgentError::Io`] /
    /// [`AgentError::Serialization`] from the write path.
    pub fn update<F, T>(&self, update: F) -> AgentResult<T>
    where
        F: FnOnce(&mut AgentState) -> AgentResult<T>,
    {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("state mutex poisoned: {e}")))?;
        let mut next = guard.clone();
        let outcome = update(&mut next)?;
        next.store_version = next.store_version.saturating_add(1);
        next.schema_version = SCHEMA_VERSION.to_string();
        atomic_write(&self.primary, &self.backup, &self.state_dir, &next)?;
        *guard = next;
        Ok(outcome)
    }

    /// Convenience: record an in-flight transaction phase update.
    ///
    /// # Errors
    ///
    /// Propagates errors from [`Self::update`].
    pub fn record_phase(
        &self,
        transaction_id: &str,
        phase: DeployState,
        monotonic_ns: u64,
    ) -> AgentResult<()> {
        self.update(|s| {
            let Some(tx) = s.in_flight_transaction.as_mut() else {
                return Err(AgentError::Internal(format!(
                    "record_phase {transaction_id}: no in-flight transaction"
                )));
            };
            if tx.transaction_id != transaction_id {
                return Err(AgentError::Internal(format!(
                    "record_phase {transaction_id}: id mismatch ({} in flight)",
                    tx.transaction_id
                )));
            }
            tx.phase = phase;
            tx.last_transition_monotonic_ns = Some(monotonic_ns);
            Ok(())
        })
    }

    /// Convenience: set the in-flight transaction record (start of a new
    /// transaction).
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Busy`] if another mutating transaction is
    /// already in flight.
    pub fn begin_transaction(&self, record: TransactionRecord) -> AgentResult<()> {
        self.update(|s| {
            if let Some(existing) = s.in_flight_transaction.as_ref() {
                if !existing.phase.is_terminal() {
                    return Err(AgentError::Busy(existing.transaction_id.clone()));
                }
            }
            s.in_flight_transaction = Some(record);
            Ok(())
        })
    }

    /// Convenience: clear the in-flight transaction (call after a
    /// successful active deployment is fully recorded, or after a
    /// terminal failure has been quarantined).
    ///
    /// # Errors
    ///
    /// Propagates errors from [`Self::update`].
    pub fn clear_transaction(&self) -> AgentResult<()> {
        self.update(|s| {
            s.in_flight_transaction = None;
            Ok(())
        })
    }

    /// Persist a candidate record. Replaces any existing candidate.
    ///
    /// # Errors
    ///
    /// Propagates errors from [`Self::update`].
    pub fn record_candidate(&self, candidate: DeploymentRecord) -> AgentResult<()> {
        self.update(|s| {
            s.candidate = Some(candidate);
            Ok(())
        })
    }

    /// Promote candidate to active. Atomic with previous-active rotation:
    /// active -> previous_active, candidate -> active.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Internal`] if no candidate is present.
    pub fn promote_candidate(&self, monotonic_ns: u64) -> AgentResult<()> {
        self.update(|s| {
            let mut cand = s.candidate.take().ok_or_else(|| {
                AgentError::Internal("promote_candidate called with no candidate".into())
            })?;
            cand.promoted_monotonic_ns = Some(monotonic_ns);
            let prev_active = s.active.take();
            s.previous_active = prev_active;
            s.active = Some(cand);
            Ok(())
        })
    }

    /// Move the active deployment back into the candidate slot and
    /// promote previous_active back to active (used by rollback).
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Unavailable`] when no previous_active exists.
    pub fn swap_active_with_previous(&self, monotonic_ns: u64) -> AgentResult<()> {
        self.update(|s| {
            let mut prev = s
                .previous_active
                .take()
                .ok_or_else(|| AgentError::Unavailable("no previous active deployment".into()))?;
            prev.promoted_monotonic_ns = Some(monotonic_ns);
            let demoted = s.active.take();
            s.active = Some(prev);
            s.previous_active = demoted;
            Ok(())
        })
    }

    /// Quarantine the in-flight candidate. The candidate slot is cleared;
    /// the in-flight transaction is moved into `quarantined` and the
    /// in-flight slot is cleared. Active deployment is preserved.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Internal`] when no in-flight transaction is
    /// recorded.
    pub fn quarantine_in_flight(&self, error: ErrorRecord, monotonic_ns: u64) -> AgentResult<()> {
        const MAX_QUARANTINE: usize = 32;
        self.update(|s| {
            let Some(tx) = s.in_flight_transaction.take() else {
                return Err(AgentError::Internal(
                    "quarantine_in_flight called with no in-flight transaction".into(),
                ));
            };
            let record = QuarantineRecord {
                transaction_id: tx.transaction_id,
                deployment_id: tx.deployment_id,
                bundle_digest: tx.bundle_digest,
                phase: tx.phase,
                error: error.clone(),
                quarantined_monotonic_ns: Some(monotonic_ns),
            };
            s.candidate = None;
            s.last_error = Some(error);
            s.quarantined.push(record);
            // Bound the persisted list so a pathological loop cannot
            // grow the state file without bound.
            if s.quarantined.len() > MAX_QUARANTINE {
                let drop_count = s.quarantined.len() - MAX_QUARANTINE;
                s.quarantined.drain(0..drop_count);
            }
            Ok(())
        })
    }

    /// Replace the `last_error` slot. Used by the recovery planner and
    /// the worker client to surface the most-recent typed error without
    /// quarantining a candidate.
    ///
    /// # Errors
    ///
    /// Propagates errors from [`Self::update`].
    pub fn set_last_error(&self, error: Option<ErrorRecord>) -> AgentResult<()> {
        self.update(|s| {
            s.last_error = error;
            Ok(())
        })
    }

    /// Copy the persisted labels map (useful when a candidate inherits
    /// the deploy-request labels).
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Internal`] if the internal mutex is poisoned.
    pub fn labels(&self) -> AgentResult<BTreeMap<String, String>> {
        let g = self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("state mutex poisoned: {e}")))?;
        Ok(g.active
            .as_ref()
            .map(|d| d.labels.clone())
            .unwrap_or_default())
    }
}

fn load_one(path: &Path) -> AgentResult<Option<AgentState>> {
    let raw = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    match decode_with_version_check::<AgentState>(&raw) {
        Ok(s) => Ok(Some(s)),
        Err(err) => Err(AgentError::CorruptState(format!(
            "decode {}: {err}",
            path.display()
        ))),
    }
}

fn atomic_write(
    primary: &Path,
    backup: &Path,
    state_dir: &Path,
    state: &AgentState,
) -> AgentResult<()> {
    let encoded = serde_json::to_vec_pretty(state)?;
    let tmp = primary.with_extension("json.tmp");
    {
        let mut f: File = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(&encoded)?;
        f.sync_all()?;
    }
    // Atomically commit the new primary.
    fs::rename(&tmp, primary)?;
    // Best-effort directory fsync so the rename survives power loss
    // where the OS supports it.
    if let Ok(d) = File::open(state_dir) {
        let _ = d.sync_all();
    }
    // Refresh the backup to mirror the newly-committed primary. The
    // backup is the last-known-good snapshot used when the primary
    // becomes unreadable; refreshing it *after* the primary commit
    // ensures it always reflects a successfully written state. A crash
    // between the primary commit and the backup refresh leaves the
    // backup one generation stale, which is still safe for recovery
    // (the agent prefers primary when it parses).
    let backup_tmp = backup.with_extension("bak.tmp");
    {
        let mut f: File = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&backup_tmp)?;
        f.write_all(&encoded)?;
        f.sync_all()?;
    }
    fs::rename(&backup_tmp, backup)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_used,
        clippy::default_trait_access
    )]
    use super::StateStore;
    use std::fs;
    use tempfile::TempDir;
    use tensorplate_protocol::agent_state::{
        DeploymentRecord, ErrorRecord, TransactionKind, TransactionRecord,
    };
    use tensorplate_protocol::deploy_transaction::DeployState;
    use tensorplate_protocol::ErrorCode;

    fn sample_record(id: &str) -> DeploymentRecord {
        DeploymentRecord {
            deployment_id: id.into(),
            bundle_digest: "sha256:cafe".into(),
            bundle_name: "yolov8n".into(),
            bundle_version: "1.0.0".into(),
            backend_hint: "mock".into(),
            model_class: "vision".into(),
            staged_path: format!("/tmp/{id}"),
            promoted_monotonic_ns: None,
            labels: Default::default(),
        }
    }

    fn sample_tx(id: &str, deployment_id: &str) -> TransactionRecord {
        TransactionRecord {
            transaction_id: id.into(),
            deployment_id: deployment_id.into(),
            phase: DeployState::Received,
            kind: TransactionKind::Deploy,
            bundle_digest: Some("sha256:cafe".into()),
            bundle_path: Some("/bundles/x".into()),
            correlation_id: None,
            started_monotonic_ns: Some(1),
            last_transition_monotonic_ns: Some(1),
            failure: None,
        }
    }

    #[test]
    fn fresh_store_starts_empty() {
        let td = TempDir::new().expect("td");
        let store = StateStore::open(td.path()).expect("open");
        let s = store.snapshot().expect("snapshot");
        assert_eq!(s.store_version, 1);
        assert!(s.active.is_none());
    }

    #[test]
    fn promote_and_swap_round_trip() {
        let td = TempDir::new().expect("td");
        let store = StateStore::open(td.path()).expect("open");
        store
            .begin_transaction(sample_tx("tx-1", "d1"))
            .expect("begin");
        store.record_candidate(sample_record("d1")).expect("cand");
        store.promote_candidate(100).expect("promote");
        let s = store.snapshot().expect("snap");
        assert_eq!(s.active.as_ref().expect("active").deployment_id, "d1");
        assert!(s.previous_active.is_none());

        // Second deploy
        store.clear_transaction().expect("clear");
        store
            .begin_transaction(sample_tx("tx-2", "d2"))
            .expect("begin");
        store.record_candidate(sample_record("d2")).expect("cand");
        store.promote_candidate(200).expect("promote");
        let s = store.snapshot().expect("snap");
        assert_eq!(s.active.as_ref().expect("a").deployment_id, "d2");
        assert_eq!(
            s.previous_active.as_ref().expect("prev").deployment_id,
            "d1"
        );

        // Rollback swaps active <-> previous.
        store.swap_active_with_previous(300).expect("swap");
        let s = store.snapshot().expect("snap");
        assert_eq!(s.active.as_ref().expect("a").deployment_id, "d1");
        assert_eq!(s.previous_active.as_ref().expect("p").deployment_id, "d2");
    }

    #[test]
    fn busy_returns_typed_error_for_concurrent_transactions() {
        let td = TempDir::new().expect("td");
        let store = StateStore::open(td.path()).expect("open");
        store
            .begin_transaction(sample_tx("tx-1", "d1"))
            .expect("begin");
        let err = store
            .begin_transaction(sample_tx("tx-2", "d2"))
            .expect_err("busy");
        assert!(matches!(err, super::AgentError::Busy(_)));
    }

    #[test]
    fn quarantine_preserves_active_and_clears_candidate() {
        let td = TempDir::new().expect("td");
        let store = StateStore::open(td.path()).expect("open");

        // Promote a first deployment.
        store
            .begin_transaction(sample_tx("tx-1", "d1"))
            .expect("begin");
        store.record_candidate(sample_record("d1")).expect("cand");
        store.promote_candidate(10).expect("promote");
        store.clear_transaction().expect("clear");

        // Try a second deploy and fail it.
        store
            .begin_transaction(sample_tx("tx-2", "d-bad"))
            .expect("begin");
        store
            .record_candidate(sample_record("d-bad"))
            .expect("cand");
        store
            .quarantine_in_flight(ErrorRecord::new(ErrorCode::OomError, "too big"), 20)
            .expect("quarantine");

        let s = store.snapshot().expect("snap");
        assert!(s.candidate.is_none());
        assert!(s.in_flight_transaction.is_none());
        assert_eq!(s.active.as_ref().expect("active").deployment_id, "d1");
        assert_eq!(s.quarantined.len(), 1);
        assert_eq!(s.quarantined[0].deployment_id, "d-bad");
    }

    #[test]
    fn reopen_recovers_persisted_state() {
        let td = TempDir::new().expect("td");
        {
            let store = StateStore::open(td.path()).expect("open");
            store
                .begin_transaction(sample_tx("tx-1", "d1"))
                .expect("begin");
            store.record_candidate(sample_record("d1")).expect("cand");
            store.promote_candidate(10).expect("promote");
        }
        let store = StateStore::open(td.path()).expect("reopen");
        let s = store.snapshot().expect("snap");
        assert_eq!(s.active.as_ref().expect("active").deployment_id, "d1");
        // store_version grew across writes; specific count isn't part of
        // the contract but it must be > 1.
        assert!(s.store_version > 1);
    }

    #[test]
    fn corrupt_primary_falls_back_to_backup() {
        let td = TempDir::new().expect("td");
        let store = StateStore::open(td.path()).expect("open");
        store
            .begin_transaction(sample_tx("tx-1", "d1"))
            .expect("begin");
        store.record_candidate(sample_record("d1")).expect("cand");
        store.promote_candidate(10).expect("promote");
        drop(store);

        // Corrupt the primary file; backup should still decode.
        fs::write(td.path().join("state.json"), b"{not json").expect("write");
        let store = StateStore::open(td.path()).expect("reopen falls back to backup");
        let s = store.snapshot().expect("snap");
        assert_eq!(s.active.as_ref().expect("active").deployment_id, "d1");
    }

    #[test]
    fn corrupt_primary_and_missing_backup_returns_typed_error() {
        let td = TempDir::new().expect("td");
        fs::write(td.path().join("state.json"), b"{nope").expect("write");
        let err = StateStore::open(td.path()).expect_err("must reject corrupt state");
        assert!(matches!(err, super::AgentError::CorruptState(_)));
    }
}
