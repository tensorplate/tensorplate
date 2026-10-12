// SPDX-License-Identifier: Apache-2.0

//! The deployment descriptor documents the agent keeps beside its state:
//! one file per deployment generation, named by the records that carry its
//! identity. A document is written inside the state update that names it,
//! so one that cannot be written fails the update, and a crash between the
//! two writes leaves a document no record names. That update also takes the
//! generation, under a hold on the worker control that lasts until `prepare`
//! has returned.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tensorplate_protocol::agent_control::is_valid_deployment_id;
use tensorplate_protocol::agent_state::{AgentState, DeploymentRecord, TransactionKind};
use tensorplate_protocol::backend_descriptor::RunnerProfile;
use tensorplate_protocol::bundle_profile::PROFILE_FORMAT_VERSION;
use tensorplate_protocol::deployment_descriptor::DeploymentDescriptorError;
use tensorplate_protocol::worker_control::CandidateRef;
use tensorplate_protocol::{
    AdmissionMode, ArtifactRole, DeploymentDescriptor, DescriptorIdentity, DescriptorInputs,
    ErrorCode, MemberQuota, ProtocolError,
};

use crate::backend_detection::BackendProbeReport;
use crate::error::{AgentError, AgentResult};
use crate::state::StateStore;
use crate::worker::WorkerControl;

const PREFIX: &str = "descriptor-";
const SUFFIX: &str = ".json";
/// Appended to a document's name while it is being written.
const UNFINISHED: &str = ".tmp";
/// The most a document holds: a descriptor is a few kilobytes and a few
/// hundred bytes for each artifact, so this is room for thousands of them.
const MAX_DOCUMENT_BYTES: u64 = 1 << 20;

/// The one place a document's file name is built; none for an id that is
/// not a deployment id. Such an id is ASCII letters, digits, `-`, `_` and
/// `.`: no path separator, and behind the prefix a leading dot hides
/// nothing. A generation holds no `-`, so the last one ends the id.
fn file_name(deployment_id: &str, generation: u64) -> Option<String> {
    is_valid_deployment_id(deployment_id)
        .then(|| format!("{PREFIX}{deployment_id}-{generation}{SUFFIX}"))
}

/// Whether `name` is what [`file_name`] builds, or that of a write that did
/// not finish.
fn is_document(name: &str) -> bool {
    let document = name.strip_suffix(UNFINISHED).unwrap_or(name);
    document
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.strip_suffix(SUFFIX))
        .and_then(|rest| rest.rsplit_once('-'))
        .and_then(|(id, generation)| file_name(id, generation.parse().ok()?))
        .is_some_and(|built| built == document)
}

fn unusable(message: String) -> AgentError {
    AgentError::DeploymentDescriptor {
        code: ErrorCode::LoadFailed,
        message,
    }
}

fn refused(path: &Path, reason: impl std::fmt::Display) -> AgentError {
    unusable(format!("`{}` {reason}", path.display()))
}

fn named(deployment_id: &str, generation: u64) -> AgentResult<String> {
    file_name(deployment_id, generation).ok_or_else(|| {
        unusable(format!(
            "of generation {generation} has no document: `{deployment_id}` is not a deployment id"
        ))
    })
}

/// The descriptor identity `candidate` carries, whole or not at all.
///
/// # Errors
///
/// [`AgentError::DeploymentDescriptor`] when it carries only part of one.
pub fn identity_of(candidate: &CandidateRef) -> AgentResult<Option<DescriptorIdentity<'_>>> {
    let id = &candidate.deployment_id;
    candidate.descriptor_identity().map_err(|e| {
        unusable(format!(
            "of candidate `{id}` is not named: the reference {e}"
        ))
    })
}

