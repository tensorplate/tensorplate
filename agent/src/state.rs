// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F02: Durable desired-state store and transaction journal.
//
// The agent persists exactly one file (`state.json`) plus a same-directory
// backup (`state.json.bak`) holding the same bytes. Every mutation:
//
//   1. Applies the caller's change to a clone of the current state, bumps
//      `store_version`, and stamps the oldest state version whose readers
//      decode the result without loss (`AgentState::required_schema_version`).
//   2. Refuses the change unless it is a legal successor (the generation
//      counter and the resident set never go backwards) and its encoding
//      decodes back to exactly the same state through the reader's own
//      decoder, so nothing is ever written that the next start would refuse.
//   3. Writes both files, each to a sibling temp file that is `fsync`ed and
//      then renamed over its target:
//        - state version 0.1 (the layout agents through 0.2.x read): the
//          primary first, then the backup, as every earlier release did;
//        - any later state version (0.2): the backup first, then the
//          primary. An older agent falls back to the backup when the
//          primary does not decode, so a 0.2 primary must never sit beside
//          a 0.1 backup. And while the primary decodes it is what the next
//          start reads, so with the primary renamed last an error means the
//          write did not commit. (When the store opened on a missing, empty
//          or damaged primary, the backup is what the next start reads, and
//          a write that fails after the backup rename is visible there.)
//      The parent directory is synced after each rename. Between the two
//      renames of a 0.2 write that sync must succeed where the platform
//      supports it (Linux); elsewhere, and after the last rename, it is
//      best-effort.
//
// `rename(2)` is atomic on POSIX filesystems, so a crash mid-write leaves
// each file either whole and old or whole and new. The reader prefers the
// primary whenever it decodes and consults the backup only when the primary
// is missing, empty or damaged; a primary refused for its state version is
// never replaced by the backup, because that backup belongs to an older
// state the newer file superseded.
//
// State updates are serialized through an in-process `Mutex`; the agent
// has exactly one active control thread mutating state, but the mutex
// guards against future concurrent mutators and against test harnesses
// that share a store across threads. Nothing guards against a second agent
// process writing the same directory.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tensorplate_protocol::agent_state::{
    decode_agent_state, AgentState, DeploymentRecord, ErrorRecord, QuarantineRecord,
    TransactionRecord, AGENT_STATE_SCHEMA_VERSION_LEGACY,
};
use tensorplate_protocol::deploy_transaction::DeployState;
use tensorplate_protocol::DecodeError;

use crate::error::{AgentError, AgentResult};

#[cfg(test)]
mod v021_reader;

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
    /// Test-only fault injection: the write step index at which the next
    /// write stops with an I/O error (see `write_state_files`).
    #[cfg(test)]
    fail_at_step: std::sync::atomic::AtomicUsize,
    /// Test-only fault injection: every directory sync fails.
    #[cfg(test)]
    fail_dir_sync: std::sync::atomic::AtomicBool,
}

/// One file of the pair, written as temp file + `fsync` + rename.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WriteStep {
    Primary,
    Backup,
}

/// Order in which a state of the given version is written. See the module
/// comment for why every state after the legacy layout writes its backup
/// first.
fn write_order(state: &AgentState) -> [WriteStep; 2] {
    if state.schema_version == AGENT_STATE_SCHEMA_VERSION_LEGACY {
        [WriteStep::Primary, WriteStep::Backup]
    } else {
        [WriteStep::Backup, WriteStep::Primary]
    }
}

