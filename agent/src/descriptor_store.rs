// SPDX-License-Identifier: Apache-2.0

//! The deployment descriptor documents the agent keeps beside its state:
//! one file per deployment generation, named by the records that carry its
//! identity. A document is written inside the state update that names it,
//! so one that cannot be written fails the update, and a crash between the
//! two writes leaves a document no record names.

#![allow(dead_code, unused_imports)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use tensorplate_protocol::agent_state::AgentState;
use tensorplate_protocol::worker_control::CandidateRef;
use tensorplate_protocol::{DeploymentDescriptor, DescriptorIdentity, ErrorCode};

use crate::error::{AgentError, AgentResult};

/// Appended to a document's name while it is being written.
const UNFINISHED: &str = ".tmp";

fn unbuilt<T>() -> AgentResult<T> {
    Err(AgentError::Internal(
        "the descriptor store is not built yet".into(),
    ))
}

fn file_name(_deployment_id: &str, _generation: u64) -> Option<String> {
    None
}

fn is_document(_name: &str) -> bool {
    false
}

/// The descriptor identity `candidate` carries, whole or not at all.
///
/// # Errors
///
/// [`AgentError::DeploymentDescriptor`] when it carries only part of one.
pub fn identity_of(_candidate: &CandidateRef) -> AgentResult<Option<DescriptorIdentity<'_>>> {
    unbuilt()
}

/// The descriptor `candidate` names, read from `store` and verified; none
/// when it names none. `generation` is the one being started.
///
/// # Errors
///
/// [`AgentError::DeploymentDescriptor`] when the candidate names another
/// generation's descriptor, there is no store, or the read is refused.
pub fn resolve(
    _store: Option<&DescriptorStore>,
    _candidate: &CandidateRef,
    _generation: u64,
) -> AgentResult<Option<DeploymentDescriptor>> {
    unbuilt()
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

    /// Write `descriptor` as the document of its generation.
    ///
    /// # Errors
    ///
    /// Not built yet.
    pub fn write(&self, _descriptor: &DeploymentDescriptor) -> AgentResult<()> {
        unbuilt()
    }

    /// Read the document `identity` names for `deployment_id`.
    ///
    /// # Errors
    ///
    /// Not built yet.
    pub fn read(
        &self,
        _deployment_id: &str,
        _identity: DescriptorIdentity<'_>,
    ) -> AgentResult<DeploymentDescriptor> {
        unbuilt()
    }

    /// Remove every document no record of `state` names.
    ///
    /// # Errors
    ///
    /// Not built yet.
    pub fn prune(&self, _state: &AgentState) -> AgentResult<usize> {
        unbuilt()
    }
}

/// The restart stamp of an agent that keeps descriptor documents. Not built
/// yet: it returns the recipe as it is.
#[cfg(unix)]
#[must_use]
pub fn restart_stamp(
    _store: std::sync::Arc<crate::state::StateStore>,
    _descriptors: std::sync::Arc<DescriptorStore>,
) -> crate::registry::RestartStamp {
    std::sync::Arc::new(|restamp| Ok(restamp.recipe.clone()))
}

#[cfg(test)]
pub(crate) mod tests;
