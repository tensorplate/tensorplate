// SPDX-License-Identifier: Apache-2.0

//! The deployment descriptor documents the agent keeps beside its state:
//! one file per deployment generation, named by the records that carry its
//! identity. A document is written inside the state update that names it,
//! so one that cannot be written fails the update, and a crash between the
//! two writes leaves a document no record names.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};

use tensorplate_protocol::agent_control::is_valid_deployment_id;
use tensorplate_protocol::agent_state::AgentState;
use tensorplate_protocol::worker_control::CandidateRef;
use tensorplate_protocol::{DeploymentDescriptor, DescriptorIdentity, ErrorCode};

use crate::error::{AgentError, AgentResult};

const PREFIX: &str = "descriptor-";
const SUFFIX: &str = ".json";
/// Appended to a document's name while it is being written.
const UNFINISHED: &str = ".tmp";

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
/// generation's descriptor, there is no store, or the read is refused.
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
    store.read(id, identity).map(Some)
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
    /// Called inside the state update that names the descriptor.
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
        let failed = |e: io::Error| io::Error::new(e.kind(), format!("{}: {e}", path.display()));
        Ok(write().map_err(failed)?)
    }

    /// Read the document `identity` names for `deployment_id` and verify
    /// that it is that descriptor.
    ///
    /// # Errors
    ///
    /// [`AgentError::DeploymentDescriptor`] when the document is missing,
    /// does not verify, or is not the one named; [`AgentError::Io`] when
    /// it cannot be read.
    pub fn read(
        &self,
        deployment_id: &str,
        identity: DescriptorIdentity<'_>,
    ) -> AgentResult<DeploymentDescriptor> {
        let path = self.path(deployment_id, identity.generation)?;
        let text = fs::read_to_string(&path).map_err(|e| match e.kind() {
            ErrorKind::NotFound => refused(&path, "is missing"),
            kind => io::Error::new(kind, format!("{}: {e}", path.display())).into(),
        })?;
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
        let kept = named_by(state)?;
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
            let current = descriptors.read(&recipe.deployment_id, identity)?;
            // The registry has compared the recipe's digest with the staged manifest's.
            let described = &current.configuration.bundle.bundle_digest;
            if !crate::bundle::bundle_digests_equal(described, &recipe.bundle_digest) {
                return Err(unstamped(format!(
                    "it describes bundle `{described}`, and `{}` is staged at `{}`",
                    recipe.bundle_digest, recipe.staged_path
                )));
            }
            let next = current
                .restamp(restamp.generation)
                .map_err(|e| unstamped(e.to_string()))?;
            active.generation = Some(next.generation);
            active.descriptor_digest = Some(next.descriptor_digest.clone());
            active.configuration_digest = Some(next.configuration_digest.clone());
            descriptors.write(&next)?;
            Ok(next)
        })?;
        Ok(CandidateRef {
            generation: Some(next.generation),
            descriptor_digest: Some(next.descriptor_digest),
            configuration_digest: Some(next.configuration_digest),
            ..recipe.clone()
        })
    })
}

#[cfg(test)]
pub(crate) mod tests;