impl StateStore {
    /// Open the store rooted at `state_dir`. The directory is created if
    /// missing. The primary file is read first; the backup is consulted
    /// when the primary is missing or empty, or fails to decode for any
    /// reason other than an unsupported state version. With neither file
    /// present (or both empty) the store starts from [`AgentState::fresh`].
    ///
    /// A primary that does not decode and has no usable backup is an
    /// explicit corruption error, so the agent does not silently overwrite
    /// operator data; so is a primary at a state version this build does
    /// not read, whatever the backup holds.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Io`] for filesystem failures and
    /// [`AgentError::CorruptState`] when the state cannot be read as
    /// described above.
    pub fn open(state_dir: impl Into<PathBuf>) -> AgentResult<Self> {
        let state_dir = state_dir.into();
        fs::create_dir_all(&state_dir)?;
        let primary = state_dir.join("state.json");
        let backup = state_dir.join("state.json.bak");
        let inner = match load_one(&primary) {
            Ok(Some(s)) => s,
            Ok(None) => match load_one(&backup) {
                Ok(Some(s)) => s,
                Ok(None) => AgentState::fresh(),
                Err(backup_err) => return Err(backup_err.into()),
            },
            Err(primary_err) if primary_err.is_version_refusal() => return Err(primary_err.into()),
            Err(primary_err) => match load_one(&backup) {
                Ok(Some(s)) => s,
                _ => return Err(primary_err.into()),
            },
        };
        Ok(Self {
            state_dir,
            primary,
            backup,
            inner: Mutex::new(inner),
            #[cfg(test)]
            fail_at_step: std::sync::atomic::AtomicUsize::new(usize::MAX),
            #[cfg(test)]
            fail_dir_sync: std::sync::atomic::AtomicBool::new(false),
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
    /// Propagates the closure's error; returns [`AgentError::Internal`]
    /// before anything is written when the result would remove or lower
    /// the generation counter, remove or rename the resident set, change it
    /// without advancing its revision, or not read back unchanged; plus
    /// [`AgentError::Io`] / [`AgentError::Serialization`] from the write
    /// path.
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
        next.schema_version = next.required_schema_version().to_string();
        guard
            .validate_successor(&next)
            .map_err(|e| AgentError::Internal(format!("state update refused: {e}")))?;
        let encoded = encode_checked(&next)?;
        self.write_state_files(&encoded, write_order(&next))?;
        *guard = next;
        Ok(outcome)
    }

    /// Write `encoded` to both files in `order`. See the module comment.
    fn write_state_files(&self, encoded: &[u8], order: [WriteStep; 2]) -> AgentResult<()> {
        for (index, step) in order.into_iter().enumerate() {
            let (target, tmp) = match step {
                WriteStep::Primary => (&self.primary, self.primary.with_extension("json.tmp")),
                WriteStep::Backup => (&self.backup, self.backup.with_extension("bak.tmp")),
            };
            self.inject_failure(2 * index)?;
            {
                let mut f: File = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&tmp)?;
                f.write_all(encoded)?;
                f.sync_all()?;
            }
            self.inject_failure(2 * index + 1)?;
            fs::rename(&tmp, target)?;
            // Where the order matters (between the two renames of a
            // backup-first write) a failed sync is an error on Linux;
            // elsewhere, and after the last rename, it is best-effort.
            let synced = self.sync_state_dir();
            let before_last_rename = index + 1 < order.len();
            if before_last_rename && step == WriteStep::Backup && cfg!(target_os = "linux") {
                synced?;
            }
        }
        Ok(())
    }

    /// Sync the state directory so a completed rename survives power loss.
    fn sync_state_dir(&self) -> std::io::Result<()> {
        #[cfg(test)]
        if self.fail_dir_sync.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "injected directory sync failure",
            ));
        }
        File::open(&self.state_dir).and_then(|d| d.sync_all())
    }

    #[cfg(test)]
    fn inject_failure(&self, point: usize) -> AgentResult<()> {
        if self.fail_at_step.load(std::sync::atomic::Ordering::SeqCst) == point {
            return Err(AgentError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("injected write failure at step {point}"),
            )));
        }
        Ok(())
    }

    #[cfg(not(test))]
    #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
    #[inline]
    fn inject_failure(&self, _point: usize) -> AgentResult<()> {
        Ok(())
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

/// Why a state file could not be loaded.
#[derive(Debug)]
enum LoadFailure {
    Io(std::io::Error),
    Decode { path: PathBuf, error: DecodeError },
}

impl LoadFailure {
    /// A file at a state version this build does not read. Such a file
    /// supersedes any backup beside it, so the backup is not consulted.
    fn is_version_refusal(&self) -> bool {
        matches!(
            self,
            Self::Decode {
                error: DecodeError::UnsupportedSchemaVersion { .. },
                ..
            }
        )
    }
}

impl From<LoadFailure> for AgentError {
    fn from(value: LoadFailure) -> Self {
        match value {
            LoadFailure::Io(err) => AgentError::Io(err),
            LoadFailure::Decode { path, error } => {
                AgentError::CorruptState(format!("decode {}: {error}", path.display()))
            }
        }
    }
}

fn load_one(path: &Path) -> Result<Option<AgentState>, LoadFailure> {
    let raw = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(LoadFailure::Io(err)),
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    decode_agent_state(&raw)
        .map(Some)
        .map_err(|error| LoadFailure::Decode {
            path: path.to_path_buf(),
            error,
        })
}

/// Encode `state` and prove the encoding reads back as exactly `state`
/// through the same decoder `open` uses.
fn encode_checked(state: &AgentState) -> AgentResult<Vec<u8>> {
    let encoded = serde_json::to_vec_pretty(state)?;
    let text = std::str::from_utf8(&encoded)
        .map_err(|e| AgentError::Internal(format!("state encoding is not UTF-8: {e}")))?;
    match decode_agent_state(text) {
        Ok(decoded) if decoded == *state => Ok(encoded),
        Ok(_) => Err(AgentError::Internal(
            "state update refused: the encoded state does not read back unchanged".into(),
        )),
        Err(e) => Err(AgentError::Internal(format!("state update refused: {e}"))),
    }
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

    // ---- State version 0.2: resident set, generation counter, write order ----

    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;

    use tensorplate_protocol::agent_state::{
        decode_agent_state, AgentState, AGENT_STATE_SCHEMA_VERSION_LEGACY,
        AGENT_STATE_SCHEMA_VERSION_RESIDENT_SET,
    };
    use tensorplate_protocol::member_quota::MemberQuota;
    use tensorplate_protocol::resident_set::{
        AdmissionMode, EndpointEntry, MemberState, ResidentMember, ResidentSet, RetainedGeneration,
    };

    use super::v021_reader;
    use crate::error::{AgentError, AgentResult};

    const LEGACY_FIXTURE: &str = "agent_state_0_1_legacy.json";
    const RESTORE_FIXTURE: &str = "agent_state_0_2_restore_step.json";

    fn fixture(name: &str) -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../protocol/rust/tests/fixtures")
            .join(name);
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    fn write_pair(dir: &Path, primary: Option<&str>, backup: Option<&str>) {
        for (name, body) in [("state.json", primary), ("state.json.bak", backup)] {
            if let Some(body) = body {
                fs::write(dir.join(name), body).expect("write");
            }
        }
    }

    fn read(dir: &Path, name: &str) -> Option<Vec<u8>> {
        fs::read(dir.join(name)).ok()
    }

    /// The one-member set a singleton deployment becomes: the fixture
    /// generator for the restore step. Descriptor and configuration digests
    /// are synthetic; the rest is carried from the singleton slots.
    fn migrate_to_one_member_set(s: &mut AgentState) -> AgentResult<()> {
        let active = s
            .active
            .take()
            .ok_or_else(|| AgentError::Internal("no active deployment".into()))?;
        let previous = s.previous_active.take();
        let previous_generation = match previous {
            Some(_) => Some(
                s.allocate_generation(0)
                    .map_err(|e| AgentError::Internal(e.to_string()))?,
            ),
            None => None,
        };
        let generation = s
            .allocate_generation(0)
            .map_err(|e| AgentError::Internal(e.to_string()))?;
        // Synthetic, distinct per generation: no descriptor exists yet.
        let descriptor = |g: u64| format!("sha256:{:0>64}", format!("d{g}"));
        let configuration = |g: u64| format!("sha256:{:0>64}", format!("c{g}"));
        let previous = previous
            .zip(previous_generation)
            .map(|(p, g)| RetainedGeneration {
                deployment_id: p.deployment_id,
                generation: g,
                bundle_digest: p.bundle_digest,
                descriptor_digest: descriptor(g),
                configuration_digest: configuration(g),
                admission_mode: AdmissionMode::Production,
                quota: MemberQuota::default(),
                staged_path: p.staged_path,
                bundle_name: p.bundle_name,
                bundle_version: p.bundle_version,
                backend_hint: p.backend_hint,
                model_class: p.model_class,
                promoted_monotonic_ns: p.promoted_monotonic_ns,
                labels: p.labels,
            });
        let member = ResidentMember {
            deployment_id: active.deployment_id.clone(),
            generation,
            bundle_digest: active.bundle_digest,
            descriptor_digest: descriptor(generation),
            configuration_digest: configuration(generation),
            admission_mode: AdmissionMode::Production,
            quota: MemberQuota::default(),
            state: MemberState::Serving,
            staged_path: active.staged_path,
            bundle_name: active.bundle_name,
            bundle_version: active.bundle_version,
            backend_hint: active.backend_hint,
            model_class: active.model_class,
            promoted_monotonic_ns: active.promoted_monotonic_ns,
            labels: active.labels,
            previous,
        };
        s.resident_set = Some(ResidentSet {
            set_id: "resident-set-1".into(),
            revision: 1,
            endpoint_map: vec![EndpointEntry {
                deployment_id: active.deployment_id,
                generation,
                unary_endpoint: Some("http://127.0.0.1:18080".into()),
                stream_endpoint: None,
            }],
            members: vec![member],
        });
        Ok(())
    }

    #[test]
    fn legacy_writes_stay_0_1_and_readable_by_0_2_1() {
        let td = TempDir::new().expect("td");
        let legacy = fixture(LEGACY_FIXTURE);
        write_pair(td.path(), Some(&legacy), Some(&legacy));
        let store = StateStore::open(td.path()).expect("open");
        store.set_last_error(None).expect("write");
        let written = read(td.path(), "state.json").expect("primary");
        assert_eq!(read(td.path(), "state.json.bak").as_ref(), Some(&written));
        let snapshot = store.snapshot().expect("snap");
        assert_eq!(snapshot.schema_version, AGENT_STATE_SCHEMA_VERSION_LEGACY);
        assert_eq!(
            written,
            serde_json::to_vec_pretty(&snapshot).expect("encode")
        );
        let old = v021_reader::open(td.path()).expect("0.2.1 reads it");
        assert_eq!(
            serde_json::to_value(old).expect("old"),
            serde_json::to_value(&snapshot).expect("new")
        );
    }

    #[test]
    fn allocation_persists_the_counter_at_0_2() {
        let td = TempDir::new().expect("td");
        let store = StateStore::open(td.path()).expect("open");
        assert_eq!(
            store
                .update(|s| Ok(s.allocate_generation(0)))
                .expect("write"),
            Ok(1)
        );
        assert_eq!(
            store
                .update(|s| Ok(s.allocate_generation(0)))
                .expect("write"),
            Ok(2)
        );
        drop(store);
        let reopened = StateStore::open(td.path()).expect("reopen");
        let s = reopened.snapshot().expect("snap");
        assert_eq!(s.schema_version, AGENT_STATE_SCHEMA_VERSION_RESIDENT_SET);
        assert_eq!(s.next_generation, Some(3));
        assert!(v021_reader::open(td.path()).is_err());
    }

    #[test]
    fn restore_step_fixture_is_what_the_writer_produces() {
        let td = TempDir::new().expect("td");
        let legacy = fixture(LEGACY_FIXTURE);
        write_pair(td.path(), Some(&legacy), Some(&legacy));
        let store = StateStore::open(td.path()).expect("open");
        store.update(migrate_to_one_member_set).expect("migrate");
        let primary = read(td.path(), "state.json").expect("primary");
        assert_eq!(read(td.path(), "state.json.bak").as_ref(), Some(&primary));
        let expected = fixture(RESTORE_FIXTURE);
        assert_eq!(
            String::from_utf8(primary).expect("utf8"),
            expected.strip_suffix('\n').unwrap_or(&expected),
            "{RESTORE_FIXTURE} no longer matches the writer's output for {LEGACY_FIXTURE}"
        );
    }

    #[test]
    fn restore_step_pair_is_refused_by_0_2_1_and_read_by_this_agent() {
        let td = TempDir::new().expect("td");
        let restore = fixture(RESTORE_FIXTURE);
        write_pair(td.path(), Some(&restore), Some(&restore));
        let s = StateStore::open(td.path())
            .expect("open")
            .snapshot()
            .expect("snap");
        let set = s.resident_set.expect("set");
        assert_eq!(set.members.len(), 1);
        assert_eq!(set.members[0].deployment_id, "vision-v2");
        assert_eq!(
            set.members[0]
                .previous
                .as_ref()
                .expect("previous")
                .deployment_id,
            "vision-v1"
        );
        assert_eq!(
            v021_reader::open(td.path()).expect_err("refused"),
            format!(
                "durable state file is corrupt or malformed: decode {}: unsupported schema_version `0.2` (expected `0.1`)",
                td.path().join("state.json").display()
            )
        );
    }

    #[test]
    fn interrupted_transition_pair_reads_as_the_legacy_state_everywhere() {
        let td = TempDir::new().expect("td");
        let legacy = fixture(LEGACY_FIXTURE);
        write_pair(td.path(), Some(&legacy), Some(&fixture(RESTORE_FIXTURE)));
        let new = StateStore::open(td.path())
            .expect("open")
            .snapshot()
            .expect("snap");
        assert_eq!(new, decode_agent_state(&legacy).expect("legacy"));
        let old = v021_reader::open(td.path()).expect("0.2.1 reads the primary");
        assert_eq!(
            serde_json::to_value(old).expect("old"),
            serde_json::to_value(&new).expect("new")
        );
    }

    #[test]
    fn a_0_2_backup_alone_is_read_here_and_refused_by_0_2_1() {
        let td = TempDir::new().expect("td");
        write_pair(td.path(), None, Some(&fixture(RESTORE_FIXTURE)));
        let s = StateStore::open(td.path())
            .expect("open")
            .snapshot()
            .expect("snap");
        assert!(s.resident_set.is_some());
        let err = v021_reader::open(td.path()).expect_err("refused");
        assert!(
            err.contains("state.json.bak: unsupported schema_version `0.2`"),
            "{err}"
        );
    }

    #[test]
    fn a_primary_at_an_unknown_state_version_is_not_replaced_by_the_backup() {
        let td = TempDir::new().expect("td");
        let legacy = fixture(LEGACY_FIXTURE);
        write_pair(
            td.path(),
            Some(r#"{"schema_version":"0.3","store_version":99}"#),
            Some(&legacy),
        );
        match StateStore::open(td.path()).expect_err("refused") {
            super::AgentError::CorruptState(message) => assert_eq!(
                message,
                format!(
                    "decode {}: unsupported schema_version `0.3` (expected `0.1 or 0.2`)",
                    td.path().join("state.json").display()
                )
            ),
            other => panic!("expected CorruptState, got {other:?}"),
        }
        // A damaged primary still falls back.
        fs::write(td.path().join("state.json"), b"{\"schema_version\":").expect("write");
        let s = StateStore::open(td.path())
            .expect("falls back")
            .snapshot()
            .expect("snap");
        assert_eq!(s, decode_agent_state(&legacy).expect("legacy"));
    }

    type Change = Box<dyn Fn(&mut AgentState)>;

    #[test]
    fn refused_updates_change_neither_disk_nor_memory() {
        let td = TempDir::new().expect("td");
        let store = StateStore::open(td.path()).expect("open");
        store
            .update(|s| Ok(s.allocate_generation(0)))
            .expect("allocate")
            .expect("gen");
        let before_disk = (
            read(td.path(), "state.json"),
            read(td.path(), "state.json.bak"),
        );
        let before_memory = store.snapshot().expect("snap");
        let cases: Vec<(&str, Change)> = vec![
            (
                "next_generation may not be removed once allocated",
                Box::new(|s| s.next_generation = None),
            ),
            (
                "next_generation may not go backwards (2 -> 1)",
                Box::new(|s| s.next_generation = Some(1)),
            ),
            (
                "generation 5 was never allocated (next_generation is 2)",
                Box::new(|s| s.resident_set = Some(unallocated_set())),
            ),
        ];
        for (expected, change) in cases {
            let err = store
                .update(|s| {
                    change(s);
                    Ok(())
                })
                .expect_err("refused");
            let message = err.to_string();
            assert!(message.contains(expected), "{message}");
            assert_eq!(
                (
                    read(td.path(), "state.json"),
                    read(td.path(), "state.json.bak")
                ),
                before_disk
            );
            assert_eq!(store.snapshot().expect("snap"), before_memory);
        }
    }

    /// A set whose member generation (5) was never allocated from a counter
    /// of 2; refused by the read-back check, not by the successor rules.
    fn unallocated_set() -> ResidentSet {
        let state = decode_agent_state(&fixture(RESTORE_FIXTURE)).expect("restore");
        let mut set = state.resident_set.expect("set");
        set.members[0].generation = 5;
        set.endpoint_map[0].generation = 5;
        set
    }

    // ---- Crash harness ----------------------------------------------------
    //
    // Every write is interrupted at each point of the real write loop (see
    // `write_state_files`: 0 before the first temp file, 1 after it, 2 after
    // the first rename, 3 after the second temp file), from each starting
    // pair of files, optionally followed by another write on the same store
    // (the error-and-continue path). Then the process "dies": the store is
    // dropped and both readers open the directory.

    #[derive(Clone, Copy, Debug)]
    enum Start {
        LegacyPair,
        Empty,
        LegacyBackupOnly,
        DamagedPrimaryLegacyBackup,
        InterruptedTransition,
        ResidentSetPair,
    }

    #[derive(Clone, Copy, Debug)]
    enum Write {
        Allocate,
        Touch,
    }

    fn prepare(dir: &Path, start: Start) {
        let legacy = fixture(LEGACY_FIXTURE);
        let restore = fixture(RESTORE_FIXTURE);
        match start {
            Start::LegacyPair => write_pair(dir, Some(&legacy), Some(&legacy)),
            Start::Empty => {}
            Start::LegacyBackupOnly => write_pair(dir, None, Some(&legacy)),
            Start::DamagedPrimaryLegacyBackup => {
                write_pair(dir, Some("{\"schema_version\":"), Some(&legacy));
            }
            Start::InterruptedTransition => write_pair(dir, Some(&legacy), Some(&restore)),
            Start::ResidentSetPair => write_pair(dir, Some(&restore), Some(&restore)),
        }
    }

    fn change(s: &mut AgentState, write: Write, step: usize) -> Option<u64> {
        match write {
            Write::Allocate => Some(s.allocate_generation(0).expect("counter")),
            Write::Touch => {
                s.last_error = Some(ErrorRecord::new(
                    ErrorCode::Internal,
                    format!("touch {step}"),
                ));
                None
            }
        }
    }

    /// What `update` would write for `write` over `memory`.
    fn attempted(memory: &AgentState, write: Write, step: usize) -> AgentState {
        let mut next = memory.clone();
        change(&mut next, write, step);
        next.store_version += 1;
        next.schema_version = next.required_schema_version().to_string();
        next
    }

    fn run_scenario(start: Start, writes: &[(Write, Option<usize>)]) {
        let label = format!("{start:?} {writes:?}");
        let td = TempDir::new().expect("td");
        prepare(td.path(), start);
        let store = StateStore::open(td.path()).unwrap_or_else(|e| panic!("{label}: open {e}"));
        let mut acceptable = vec![store.snapshot().expect("snap")];
        let mut issued: Vec<u64> = Vec::new();
        let mut last_ok = true;
        for (step, (write, fail_at)) in writes.iter().enumerate() {
            let memory = store.snapshot().expect("snap");
            // What the next start reads while this write is in progress:
            // the primary when it decodes, else the backup.
            let primary_decodes = read(td.path(), "state.json")
                .and_then(|b| String::from_utf8(b).ok())
                .is_some_and(|raw| decode_agent_state(&raw).is_ok());
            store
                .fail_at_step
                .store(fail_at.unwrap_or(usize::MAX), Ordering::SeqCst);
            let result = store.update(|s| Ok(change(s, *write, step)));
            match (result, fail_at) {
                (Ok(generation), None) => {
                    issued.extend(generation);
                    acceptable = vec![store.snapshot().expect("snap")];
                    last_ok = true;
                }
                (Err(err), Some(point)) => {
                    assert!(
                        err.to_string()
                            .contains(&format!("injected write failure at step {point}")),
                        "{label}: {err}"
                    );
                    assert_eq!(
                        store.snapshot().expect("snap"),
                        memory,
                        "{label}: memory moved"
                    );
                    // A failed write may be visible after a crash only where
                    // the design says it can be: a legacy write renames the
                    // primary first, and a later-version write is readable
                    // early only when the next start reads the backup.
                    let attempt = attempted(&memory, *write, step);
                    if attempt.schema_version == AGENT_STATE_SCHEMA_VERSION_LEGACY
                        || !primary_decodes
                    {
                        acceptable.push(attempt);
                    }
                    last_ok = false;
                }
                (other, _) => panic!("{label}: unexpected result {other:?}"),
            }
        }
        drop(store);

        let new = StateStore::open(td.path())
            .unwrap_or_else(|e| panic!("{label}: reopen {e}"))
            .snapshot()
            .expect("snap");
        assert!(
            acceptable.contains(&new),
            "{label}: loaded a state that was neither committed nor attempted: {new:?}"
        );
        let counter = new.next_generation.unwrap_or(1);
        assert!(
            issued.iter().all(|g| *g < counter),
            "{label}: counter {counter} would reissue one of {issued:?}"
        );
        match v021_reader::open(td.path()) {
            Ok(old) => {
                assert_eq!(
                    new.schema_version, AGENT_STATE_SCHEMA_VERSION_LEGACY,
                    "{label}: 0.2.1 started on a state this agent reads at 0.2"
                );
                assert_eq!(
                    serde_json::to_value(old).expect("old"),
                    serde_json::to_value(&new).expect("new"),
                    "{label}: 0.2.1 and this agent read different states"
                );
            }
            Err(message) => assert_eq!(
                new.schema_version, AGENT_STATE_SCHEMA_VERSION_RESIDENT_SET,
                "{label}: 0.2.1 refused a 0.1 state: {message}"
            ),
        }
        if let Some(primary) = read(td.path(), "state.json") {
            let primary = String::from_utf8(primary).expect("utf8");
            if decode_agent_state(&primary)
                .is_ok_and(|s| s.schema_version == AGENT_STATE_SCHEMA_VERSION_RESIDENT_SET)
            {
                let backup = read(td.path(), "state.json.bak")
                    .map(|b| String::from_utf8(b).expect("utf8"))
                    .unwrap_or_default();
                assert!(
                    v021_reader::decode(&backup).is_err(),
                    "{label}: a 0.2 primary sits beside a backup 0.2.1 can read"
                );
            }
        }
        if last_ok {
            for tmp in ["state.json.tmp", "state.json.bak.tmp"] {
                assert!(!td.path().join(tmp).exists(), "{label}: {tmp} left behind");
            }
        }
    }

    #[test]
    fn every_interrupted_write_leaves_a_state_both_readers_agree_on() {
        let starts = [
            Start::LegacyPair,
            Start::Empty,
            Start::LegacyBackupOnly,
            Start::DamagedPrimaryLegacyBackup,
            Start::InterruptedTransition,
            Start::ResidentSetPair,
        ];
        let points = [None, Some(0), Some(1), Some(2), Some(3)];
        let follow_ups = [
            None,
            Some((Write::Allocate, None)),
            Some((Write::Touch, Some(2))),
        ];
        let mut scenarios = 0;
        for start in starts {
            for write in [Write::Allocate, Write::Touch] {
                for point in points {
                    for follow_up in follow_ups {
                        let mut writes = vec![(write, point)];
                        writes.extend(follow_up);
                        run_scenario(start, &writes);
                        scenarios += 1;
                    }
                }
            }
        }
        assert_eq!(scenarios, 180);
    }

    #[test]
    fn the_transition_write_renames_the_backup_first() {
        // Stopped after its first rename, the write from a committed 0.1 pair
        // to 0.2 must have replaced the backup and left the primary alone.
        let td = TempDir::new().expect("td");
        let legacy = fixture(LEGACY_FIXTURE);
        write_pair(td.path(), Some(&legacy), Some(&legacy));
        let store = StateStore::open(td.path()).expect("open");
        store.fail_at_step.store(2, Ordering::SeqCst);
        store
            .update(|s| Ok(s.allocate_generation(0)))
            .expect_err("stopped");
        assert_eq!(read(td.path(), "state.json"), Some(legacy.into_bytes()));
        let backup =
            String::from_utf8(read(td.path(), "state.json.bak").expect("bak")).expect("utf8");
        assert_eq!(
            decode_agent_state(&backup).expect("0.2").schema_version,
            AGENT_STATE_SCHEMA_VERSION_RESIDENT_SET
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_failed_sync_between_the_renames_stops_a_0_2_write() {
        let td = TempDir::new().expect("td");
        let legacy = fixture(LEGACY_FIXTURE);
        write_pair(td.path(), Some(&legacy), Some(&legacy));
        let store = StateStore::open(td.path()).expect("open");
        store.fail_dir_sync.store(true, Ordering::SeqCst);
        let err = store
            .update(|s| Ok(s.allocate_generation(0)))
            .expect_err("the sync after the backup rename must succeed");
        assert!(
            err.to_string().contains("injected directory sync failure"),
            "{err}"
        );
        assert_eq!(
            read(td.path(), "state.json"),
            Some(legacy.clone().into_bytes())
        );
        assert_eq!(
            store.snapshot().expect("snap"),
            decode_agent_state(&legacy).expect("legacy")
        );
    }

    #[test]
    fn a_failed_sync_does_not_stop_a_legacy_write() {
        let td = TempDir::new().expect("td");
        let legacy = fixture(LEGACY_FIXTURE);
        write_pair(td.path(), Some(&legacy), Some(&legacy));
        let store = StateStore::open(td.path()).expect("open");
        store.fail_dir_sync.store(true, Ordering::SeqCst);
        store.set_last_error(None).expect("best-effort sync");
        let written = read(td.path(), "state.json").expect("primary");
        assert_eq!(read(td.path(), "state.json.bak"), Some(written.clone()));
        let s = decode_agent_state(&String::from_utf8(written).expect("utf8")).expect("decode");
        assert_eq!(s.schema_version, AGENT_STATE_SCHEMA_VERSION_LEGACY);
        assert!(s.last_error.is_none());
    }
}
