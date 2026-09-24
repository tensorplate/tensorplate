// SPDX-License-Identifier: Apache-2.0
//
// The resident set: every deployment the agent keeps loaded at once, as one
// durable, versioned record inside the agent's state file
// (`protocol/schemas/agent_state.json`, state `schema_version` "0.2").
//
// A set revision names its members in committed order. Each member is one
// deployment at one generation: a durable, monotonic identifier the agent
// allocates from the state file's `next_generation` counter and never hands
// out twice. The committed endpoint map lists where each serving member
// listens. This module models the shape and its invariants, not the I/O.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::agent_control::{is_valid_deployment_id, looks_like_digest};
use crate::json_numbers::MAX_SAFE_BYTES;
use crate::member_quota::{MemberQuota, MemberQuotaError};
use crate::serde_shape::{
    deserialize_map_only, deserialize_some_map_only, deserialize_vec_map_only,
};

/// Largest counter value (generation, revision) the state file carries:
/// 2^53 - 1, so every JSON reader represents it exactly.
pub const MAX_STATE_COUNTER: u64 = MAX_SAFE_BYTES;

/// How a member was admitted.
///
/// `qualification` is the operator-selected mode for measuring a proposed
/// set with an explicit test count, which is not an ordinary serving claim.
/// Recording it with the member keeps that distinction across an agent
/// restart.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionMode {
    Production,
    Qualification,
}

/// Durable lifecycle state of a committed member.
///
/// A `quarantined` member stays in the set but is not to be launched, and
/// has no endpoint, until an explicit recovery returns it to service or an
/// undeploy removes it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberState {
    Serving,
    Quarantined,
}

/// A generation the agent keeps for rollback: the member's previous
/// committed generation, or the member a size-one `replace` displaced.
///
/// It carries everything needed to launch it again from its staged root.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedGeneration {
    pub deployment_id: String,
    pub generation: u64,
    pub bundle_digest: String,
    pub descriptor_digest: String,
    pub configuration_digest: String,
    pub admission_mode: AdmissionMode,
    #[serde(deserialize_with = "deserialize_map_only")]
    pub quota: MemberQuota,
    pub staged_path: String,
    pub bundle_name: String,
    pub bundle_version: String,
    pub backend_hint: String,
    pub model_class: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_shape::deserialize_some"
    )]
    pub promoted_monotonic_ns: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "deserialize_map_only"
    )]
    pub labels: BTreeMap<String, String>,
}

/// One committed member of the resident set.
///
/// The fields beside the identity (`deployment_id`, `generation`, the three
/// digests, `quota`) are the ones the singleton `active` record carries, so
/// a member holds everything status, supervision and rollback read from
/// that record. A singleton record whose id or staged path breaks the
/// member rules cannot become a member.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResidentMember {
    pub deployment_id: String,
    pub generation: u64,
    pub bundle_digest: String,
    pub descriptor_digest: String,
    pub configuration_digest: String,
    pub admission_mode: AdmissionMode,
    #[serde(deserialize_with = "deserialize_map_only")]
    pub quota: MemberQuota,
    pub state: MemberState,
    pub staged_path: String,
    pub bundle_name: String,
    pub bundle_version: String,
    pub backend_hint: String,
    pub model_class: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_shape::deserialize_some"
    )]
    pub promoted_monotonic_ns: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "deserialize_map_only"
    )]
    pub labels: BTreeMap<String, String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some_map_only"
    )]
    pub previous: Option<RetainedGeneration>,
}

impl ResidentMember {
    /// The member's committed generation in its retained form, for use as
    /// the `previous` of the generation that replaces it.
    #[must_use]
    pub fn to_retained(&self) -> RetainedGeneration {
        RetainedGeneration {
            deployment_id: self.deployment_id.clone(),
            generation: self.generation,
            bundle_digest: self.bundle_digest.clone(),
            descriptor_digest: self.descriptor_digest.clone(),
            configuration_digest: self.configuration_digest.clone(),
            admission_mode: self.admission_mode,
            quota: self.quota,
            staged_path: self.staged_path.clone(),
            bundle_name: self.bundle_name.clone(),
            bundle_version: self.bundle_version.clone(),
            backend_hint: self.backend_hint.clone(),
            model_class: self.model_class.clone(),
            promoted_monotonic_ns: self.promoted_monotonic_ns,
            labels: self.labels.clone(),
        }
    }
}