/// The descriptor `candidate` names, read from `store` and verified; none
/// when it names none. `generation` is the one being started.
///
/// # Errors
///
/// [`AgentError::DeploymentDescriptor`] when the candidate names another
/// generation's descriptor, there is no store, the read is refused, or the
/// candidate states another bundle digest, staged path, model class or
/// backend than the descriptor, or carries another model artifact path,
/// bundle name or bundle version.
pub fn resolve(
    store: Option<&DescriptorStore>,
    candidate: &CandidateRef,
    generation: u64,
) -> AgentResult<Option<DeploymentDescriptor>> {
    let Some(identity) = identity_of(candidate)? else {
        return Ok(None);
    };
    let (id, named) = (&candidate.deployment_id, identity.generation);
    if named != generation {
        return Err(unusable(format!(
            "of `{id}` generation {named} is not that of generation {generation}, which is being started"
        )));
    }
    let Some(store) = store else {
        return Err(unusable(format!(
            "of `{id}` generation {named} cannot be read: no descriptor store"
        )));
    };
    let descriptor = store.read(id, identity)?;
    let configuration = &descriptor.configuration;
    let disagrees = |field: &str, described: &str, referenced: &str| {
        unusable(format!(
            "of `{id}` generation {named} is not the candidate's: its `{field}` is `{described}`, and the reference's is `{referenced}`"
        ))
    };
    let digest = &configuration.bundle.bundle_digest;
    if !crate::bundle::bundle_digests_equal(digest, &candidate.bundle_digest) {
        return Err(disagrees("bundle_digest", digest, &candidate.bundle_digest));
    }
    let mut artifacts = configuration.artifacts.iter();
    let model = artifacts.find(|artifact| artifact.role == ArtifactRole::Model);
    let (bundle, carried) = (&configuration.bundle, candidate);
    // The last three are compared when the reference carries them.
    let stated = [
        (
            "staged_path",
            &*descriptor.staged_path,
            Some(&*carried.staged_path),
        ),
        (
            "model_class",
            configuration.model_class.as_str(),
            Some(&*carried.model_class),
        ),
        (
            "backend_hint",
            &*configuration.backend_hint,
            Some(&*carried.backend_hint),
        ),
        (
            "artifact_relative_path",
            model.map_or("", |model| &*model.path),
            carried.artifact_relative_path.as_deref(),
        ),
        ("bundle_name", &*bundle.name, carried.bundle_name.as_deref()),
        (
            "bundle_version",
            &*bundle.version,
            carried.bundle_version.as_deref(),
        ),
    ];
    for (field, described, referenced) in stated {
        if let Some(referenced) = referenced.filter(|referenced| *referenced != described) {
            return Err(disagrees(field, described, referenced));
        }
    }
    Ok(Some(descriptor))
}

/// One step of a document write, which a test can make fail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Step {
    SyncFile,
    Rename,
    SyncDirectory,
}

/// The descriptor documents in one directory, the agent's state directory.
/// Nothing is created there before the first document is written.
#[derive(Debug)]
pub struct DescriptorStore {
    dir: PathBuf,
    #[cfg(test)]
    fail_at: std::sync::Mutex<Option<Step>>,
}