/// Where one serving member listens in the committed revision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointEntry {
    pub deployment_id: String,
    pub generation: u64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_shape::deserialize_some"
    )]
    pub unary_endpoint: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_shape::deserialize_some"
    )]
    pub stream_endpoint: Option<String>,
}

/// One committed revision of the resident set.
///
/// `set_id` is assigned when the set is first committed and never changes;
/// `revision` increases with every committed change. `members` is in
/// committed order. `endpoint_map` has exactly one entry per serving member,
/// in member order, at that member's generation; quarantined members have
/// none.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResidentSet {
    pub set_id: String,
    pub revision: u64,
    #[serde(deserialize_with = "deserialize_vec_map_only")]
    pub members: Vec<ResidentMember>,
    #[serde(deserialize_with = "deserialize_vec_map_only")]
    pub endpoint_map: Vec<EndpointEntry>,
}

impl ResidentSet {
    /// Every generation the set refers to: members and their retained
    /// previous generations.
    pub fn generations(&self) -> impl Iterator<Item = u64> + '_ {
        self.members.iter().flat_map(|member| {
            std::iter::once(member.generation)
                .chain(member.previous.as_ref().map(|previous| previous.generation))
        })
    }

    /// Check the set's invariants against the state file's generation
    /// counter.
    ///
    /// # Errors
    ///
    /// Returns [`ResidentSetError`] naming the first violated rule.
    pub fn validate(&self, next_generation: u64) -> Result<(), ResidentSetError> {
        if !is_valid_deployment_id(&self.set_id) {
            return Err(ResidentSetError::InvalidSetId);
        }
        if !(1..=MAX_STATE_COUNTER).contains(&self.revision) {
            return Err(ResidentSetError::RevisionOutOfRange(self.revision));
        }
        let mut ids = BTreeSet::new();
        let mut generations = BTreeSet::new();
        for member in &self.members {
            validate_generation_record(
                &RecordView::member(member),
                next_generation,
                &mut generations,
            )?;
            if !ids.insert(member.deployment_id.as_str()) {
                return Err(ResidentSetError::DuplicateMember(
                    member.deployment_id.clone(),
                ));
            }
            if let Some(previous) = member.previous.as_ref() {
                validate_generation_record(
                    &RecordView::retained(previous),
                    next_generation,
                    &mut generations,
                )?;
                if previous.generation >= member.generation {
                    return Err(ResidentSetError::PreviousNotOlder {
                        deployment_id: member.deployment_id.clone(),
                        previous: previous.generation,
                        generation: member.generation,
                    });
                }
            }
        }
        self.validate_endpoint_map()
    }

    fn validate_endpoint_map(&self) -> Result<(), ResidentSetError> {
        let mut serving = self
            .members
            .iter()
            .filter(|member| member.state == MemberState::Serving);
        for entry in &self.endpoint_map {
            let Some(member) = serving.next() else {
                return Err(ResidentSetError::EndpointWithoutServingMember(
                    entry.deployment_id.clone(),
                ));
            };
            if entry.deployment_id != member.deployment_id || entry.generation != member.generation
            {
                return Err(ResidentSetError::EndpointMismatch {
                    expected: format!("{}@{}", member.deployment_id, member.generation),
                    found: format!("{}@{}", entry.deployment_id, entry.generation),
                });
            }
            let endpoints = [
                entry.unary_endpoint.as_ref(),
                entry.stream_endpoint.as_ref(),
            ];
            if endpoints.iter().all(Option::is_none) {
                return Err(ResidentSetError::EndpointMissing(
                    entry.deployment_id.clone(),
                ));
            }
            if endpoints
                .iter()
                .flatten()
                .any(|endpoint| endpoint.is_empty())
            {
                return Err(ResidentSetError::EndpointEmpty(entry.deployment_id.clone()));
            }
        }
        if let Some(member) = serving.next() {
            return Err(ResidentSetError::ServingMemberWithoutEndpoint(
                member.deployment_id.clone(),
            ));
        }
        Ok(())
    }
}

/// Borrowed view of the fields a member and a retained generation share,
/// so both are checked by one function.
struct RecordView<'a> {
    deployment_id: &'a str,
    generation: u64,
    digests: [(&'static str, &'a str); 3],
    quota: &'a MemberQuota,
    staged_path: &'a str,
}