impl DescriptorStore {
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            #[cfg(test)]
            fail_at: std::sync::Mutex::new(None),
        }
    }

    fn path(&self, deployment_id: &str, generation: u64) -> AgentResult<PathBuf> {
        Ok(self.dir.join(named(deployment_id, generation)?))
    }

    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn step(&self, step: Step, run: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
        #[cfg(test)]
        if self.fail_at.lock().is_ok_and(|at| *at == Some(step)) {
            let stopped = format!("stopped at {step:?}");
            return Err(io::Error::new(ErrorKind::Other, stopped));
        }
        #[cfg(not(test))]
        let _ = step;
        run()
    }

    /// Write `descriptor` as the document of its generation: a temporary
    /// file, synced, renamed over the document, then the directory synced.
    /// A write that fails removes its temporary file. Called inside the
    /// state update that names the descriptor.
    ///
    /// # Errors
    ///
    /// [`AgentError::DeploymentDescriptor`] for a descriptor that does not
    /// verify, and [`AgentError::Io`] when a step fails.
    pub fn write(&self, descriptor: &DeploymentDescriptor) -> AgentResult<()> {
        let name = named(&descriptor.deployment_id, descriptor.generation)?;
        let path = self.dir.join(&name);
        descriptor
            .verify()
            .map_err(|e| refused(&path, format!("is not written: {e}")))?;
        let bytes = serde_json::to_vec_pretty(descriptor)?;
        let unfinished = self.dir.join(format!("{name}{UNFINISHED}"));
        let write = || {
            let mut file = File::create(&unfinished)?;
            file.write_all(&bytes)?;
            self.step(Step::SyncFile, || file.sync_all())?;
            self.step(Step::Rename, || fs::rename(&unfinished, &path))?;
            // A rename that is not synced can be lost while the state that names it is kept.
            self.step(Step::SyncDirectory, || {
                let synced = File::open(&self.dir).and_then(|dir| dir.sync_all());
                synced.or_else(|e| if cfg!(unix) { Err(e) } else { Ok(()) })
            })
        };
        let failed = |e: io::Error| {
            let _ = fs::remove_file(&unfinished);
            io::Error::new(e.kind(), format!("{}: {e}", path.display()))
        };
        Ok(write().map_err(failed)?)
    }

    /// The text of the document at `path`. Only a regular file is opened:
    /// a FIFO there would block the state update the read runs in. A
    /// document that cannot be read cannot be verified.
    fn text(path: &Path) -> AgentResult<String> {
        let unverified = |reason: String| refused(path, format!("does not verify: {reason}"));
        let unread = |e: io::Error| match e.kind() {
            ErrorKind::NotFound => refused(path, "is missing"),
            _ => unverified(format!("it cannot be read: {e}")),
        };
        if !fs::symlink_metadata(path).map_err(unread)?.is_file() {
            return Err(unverified("it is not a regular file".into()));
        }
        let mut bytes = Vec::new();
        let mut bounded = File::open(path)
            .map_err(unread)?
            .take(MAX_DOCUMENT_BYTES + 1);
        bounded.read_to_end(&mut bytes).map_err(unread)?;
        if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
            return Err(unverified(format!(
                "it holds more than {MAX_DOCUMENT_BYTES} bytes"
            )));
        }
        String::from_utf8(bytes).map_err(|e| unverified(format!("it is not UTF-8: {e}")))
    }

    /// Read the document `identity` names for `deployment_id` and verify
    /// that it is that descriptor.
    ///
    /// # Errors
    ///
    /// [`AgentError::DeploymentDescriptor`] when the document is missing,
    /// cannot be read, is not a regular file of at most
    /// [`MAX_DOCUMENT_BYTES`] of UTF-8, does not verify, or is not the one
    /// named.
    pub fn read(
        &self,
        deployment_id: &str,
        identity: DescriptorIdentity<'_>,
    ) -> AgentResult<DeploymentDescriptor> {
        let path = self.path(deployment_id, identity.generation)?;
        let text = Self::text(&path)?;
        let descriptor = DeploymentDescriptor::from_json(&text)
            .map_err(|e| refused(&path, format!("does not verify: {e}")))?;
        if descriptor.deployment_id != deployment_id || descriptor.identity() != identity {
            let show = |id: &str, of: DescriptorIdentity<'_>| {
                let (descriptor, configuration) = (of.descriptor_digest, of.configuration_digest);
                let generation = of.generation;
                format!("generation {generation} of `{id}` (descriptor `{descriptor}`, configuration `{configuration}`)")
            };
            let found = show(&descriptor.deployment_id, descriptor.identity());
            let named = show(deployment_id, identity);
            return Err(refused(&path, format!("is {found}, not {named}")));
        }
        Ok(descriptor)
    }

    /// The descriptor `identity` names for `deployment_id`, read, verified
    /// and re-stamped for `generation`. It must describe `bundle_digest`,
    /// which the caller has compared with the manifest at `staged_path`.
    fn restamped(
        &self,
        deployment_id: &str,
        identity: DescriptorIdentity<'_>,
        bundle_digest: &str,
        staged_path: &str,
        generation: u64,
    ) -> AgentResult<DeploymentDescriptor> {
        let path = self.path(deployment_id, identity.generation)?;
        let unstamped = |reason: String| refused(&path, format!("is not re-stamped: {reason}"));
        let current = self.read(deployment_id, identity)?;
        let described = &current.configuration.bundle.bundle_digest;
        if !crate::bundle::bundle_digests_equal(described, bundle_digest) {
            return Err(unstamped(format!(
                "it describes bundle `{described}`, and `{bundle_digest}` is staged at `{staged_path}`"
            )));
        }
        let next = current.restamp(generation);
        next.map_err(|e| unstamped(e.to_string()))
    }

    /// Remove every document no record of `state` names, and every write
    /// that did not finish; returns how many files were removed. Only for
    /// a start: later it would race the write of a document not named yet.
    ///
    /// # Errors
    ///
    /// An [`AgentError`] when what a record names cannot be told, and then
    /// nothing is removed; [`AgentError::Io`] when the directory cannot be
    /// read or a file cannot be removed.
    pub fn prune(&self, state: &AgentState) -> AgentResult<usize> {
        self.remove_all_but(&named_by(state)?)
    }

    fn remove_all_but(&self, kept: &BTreeSet<String>) -> AgentResult<usize> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        let mut removed = 0;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if is_document(name) && !kept.contains(name) && entry.file_type()?.is_file() {
                fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

/// The file name of every document a record of `state` names: the
/// singleton records, the in-flight transaction, and the resident set's
/// members with the generations they retain.
fn named_by(state: &AgentState) -> AgentResult<BTreeSet<String>> {
    let records = [&state.active, &state.previous_active, &state.candidate]
        .into_iter()
        .flatten()
        .map(|record| (&record.deployment_id, record.descriptor_identity()));
    let transaction = state
        .in_flight_transaction
        .iter()
        .map(|tx| (&tx.deployment_id, tx.descriptor_identity()));
    let mut kept = BTreeSet::new();
    for (deployment_id, identity) in records.chain(transaction) {
        let identity = identity.map_err(|e| {
            AgentError::Internal(format!("a state record of `{deployment_id}` {e}"))
        })?;
        if let Some(identity) = identity {
            kept.insert(named(deployment_id, identity.generation)?);
        }
    }
    for member in state.resident_set.iter().flat_map(|set| &set.members) {
        kept.insert(named(&member.deployment_id, member.generation)?);
        if let Some(previous) = &member.previous {
            kept.insert(named(&previous.deployment_id, previous.generation)?);
        }
    }
    Ok(kept)
}

/// Remove, at a start, every document that neither file of the state pair
/// names. The two differ after a write that stopped between its renames,
/// and a later start reads the backup once the primary is damaged.
///
/// # Errors
///
/// As [`DescriptorStore::prune`], and when what a state file holds cannot
/// be told (it cannot be read, or is at a state version this build does not
/// read); nothing is removed then.
pub fn prune_at_start(store: &StateStore) -> AgentResult<usize> {
    let mut kept = BTreeSet::new();
    for state in store.files()? {
        kept.append(&mut named_by(&state)?);
    }
    DescriptorStore::new(store.state_dir()).remove_all_but(&kept)
}

/// The identity fields of a record that names `descriptor`.
fn naming(descriptor: &DeploymentDescriptor) -> (Option<u64>, Option<String>, Option<String>) {
    let descriptor_digest = descriptor.descriptor_digest.clone();
    let configuration_digest = descriptor.configuration_digest.clone();
    let generation = Some(descriptor.generation);
    (
        generation,
        Some(descriptor_digest),
        Some(configuration_digest),
    )
}

/// Which record the agent starts again, and so where the new generation's
/// descriptor is named.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Start<'a> {
    /// Recovery restores `active`: named on it.
    Restore,
    /// The rollback transaction of this id restores `previous_active`:
    /// named on the transaction, and the swap gives it to the record.
    Rollback(&'a str),
}

/// The refusal to start `record` again when the bundle staged at its path is
/// not the one it names: staging is keyed by the deployment id, so a later
/// deploy under the id replaced it. None when the staged bundle is the
/// record's, and for a record that does not name a descriptor, which is
/// started from whatever is staged.
///
/// # Errors
///
/// The staged manifest's own error when it cannot be read.
pub(crate) fn another_bundle_staged(record: &DeploymentRecord) -> AgentResult<Option<AgentError>> {
    if !matches!(record.descriptor_identity(), Ok(Some(_))) {
        return Ok(None);
    }
    let (id, digest) = (&record.deployment_id, &*record.bundle_digest);
    let staged_path = &*record.staged_path;
    let staged = crate::bundle::staged_manifest(Path::new(staged_path))?.bundle_digest;
    if crate::bundle::bundle_digests_equal(&staged, digest) {
        return Ok(None);
    }
    Ok(Some(AgentError::BundleIntegrity {
        path: "manifest.json".into(),
        reason: format!(
            "`{id}` is not started again: the bundle staged at `{staged_path}` is `{staged}`, not the `{digest}` its record names"
        ),
    }))
}

/// The reference `record` is started again from. For a record that names a
/// descriptor, one state update takes the next generation, re-stamps the
/// descriptor for it, names it where `start` says and writes its document;
/// the reference names it, in hand under a hold on `worker`. A record that
/// names none is referenced as it is.
///
/// # Errors
///
/// For a described record: the staged manifest's own error when it cannot
/// be read, and [`AgentError::BundleIntegrity`] when it is not the record's
/// bundle; [`AgentError::DeploymentDescriptor`] when the record no longer
/// stands where `start` reads it, its document is refused or describes
/// another bundle, or a rollback's transaction is not in flight; and the
/// errors of the state update.
pub(crate) fn start_again<'w>(
    store: &StateStore,
    worker: &'w dyn WorkerControl,
    record: &DeploymentRecord,
    artifact_relative_path: String,
    start: Start<'_>,
) -> AgentResult<InHand<'w>> {
    let id = &record.deployment_id;
    let reference = reference_of(record, Some(artifact_relative_path));
    let named = record.descriptor_identity();
    let named = named.map_err(|e| unusable(format!("of `{id}` is not named: its record {e}")))?;
    let Some(identity) = named else {
        return Ok(InHand::as_recorded(worker, reference));
    };
    // `prepare` reads no staged manifest for a reference that names a descriptor.
    if let Some(refusal) = another_bundle_staged(record)? {
        return Err(refusal);
    }
    let (digest, staged_path) = (&*record.bundle_digest, &*record.staged_path);
    let descriptors = DescriptorStore::new(store.state_dir());
    let path = descriptors.path(id, identity.generation)?;
    let unstamped = |reason: String| refused(&path, format!("is not re-stamped: {reason}"));
    let guard = Held::new(worker);
    let next = store.update(|state| {
        let (slot, held) = match start {
            Start::Restore => (&state.active, "active"),
            Start::Rollback(_) => (&state.previous_active, "previous active"),
        };
        if slot.as_ref() != Some(record) {
            return Err(unstamped(format!(
                "the {held} deployment record does not name it"
            )));
        }
        let generation = state.allocate_generation(1);
        let generation = generation.map_err(|e| unstamped(e.to_string()))?;
        let next = descriptors.restamped(id, identity, digest, staged_path, generation)?;
        let (generation, descriptor_digest, configuration_digest) = naming(&next);
        match start {
            Start::Restore => {
                state.active = Some(DeploymentRecord {
                    generation,
                    descriptor_digest,
                    configuration_digest,
                    ..record.clone()
                });
            }
            Start::Rollback(transaction_id) => {
                let in_flight = state.in_flight_transaction.as_mut().filter(|tx| {
                    tx.transaction_id == transaction_id
                        && tx.kind == TransactionKind::Rollback
                        && tx.deployment_id == *id
                });
                let rollback = in_flight.ok_or_else(|| {
                    unstamped(format!(
                        "rollback transaction `{transaction_id}` of `{id}` is not in flight"
                    ))
                })?;
                rollback.generation = generation;
                rollback.descriptor_digest = descriptor_digest;
                rollback.configuration_digest = configuration_digest;
            }
        }
        descriptors.write(&next)?;
        Ok(next)
    })?;
    Ok(InHand::naming(worker, reference, &next, guard))
}

/// What a deploy starts the first generation of a staged bundle from.
pub(crate) struct Deploy<'a> {
    pub(crate) transaction_id: &'a str,
    /// The candidate record, as the deploy recorded it.
    pub(crate) record: &'a DeploymentRecord,
    pub(crate) artifact_relative_path: Option<String>,
    /// The verified manifest's; only a format 0.2 bundle gets a descriptor.
    pub(crate) format_version: &'a str,
    pub(crate) probes: &'a BTreeMap<String, BackendProbeReport>,
}

/// The reference a deploy starts its candidate from. For a format 0.2
/// bundle the staged copy is verified again, then one state update takes
/// the next generation, derives the descriptor from that copy, names it on
/// the candidate record and the deploy's transaction and writes its
/// document; the reference names it, in hand under a hold on `worker`. A
/// bundle of another format is referenced as it was before descriptors.
///
/// The quota is the default: none is assigned yet, and it is not a limit.
/// The runtime version is the one this agent runs, whatever its
/// configuration gives the compatibility check to judge against.
///
/// # Errors
///
/// For a format 0.2 bundle: the staged copy's own error when it does not
/// verify, and [`AgentError::BundleIntegrity`] when it is not the bundle
/// the record names; [`AgentError::DeploymentDescriptor`] when the candidate
/// record or the transaction no longer stands, or the descriptor cannot be
/// derived (`unsupported` for a runner profile that is not installed or a
/// compute type it does not list, `config_invalid` otherwise); and the
/// errors of the state update.
pub(crate) fn start_deploy<'w>(
    store: &StateStore,
    worker: &'w dyn WorkerControl,
    deploy: &Deploy<'_>,
) -> AgentResult<InHand<'w>> {
    let record = deploy.record;
    let id = &record.deployment_id;
    let reference = reference_of(record, deploy.artifact_relative_path.clone());
    if deploy.format_version != PROFILE_FORMAT_VERSION {
        return Ok(InHand::as_recorded(worker, reference));
    }
    let (digest, staged_path) = (&*record.bundle_digest, &*record.staged_path);
    let staged = crate::bundle::staged_bundle(Path::new(staged_path))?;
    if !crate::bundle::bundle_digests_equal(&staged.manifest_digest, digest) {
        return Err(AgentError::BundleIntegrity {
            path: "manifest.json".into(),
            reason: format!(
                "`{id}` is not started: the bundle staged at `{staged_path}` is `{}`, not the `{digest}` that was verified",
                staged.manifest_digest
            ),
        });
    }
    // No report for the backend: no profile is known to be installed.
    let report = deploy.probes.get(&record.backend_hint);
    let probed = report
        .into_iter()
        .flat_map(|report| &report.runner_profiles);
    let installed: Vec<RunnerProfile> = probed.map(|probe| probe.profile.clone()).collect();
    let descriptors = DescriptorStore::new(store.state_dir());
    let underived = |reason: String| unusable(format!("of `{id}` is not derived: {reason}"));
    let guard = Held::new(worker);
    let derived = store.update(|state| {
        if state.candidate.as_ref() != Some(record) {
            return Err(underived("the candidate record is not its deploy's".into()));
        }
        let generation = state.allocate_generation(1);
        let generation = generation.map_err(|e| underived(e.to_string()))?;
        let inputs = DescriptorInputs {
            deployment_id: id,
            generation,
            admission_mode: AdmissionMode::Production,
            staged_path,
            quota: MemberQuota::default(),
            runtime_version: tensorplate_protocol::version(),
            runner_profiles: &installed,
            acceptance_profile_digest: None,
        };
        let derived = DeploymentDescriptor::derive(&staged, &inputs);
        let derived = derived.map_err(|e: DeploymentDescriptorError| {
            let message = format!("of `{id}` is not derived: {e}");
            let code = ProtocolError::from(e).code;
            AgentError::DeploymentDescriptor { code, message }
        })?;
        let (generation, descriptor_digest, configuration_digest) = naming(&derived);
        let in_flight = state.in_flight_transaction.as_mut().filter(|tx| {
            tx.transaction_id == deploy.transaction_id
                && tx.kind == TransactionKind::Deploy
                && tx.deployment_id == *id
        });
        let transaction = in_flight.ok_or_else(|| {
            let transaction_id = deploy.transaction_id;
            underived(format!(
                "deploy transaction `{transaction_id}` is not in flight"
            ))
        })?;
        state.candidate = Some(DeploymentRecord {
            generation,
            descriptor_digest: descriptor_digest.clone(),
            configuration_digest: configuration_digest.clone(),
            ..record.clone()
        });
        transaction.generation = generation;
        transaction.descriptor_digest = descriptor_digest;
        transaction.configuration_digest = configuration_digest;
        descriptors.write(&derived)?;
        Ok(derived)
    })?;
    Ok(InHand::naming(worker, reference, &derived, guard))
}