impl<'a> RecordView<'a> {
    fn member(member: &'a ResidentMember) -> Self {
        Self {
            deployment_id: &member.deployment_id,
            generation: member.generation,
            digests: [
                ("bundle_digest", &member.bundle_digest),
                ("descriptor_digest", &member.descriptor_digest),
                ("configuration_digest", &member.configuration_digest),
            ],
            quota: &member.quota,
            staged_path: &member.staged_path,
        }
    }

    fn retained(retained: &'a RetainedGeneration) -> Self {
        Self {
            deployment_id: &retained.deployment_id,
            generation: retained.generation,
            digests: [
                ("bundle_digest", &retained.bundle_digest),
                ("descriptor_digest", &retained.descriptor_digest),
                ("configuration_digest", &retained.configuration_digest),
            ],
            quota: &retained.quota,
            staged_path: &retained.staged_path,
        }
    }
}

fn validate_generation_record(
    record: &RecordView<'_>,
    next_generation: u64,
    seen_generations: &mut BTreeSet<u64>,
) -> Result<(), ResidentSetError> {
    if !is_valid_deployment_id(record.deployment_id) {
        return Err(ResidentSetError::InvalidDeploymentId(
            record.deployment_id.to_owned(),
        ));
    }
    if record.generation == 0 || record.generation >= next_generation {
        return Err(ResidentSetError::GenerationNotAllocated {
            deployment_id: record.deployment_id.to_owned(),
            generation: record.generation,
            next_generation,
        });
    }
    if !seen_generations.insert(record.generation) {
        return Err(ResidentSetError::DuplicateGeneration(record.generation));
    }
    for (field, digest) in record.digests {
        if !looks_like_digest(digest) {
            return Err(ResidentSetError::InvalidDigest {
                deployment_id: record.deployment_id.to_owned(),
                field,
            });
        }
    }
    if !record.staged_path.starts_with('/') {
        return Err(ResidentSetError::StagedPathNotAbsolute(
            record.deployment_id.to_owned(),
        ));
    }
    record
        .quota
        .validate()
        .map_err(|source| ResidentSetError::Quota {
            deployment_id: record.deployment_id.to_owned(),
            source,
        })
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum ResidentSetError {
    #[error("resident_set.set_id must be one filesystem-safe path segment of 1-128 bytes")]
    InvalidSetId,
    #[error("resident_set.revision {0} is outside [1, 2^53)")]
    RevisionOutOfRange(u64),
    #[error(
        "resident set member id `{0}` must be one filesystem-safe path segment of 1-128 bytes"
    )]
    InvalidDeploymentId(String),
    #[error("resident set member `{0}` appears more than once")]
    DuplicateMember(String),
    #[error("generation {0} appears more than once in the resident set")]
    DuplicateGeneration(u64),
    #[error(
        "`{deployment_id}` generation {generation} was never allocated (next_generation is {next_generation})"
    )]
    GenerationNotAllocated {
        deployment_id: String,
        generation: u64,
        next_generation: u64,
    },
    #[error("`{deployment_id}` {field} must follow the `algo:hex` form")]
    InvalidDigest {
        deployment_id: String,
        field: &'static str,
    },
    #[error("`{0}` staged_path must be absolute")]
    StagedPathNotAbsolute(String),
    #[error("`{deployment_id}` {source}")]
    Quota {
        deployment_id: String,
        source: MemberQuotaError,
    },
    #[error(
        "`{deployment_id}` previous generation {previous} is not older than generation {generation}"
    )]
    PreviousNotOlder {
        deployment_id: String,
        previous: u64,
        generation: u64,
    },
    #[error("endpoint_map entry `{0}` has no serving member left to describe")]
    EndpointWithoutServingMember(String),
    #[error("endpoint_map entry {found} does not match the next serving member {expected}")]
    EndpointMismatch { expected: String, found: String },
    #[error("endpoint_map entry `{0}` names neither a unary nor a stream endpoint")]
    EndpointMissing(String),
    #[error("endpoint_map entry `{0}` has an empty endpoint")]
    EndpointEmpty(String),
    #[error("serving member `{0}` has no endpoint_map entry")]
    ServingMemberWithoutEndpoint(String),
}