/// The reference of `record` before any descriptor is named on it.
fn reference_of(record: &DeploymentRecord, artifact_relative_path: Option<String>) -> CandidateRef {
    CandidateRef {
        deployment_id: record.deployment_id.clone(),
        staged_path: record.staged_path.clone(),
        bundle_digest: record.bundle_digest.clone(),
        generation: None,
        descriptor_digest: None,
        configuration_digest: None,
        backend_hint: record.backend_hint.clone(),
        model_class: record.model_class.clone(),
        bundle_name: Some(record.bundle_name.clone()),
        bundle_version: Some(record.bundle_version.clone()),
        artifact_relative_path,
    }
}

/// A hold on a worker control, released when dropped.
struct Held<'w>(&'w dyn WorkerControl);

impl<'w> Held<'w> {
    fn new(worker: &'w dyn WorkerControl) -> Self {
        worker.hold();
        Self(worker)
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// A reference on its way to `prepare`. One that names a descriptor was
/// given its generation under a hold taken before that generation was: the
/// worker control starts none by itself until `prepare` has returned, so
/// every generation it holds by then is older than the one handed to it.
pub(crate) struct InHand<'w> {
    worker: &'w dyn WorkerControl,
    reference: CandidateRef,
    held: Option<Held<'w>>,
}

impl<'w> InHand<'w> {
    /// A reference that names no descriptor: the worker control takes its
    /// generation itself, inside `prepare`.
    fn as_recorded(worker: &'w dyn WorkerControl, reference: CandidateRef) -> Self {
        Self {
            worker,
            reference,
            held: None,
        }
    }

    fn naming(
        worker: &'w dyn WorkerControl,
        reference: CandidateRef,
        descriptor: &DeploymentDescriptor,
        held: Held<'w>,
    ) -> Self {
        let (generation, descriptor_digest, configuration_digest) = naming(descriptor);
        Self {
            worker,
            reference: CandidateRef {
                generation,
                descriptor_digest,
                configuration_digest,
                ..reference
            },
            held: Some(held),
        }
    }

    #[cfg(test)]
    pub(crate) fn reference(&self) -> &CandidateRef {
        &self.reference
    }

    /// Ask the worker control to prepare the reference, and return it with
    /// the answer. The hold ends here: from its `prepare` on the worker
    /// control holds the member for the candidate itself.
    pub(crate) fn prepare(
        self,
        transaction_id: &str,
        timeout: Duration,
    ) -> (CandidateRef, AgentResult<()>) {
        let Self {
            worker,
            reference,
            held,
        } = self;
        let prepared = worker.prepare(transaction_id, &reference, timeout);
        drop(held);
        (reference, prepared)
    }
}

/// The restart stamp of an agent that keeps descriptor documents: in one
/// state update it re-stamps the descriptor `active` names for the new
/// generation, writes its document and names it, and returns the recipe
/// naming it. A recipe that names no descriptor is returned as it is.
#[cfg(unix)]
#[must_use]
pub fn restart_stamp(
    store: std::sync::Arc<crate::state::StateStore>,
    descriptors: std::sync::Arc<DescriptorStore>,
) -> crate::registry::RestartStamp {
    std::sync::Arc::new(move |restamp| {
        let recipe = restamp.recipe;
        let Some(identity) = identity_of(recipe)? else {
            return Ok(recipe.clone());
        };
        let path = descriptors.path(&recipe.deployment_id, identity.generation)?;
        let unstamped = |reason: String| refused(&path, format!("is not re-stamped: {reason}"));
        if restamp.stopped_generation != identity.generation {
            return Err(unstamped(format!(
                "the restart replaces generation {}, not the generation {} it describes",
                restamp.stopped_generation, identity.generation
            )));
        }
        let next = store.update(|state| {
            let active = state
                .active
                .as_mut()
                .filter(|active| {
                    active.deployment_id == recipe.deployment_id
                        && active.descriptor_identity() == Ok(Some(identity))
                })
                .ok_or_else(|| unstamped("the active deployment record does not name it".into()))?;
            // The registry has compared the recipe's digest with the staged manifest's.
            let (digest, staged_path) = (&recipe.bundle_digest, &recipe.staged_path);
            let (id, generation) = (&recipe.deployment_id, restamp.generation);
            let next = descriptors.restamped(id, identity, digest, staged_path, generation)?;
            (
                active.generation,
                active.descriptor_digest,
                active.configuration_digest,
            ) = naming(&next);
            descriptors.write(&next)?;
            Ok(next)
        })?;
        let (generation, descriptor_digest, configuration_digest) = naming(&next);
        Ok(CandidateRef {
            generation,
            descriptor_digest,
            configuration_digest,
            ..recipe.clone()
        })
    })
}

#[cfg(test)]
pub(crate) mod tests;
