// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F02: Rust mirror of `protocol/schemas/agent_state.json`.
//
// This is the durable desired-state record `tensorplate-agent` writes to
// disk after every state transition. The on-disk file is rewritten
// atomically by the agent's state store; this module only models the
// shape, not the I/O.
//
// The state file has its own version track. `schema_version` "0.1" is the
// singleton layout every agent through 0.2.x reads; "0.2" adds the durable
// generation counter and the resident set. Both are decoded by
// [`decode_agent_state`], not by the protocol-wide
// [`crate::decode_with_version_check`], whose `SCHEMA_VERSION` stays "0.1"
// for every other payload and does not move these versions. A writer stamps
// the oldest version whose readers decode the state without loss
// ([`AgentState::required_schema_version`]), so an agent that never
// allocates a generation keeps writing files an older agent reads, and one
// that has is refused by an older agent with its typed unsupported-version
// error instead of being misread.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::deploy_transaction::DeployState;
use crate::error::ErrorCode;
use crate::resident_set::{ResidentSet, ResidentSetError, MAX_STATE_COUNTER};
use crate::serde_shape::{deserialize_some, deserialize_some_map_only};
use crate::{DecodeError, ValidatePayload};

/// State version of the singleton layout: the six legacy slots and the
/// v0.2.1 error, phase and transaction-kind vocabulary. Every agent through
/// 0.2.x reads exactly this. A literal, not [`SCHEMA_VERSION`]: moving the
/// protocol version must not move what those agents read.
pub const AGENT_STATE_SCHEMA_VERSION_LEGACY: &str = "0.1";

/// State version that adds `next_generation` and `resident_set`.
pub const AGENT_STATE_SCHEMA_VERSION_RESIDENT_SET: &str = "0.2";

/// Every state version [`decode_agent_state`] accepts.
pub const AGENT_STATE_SCHEMA_VERSIONS: [&str; 2] = [
    AGENT_STATE_SCHEMA_VERSION_LEGACY,
    AGENT_STATE_SCHEMA_VERSION_RESIDENT_SET,
];

/// [`AGENT_STATE_SCHEMA_VERSIONS`] as the decoder's errors name them.
const AGENT_STATE_SCHEMA_VERSIONS_DISPLAY: &str = "0.1 or 0.2";

/// Top-level keys of a state file. A "0.2" file with any other top-level
/// key, or any other key in one of the records below, is refused: an older
/// 0.2 reader would otherwise drop a field it does not know on its next
/// write. "0.1" files keep the lenient reading every earlier agent applied.
pub const AGENT_STATE_ROOT_KEYS: [&str; 10] = [
    "schema_version",
    "store_version",
    "active",
    "previous_active",
    "candidate",
    "in_flight_transaction",
    "last_error",
    "quarantined",
    "next_generation",
    "resident_set",
];

/// Keys of the records a "0.2" file shares with the singleton layout. The
/// resident-set records refuse unknown keys through serde; these records
/// keep serde's leniency for "0.1" files, so [`decode_agent_state`] checks
/// them by name in a "0.2" file.
pub const DEPLOYMENT_RECORD_KEYS: [&str; 9] = [
    "deployment_id",
    "bundle_digest",
    "bundle_name",
    "bundle_version",
    "backend_hint",
    "model_class",
    "staged_path",
    "promoted_monotonic_ns",
    "labels",
];

/// See [`DEPLOYMENT_RECORD_KEYS`].
pub const TRANSACTION_RECORD_KEYS: [&str; 10] = [
    "transaction_id",
    "deployment_id",
    "phase",
    "kind",
    "bundle_digest",
    "bundle_path",
    "correlation_id",
    "started_monotonic_ns",
    "last_transition_monotonic_ns",
    "failure",
];

/// See [`DEPLOYMENT_RECORD_KEYS`].
pub const ERROR_RECORD_KEYS: [&str; 4] = ["code", "message", "recoverable", "context"];

/// See [`DEPLOYMENT_RECORD_KEYS`].
pub const QUARANTINE_RECORD_KEYS: [&str; 6] = [
    "transaction_id",
    "deployment_id",
    "bundle_digest",
    "phase",
    "error",
    "quarantined_monotonic_ns",
];

/// Stored kind of in-flight transaction. Both deploy and rollback walk
/// the same phase enum but report distinct kinds so recovery can tell
/// them apart.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionKind {
    Deploy,
    Rollback,
}

/// One persisted deployment record (active, previous active, or candidate).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeploymentRecord {
    pub deployment_id: String,
    pub bundle_digest: String,
    pub bundle_name: String,
    pub bundle_version: String,
    pub backend_hint: String,
    pub model_class: String,
    pub staged_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted_monotonic_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

/// Persistable typed error attached to a failed/quarantined transaction.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ErrorRecord {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub recoverable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

impl ErrorRecord {
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            recoverable: false,
            context: None,
        }
    }

    #[must_use]
    pub fn recoverable(mut self, recoverable: bool) -> Self {
        self.recoverable = recoverable;
        self
    }

    #[must_use]
    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }
}

/// One in-flight transaction journal entry.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TransactionRecord {
    pub transaction_id: String,
    pub deployment_id: String,
    pub phase: DeployState,
    pub kind: TransactionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_monotonic_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition_monotonic_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<ErrorRecord>,
}

/// One quarantine entry persisted outside the active/previous/candidate slots.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QuarantineRecord {
    pub transaction_id: String,
    pub deployment_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_digest: Option<String>,
    pub phase: DeployState,
    pub error: ErrorRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quarantined_monotonic_ns: Option<u64>,
}

/// Root durable-state record.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentState {
    pub schema_version: String,
    pub store_version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<DeploymentRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_active: Option<DeploymentRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<DeploymentRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_flight_transaction: Option<TransactionRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<ErrorRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quarantined: Vec<QuarantineRecord>,
    /// The next deployment generation the agent will hand out. Present from
    /// the first allocation on, never removed and never lowered; every
    /// generation in `resident_set` is below it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub next_generation: Option<u64>,
    /// The committed resident set. When present, the singleton `active`,
    /// `previous_active` and `candidate` slots are absent: the set is the
    /// only record of what is deployed.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some_map_only"
    )]
    pub resident_set: Option<ResidentSet>,
}

impl AgentState {
    /// Fresh state with `store_version = 1` and the singleton layout's
    /// version.
    #[must_use]
    pub fn fresh() -> Self {
        Self {
            schema_version: AGENT_STATE_SCHEMA_VERSION_LEGACY.to_string(),
            store_version: 1,
            ..Self::default()
        }
    }

    /// The oldest state version whose readers decode this state without
    /// loss: "0.2" once the state carries the generation counter or a
    /// resident set, "0.1" otherwise. A value outside the v0.2.1 error,
    /// phase or transaction-kind vocabulary also rules out "0.1"; no such
    /// value exists today, and one added after a release has written "0.2"
    /// takes a new state version (see the classification functions below).
    #[must_use]
    pub fn required_schema_version(&self) -> &'static str {
        if self.next_generation.is_some()
            || self.resident_set.is_some()
            || !self.uses_only_legacy_vocabulary()
        {
            AGENT_STATE_SCHEMA_VERSION_RESIDENT_SET
        } else {
            AGENT_STATE_SCHEMA_VERSION_LEGACY
        }
    }

    fn uses_only_legacy_vocabulary(&self) -> bool {
        let error_ok = |e: &ErrorRecord| error_code_is_legacy(e.code);
        self.last_error.as_ref().map_or(true, error_ok)
            && self.in_flight_transaction.as_ref().map_or(true, |tx| {
                deploy_state_is_legacy(tx.phase)
                    && transaction_kind_is_legacy(tx.kind)
                    && tx.failure.as_ref().map_or(true, error_ok)
            })
            && self
                .quarantined
                .iter()
                .all(|q| deploy_state_is_legacy(q.phase) && error_ok(&q.error))
    }

    /// Hand out the next deployment generation and advance the counter.
    ///
    /// Within one state file the counter never returns a value twice. The
    /// result is also at least `floor`, so a caller that knows of
    /// generations recorded outside this file (staged roots left by an
    /// earlier state directory) keeps them from being issued again. Call it
    /// inside the state store's update closure: the counter then advances
    /// in the same atomic write that records what the generation is used
    /// for, and a generation is never returned for a write that did not
    /// commit.
    ///
    /// # Errors
    ///
    /// Returns [`AgentStateError::GenerationCounterExhausted`] when the
    /// counter would pass 2^53 - 1.
    pub fn allocate_generation(&mut self, floor: u64) -> Result<u64, AgentStateError> {
        let generation = self.next_generation.unwrap_or(1).max(floor).max(1);
        if generation >= MAX_STATE_COUNTER {
            return Err(AgentStateError::GenerationCounterExhausted);
        }
        self.next_generation = Some(generation + 1);
        Ok(generation)
    }

    /// Check that `next` may replace `self` as the durable state: the
    /// generation counter is never removed or lowered; a resident set is
    /// never removed, keeps its `set_id`, advances its `revision` whenever
    /// its members or endpoints change, and takes on no generation below
    /// the newest one it already names.
    ///
    /// # Errors
    ///
    /// Returns the [`AgentStateError`] naming the violated rule.
    pub fn validate_successor(&self, next: &AgentState) -> Result<(), AgentStateError> {
        if let Some(counter) = self.next_generation {
            match next.next_generation {
                None => return Err(AgentStateError::GenerationCounterRemoved),
                Some(n) if n < counter => {
                    return Err(AgentStateError::GenerationCounterLowered {
                        from: counter,
                        to: n,
                    })
                }
                Some(_) => {}
            }
        }
        if let Some(set) = self.resident_set.as_ref() {
            let Some(next_set) = next.resident_set.as_ref() else {
                return Err(AgentStateError::ResidentSetRemoved);
            };
            if next_set.set_id != set.set_id {
                return Err(AgentStateError::SetIdChanged {
                    from: set.set_id.clone(),
                    to: next_set.set_id.clone(),
                });
            }
            let changed =
                next_set.members != set.members || next_set.endpoint_map != set.endpoint_map;
            if next_set.revision < set.revision || (changed && next_set.revision == set.revision) {
                return Err(AgentStateError::RevisionNotAdvanced {
                    from: set.revision,
                    to: next_set.revision,
                });
            }
            // Transactions are serialized and generations are allocated in
            // increasing order, so a generation that joins the set was
            // allocated after every generation the set already names. One
            // below them is a retired generation being issued again.
            let named: BTreeSet<u64> = set.generations().collect();
            if let Some(&newest) = named.last() {
                if let Some(reissued) = next_set
                    .generations()
                    .find(|g| !named.contains(g) && *g < newest)
                {
                    return Err(AgentStateError::GenerationReissued {
                        generation: reissued,
                        newest,
                    });
                }
            }
        }
        Ok(())
    }

    fn validate_state(&self) -> Result<(), AgentStateError> {
        if self.store_version == 0 {
            return Err(AgentStateError::InvalidStoreVersion);
        }
        if !AGENT_STATE_SCHEMA_VERSIONS.contains(&self.schema_version.as_str()) {
            return Err(AgentStateError::UnsupportedStateVersion(
                self.schema_version.clone(),
            ));
        }
        if self.schema_version == AGENT_STATE_SCHEMA_VERSION_LEGACY
            && self.required_schema_version() != AGENT_STATE_SCHEMA_VERSION_LEGACY
        {
            return Err(AgentStateError::LegacyVersionCarriesNewContent);
        }
        if let Some(counter) = self.next_generation {
            if !(1..=MAX_STATE_COUNTER).contains(&counter) {
                return Err(AgentStateError::NextGenerationOutOfRange(counter));
            }
        }
        if let Some(set) = self.resident_set.as_ref() {
            let counter = self
                .next_generation
                .ok_or(AgentStateError::ResidentSetWithoutCounter)?;
            for (slot, present) in [
                ("active", self.active.is_some()),
                ("previous_active", self.previous_active.is_some()),
                ("candidate", self.candidate.is_some()),
            ] {
                if present {
                    return Err(AgentStateError::SingletonSlotBesideResidentSet(slot));
                }
            }
            set.validate(counter)?;
        }
        Ok(())
    }
}

// The v0.2.1 vocabulary: every value an agent through 0.2.x can decode. The
// matches name each variant with no wildcard, so adding an error code, a
// phase or a transaction kind fails to compile here until it is classified.
// A new value is not "0.1" content. Until a release writes "0.2" it may join
// that version; after one has, readers of "0.2" cannot decode it either, so
// it takes a new state version, and `required_schema_version`, the schema
// and the store's write order move with it. The schema's "0.1" branch pins
// the same lists (the fixture tests keep the two in step).

fn error_code_is_legacy(code: ErrorCode) -> bool {
    match code {
        ErrorCode::ConfigInvalid
        | ErrorCode::LoadFailed
        | ErrorCode::NotReady
        | ErrorCode::ShapeMismatch
        | ErrorCode::Unsupported
        | ErrorCode::OomError
        | ErrorCode::Timeout
        | ErrorCode::InferenceFailed
        | ErrorCode::Internal => true,
    }
}

fn deploy_state_is_legacy(phase: DeployState) -> bool {
    match phase {
        DeployState::Received
        | DeployState::Verified
        | DeployState::Staged
        | DeployState::CapacityChecked
        | DeployState::Prepared
        | DeployState::Warmed
        | DeployState::Promoted
        | DeployState::Active
        | DeployState::Failed
        | DeployState::RolledBack => true,
    }
}

fn transaction_kind_is_legacy(kind: TransactionKind) -> bool {
    match kind {
        TransactionKind::Deploy | TransactionKind::Rollback => true,
    }
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum AgentStateError {
    #[error("AgentState.store_version must be >= 1")]
    InvalidStoreVersion,
    #[error(
        "unsupported state schema_version `{0}` (expected `{}`)",
        AGENT_STATE_SCHEMA_VERSIONS_DISPLAY
    )]
    UnsupportedStateVersion(String),
    #[error(
        "a state stamped schema_version 0.1 carries content an agent through 0.2.x cannot read (the generation counter, a resident set, or a newer error code, phase or transaction kind)"
    )]
    LegacyVersionCarriesNewContent,
    #[error("unknown top-level field `{0}` in a schema_version 0.2 state")]
    UnknownRootField(String),
    #[error("unknown field `{field}` in `{record}` of a schema_version 0.2 state")]
    UnknownRecordField { record: String, field: String },
    #[error("next_generation {0} is outside [1, 2^53)")]
    NextGenerationOutOfRange(u64),
    #[error("resident_set requires next_generation")]
    ResidentSetWithoutCounter,
    #[error("the singleton `{0}` slot must be absent when a resident_set is present")]
    SingletonSlotBesideResidentSet(&'static str),
    #[error("resident_set: {0}")]
    ResidentSet(#[from] ResidentSetError),
    #[error("the generation counter is exhausted")]
    GenerationCounterExhausted,
    #[error("next_generation may not be removed once allocated")]
    GenerationCounterRemoved,
    #[error("next_generation may not go backwards ({from} -> {to})")]
    GenerationCounterLowered { from: u64, to: u64 },
    #[error("resident_set may not be removed once committed")]
    ResidentSetRemoved,
    #[error("resident_set.set_id may not change (`{from}` -> `{to}`)")]
    SetIdChanged { from: String, to: String },
    #[error("resident_set.revision must advance when the set changes ({from} -> {to})")]
    RevisionNotAdvanced { from: u64, to: u64 },
    #[error(
        "generation {generation} joins the resident set below generation {newest}, which it already names"
    )]
    GenerationReissued { generation: u64, newest: u64 },
}

impl ValidatePayload for AgentState {
    fn validate_payload(self) -> Result<Self, DecodeError> {
        self.validate_state()
            .map_err(|e| DecodeError::InvalidPayload(e.to_string()))?;
        Ok(self)
    }
}

/// The first key, in the records a "0.2" file shares with the singleton
/// layout, that this reader does not know, as `(record, key)`. Values that
/// are not objects are left for the structural decode to refuse.
fn first_unknown_record_field(root: &serde_json::Value) -> Option<(String, String)> {
    fn unknown(value: &serde_json::Value, allowed: &[&str]) -> Option<String> {
        value
            .as_object()?
            .keys()
            .find(|key| !allowed.contains(&key.as_str()))
            .cloned()
    }
    let mut records: Vec<(String, &serde_json::Value, &[&str])> = Vec::new();
    for slot in ["active", "previous_active", "candidate"] {
        if let Some(record) = root.get(slot) {
            records.push((slot.to_string(), record, &DEPLOYMENT_RECORD_KEYS));
        }
    }
    if let Some(tx) = root.get("in_flight_transaction") {
        records.push(("in_flight_transaction".into(), tx, &TRANSACTION_RECORD_KEYS));
        if let Some(failure) = tx.get("failure") {
            records.push((
                "in_flight_transaction.failure".into(),
                failure,
                &ERROR_RECORD_KEYS,
            ));
        }
    }
    if let Some(error) = root.get("last_error") {
        records.push(("last_error".into(), error, &ERROR_RECORD_KEYS));
    }
    if let Some(quarantined) = root
        .get("quarantined")
        .and_then(serde_json::Value::as_array)
    {
        for (index, record) in quarantined.iter().enumerate() {
            records.push((
                format!("quarantined[{index}]"),
                record,
                &QUARANTINE_RECORD_KEYS,
            ));
            if let Some(error) = record.get("error") {
                records.push((
                    format!("quarantined[{index}].error"),
                    error,
                    &ERROR_RECORD_KEYS,
                ));
            }
        }
    }
    records
        .into_iter()
        .find_map(|(name, value, allowed)| unknown(value, allowed).map(|key| (name, key)))
}

/// Decode a durable state file.
///
/// Accepts state `schema_version` "0.1" and "0.2"; any other version is
/// [`DecodeError::UnsupportedSchemaVersion`] before the body is read. A
/// "0.2" file with an unknown field, at the top level or in any record, is
/// refused. Structural
/// decoding reads the raw text, so a key repeated inside a record is an
/// error rather than a silent last-wins.
///
/// # Errors
///
/// - [`DecodeError::Malformed`] for invalid JSON or a structural mismatch.
/// - [`DecodeError::MissingSchemaVersion`] when `schema_version` is absent
///   or not a string.
/// - [`DecodeError::UnsupportedSchemaVersion`] for any other version.
/// - [`DecodeError::InvalidPayload`] when an invariant fails.
pub fn decode_agent_state(raw: &str) -> Result<AgentState, DecodeError> {
    let value: serde_json::Value = serde_json::from_str(raw)?;
    let observed = value
        .get("schema_version")
        .and_then(serde_json::Value::as_str)
        .ok_or(DecodeError::MissingSchemaVersion)?;
    if !AGENT_STATE_SCHEMA_VERSIONS.contains(&observed) {
        return Err(DecodeError::UnsupportedSchemaVersion {
            got: observed.to_string(),
            expected: AGENT_STATE_SCHEMA_VERSIONS_DISPLAY,
        });
    }
    if observed == AGENT_STATE_SCHEMA_VERSION_RESIDENT_SET {
        if let Some(unknown) = value.as_object().and_then(|root| {
            root.keys()
                .find(|key| !AGENT_STATE_ROOT_KEYS.contains(&key.as_str()))
        }) {
            return Err(DecodeError::InvalidPayload(
                AgentStateError::UnknownRootField(unknown.clone()).to_string(),
            ));
        }
        if let Some((record, field)) = first_unknown_record_field(&value) {
            return Err(DecodeError::InvalidPayload(
                AgentStateError::UnknownRecordField { record, field }.to_string(),
            ));
        }
    }
    let parsed: AgentState = serde_json::from_str(raw)?;
    parsed.validate_payload()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]

    use std::collections::BTreeMap;

    use super::{
        decode_agent_state, AgentState, AgentStateError, DeploymentRecord, ErrorRecord,
        QuarantineRecord, TransactionKind, TransactionRecord, AGENT_STATE_ROOT_KEYS,
        AGENT_STATE_SCHEMA_VERSION_LEGACY, AGENT_STATE_SCHEMA_VERSION_RESIDENT_SET,
    };
    use crate::deploy_transaction::DeployState;
    use crate::error::ErrorCode;
    use crate::member_quota::{DomainQuotaBytes, MemberQuota};
    use crate::resident_set::{
        AdmissionMode, EndpointEntry, MemberState, ResidentMember, ResidentSet, MAX_STATE_COUNTER,
    };
    use crate::{decode_with_version_check, DecodeError};

    fn sample_deployment() -> DeploymentRecord {
        DeploymentRecord {
            deployment_id: "deploy-1".into(),
            bundle_digest: "sha256:cafe".into(),
            bundle_name: "yolov8n".into(),
            bundle_version: "1.0.0".into(),
            backend_hint: "tensorrt".into(),
            model_class: "vision".into(),
            staged_path: "/var/lib/tensorplate/staging/deploy-1".into(),
            promoted_monotonic_ns: Some(123_456_789),
            labels: BTreeMap::new(),
        }
    }

    fn sample_member(id: &str, generation: u64) -> ResidentMember {
        ResidentMember {
            deployment_id: id.into(),
            generation,
            bundle_digest: "sha256:cafe".into(),
            descriptor_digest: "sha256:d0d0".into(),
            configuration_digest: "sha256:c0c0".into(),
            admission_mode: AdmissionMode::Production,
            quota: MemberQuota {
                session_count: 1,
                domain_bytes: DomainQuotaBytes {
                    shared_pool: None,
                    guest_ram: Some(1024),
                    device_vram: Some(2048),
                },
            },
            state: MemberState::Serving,
            staged_path: format!("/var/lib/tensorplate/bundles/staging/{id}/{generation}"),
            bundle_name: format!("bundle-{id}"),
            bundle_version: "1.0.0".into(),
            backend_hint: "python_pytorch".into(),
            model_class: "speech".into(),
            promoted_monotonic_ns: None,
            labels: BTreeMap::new(),
            previous: None,
        }
    }

    fn endpoint(id: &str, generation: u64) -> EndpointEntry {
        EndpointEntry {
            deployment_id: id.into(),
            generation,
            unary_endpoint: None,
            stream_endpoint: Some(format!("127.0.0.1:{}", 18_100 + generation)),
        }
    }

    /// A valid "0.2" state with a two-member set at generations 1 and 2.
    fn set_state() -> AgentState {
        AgentState {
            schema_version: AGENT_STATE_SCHEMA_VERSION_RESIDENT_SET.into(),
            store_version: 7,
            next_generation: Some(3),
            resident_set: Some(ResidentSet {
                set_id: "resident-set-1".into(),
                revision: 2,
                members: vec![sample_member("stt", 1), sample_member("tts", 2)],
                endpoint_map: vec![endpoint("stt", 1), endpoint("tts", 2)],
            }),
            ..AgentState::default()
        }
    }

    fn round_trip(s: &AgentState) -> AgentState {
        let raw = serde_json::to_string(s).expect("serialize");
        decode_agent_state(&raw).expect("decode")
    }

    fn invalid(s: &AgentState) -> String {
        let raw = serde_json::to_string(s).expect("serialize");
        match decode_agent_state(&raw) {
            Err(DecodeError::InvalidPayload(message)) => message,
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[test]
    fn fresh_round_trips() {
        let s = AgentState::fresh();
        let back = round_trip(&s);
        assert_eq!(s, back);
        assert_eq!(back.schema_version, AGENT_STATE_SCHEMA_VERSION_LEGACY);
    }

    #[test]
    fn populated_round_trips() {
        let s = AgentState {
            schema_version: AGENT_STATE_SCHEMA_VERSION_LEGACY.to_string(),
            store_version: 5,
            active: Some(sample_deployment()),
            previous_active: Some(sample_deployment()),
            candidate: None,
            in_flight_transaction: Some(TransactionRecord {
                transaction_id: "tx-1".into(),
                deployment_id: "deploy-1".into(),
                phase: DeployState::Warmed,
                kind: TransactionKind::Deploy,
                bundle_digest: Some("sha256:cafe".into()),
                bundle_path: Some("/bundles/deploy-1".into()),
                correlation_id: Some("corr-1".into()),
                started_monotonic_ns: Some(1),
                last_transition_monotonic_ns: Some(2),
                failure: None,
            }),
            last_error: None,
            quarantined: vec![QuarantineRecord {
                transaction_id: "tx-q".into(),
                deployment_id: "deploy-bad".into(),
                bundle_digest: Some("sha256:dead".into()),
                phase: DeployState::Prepared,
                error: ErrorRecord::new(ErrorCode::OomError, "exceeded device memory"),
                quarantined_monotonic_ns: Some(42),
            }],
            next_generation: None,
            resident_set: None,
        };
        assert_eq!(
            s.required_schema_version(),
            AGENT_STATE_SCHEMA_VERSION_LEGACY
        );
        assert_eq!(round_trip(&s), s);
    }

    #[test]
    fn resident_set_state_round_trips_at_0_2() {
        let s = set_state();
        assert_eq!(
            s.required_schema_version(),
            AGENT_STATE_SCHEMA_VERSION_RESIDENT_SET
        );
        assert_eq!(round_trip(&s), s);
    }

    #[test]
    fn rejects_unknown_schema_version() {
        let raw = r#"{"schema_version":"0.3","store_version":1}"#;
        match decode_agent_state(raw).expect_err("rejected") {
            DecodeError::UnsupportedSchemaVersion { got, expected } => {
                assert_eq!(got, "0.3");
                assert_eq!(expected, "0.1 or 0.2");
            }
            other => panic!("expected UnsupportedSchemaVersion, got {other:?}"),
        }
    }

    #[test]
    fn rejects_zero_store_version() {
        let raw = format!(
            r#"{{"schema_version":"{AGENT_STATE_SCHEMA_VERSION_LEGACY}","store_version":0}}"#
        );
        let err = decode_agent_state(&raw).expect_err("rejected");
        assert!(matches!(err, DecodeError::InvalidPayload(_)));
    }

    #[test]
    fn protocol_wide_decoder_refuses_every_0_2_state() {
        // Agents through 0.2.x read the state file through the protocol-wide
        // decoder, which this crate leaves at "0.1".
        let raw = serde_json::to_string(&set_state()).expect("serialize");
        match decode_with_version_check::<AgentState>(&raw).expect_err("refused") {
            DecodeError::UnsupportedSchemaVersion { got, expected } => {
                assert_eq!(got, "0.2");
                assert_eq!(expected, "0.1");
            }
            other => panic!("expected UnsupportedSchemaVersion, got {other:?}"),
        }
    }

    #[test]
    fn allocation_advances_the_counter_and_honours_the_floor() {
        let mut s = AgentState::fresh();
        assert_eq!(s.allocate_generation(0), Ok(1));
        assert_eq!(s.allocate_generation(0), Ok(2));
        assert_eq!(s.next_generation, Some(3));
        assert_eq!(s.allocate_generation(10), Ok(10));
        assert_eq!(s.next_generation, Some(11));
        // A floor below the counter changes nothing.
        assert_eq!(s.allocate_generation(4), Ok(11));
        assert_eq!(
            s.required_schema_version(),
            AGENT_STATE_SCHEMA_VERSION_RESIDENT_SET
        );
    }

    #[test]
    fn allocation_stops_at_the_json_safe_bound() {
        let mut s = AgentState::fresh();
        assert_eq!(
            s.allocate_generation(MAX_STATE_COUNTER - 1),
            Ok(MAX_STATE_COUNTER - 1)
        );
        assert_eq!(s.next_generation, Some(MAX_STATE_COUNTER));
        assert_eq!(
            s.allocate_generation(0),
            Err(AgentStateError::GenerationCounterExhausted)
        );
        assert_eq!(s.next_generation, Some(MAX_STATE_COUNTER));
    }

    #[test]
    fn a_0_1_stamp_cannot_carry_0_2_content() {
        let mut s = AgentState::fresh();
        s.next_generation = Some(1);
        assert_eq!(
            invalid(&s),
            AgentStateError::LegacyVersionCarriesNewContent.to_string()
        );
    }

    #[test]
    fn a_0_2_stamp_may_carry_only_the_counter() {
        let mut s = AgentState::fresh();
        s.schema_version = AGENT_STATE_SCHEMA_VERSION_RESIDENT_SET.into();
        s.next_generation = Some(4);
        assert_eq!(round_trip(&s), s);
    }

    #[test]
    fn a_resident_set_needs_the_counter() {
        let mut s = set_state();
        s.next_generation = None;
        assert_eq!(
            invalid(&s),
            AgentStateError::ResidentSetWithoutCounter.to_string()
        );
    }

    #[test]
    fn singleton_slots_are_absent_beside_a_resident_set() {
        for slot in ["active", "previous_active", "candidate"] {
            let mut s = set_state();
            match slot {
                "active" => s.active = Some(sample_deployment()),
                "previous_active" => s.previous_active = Some(sample_deployment()),
                _ => s.candidate = Some(sample_deployment()),
            }
            assert_eq!(
                invalid(&s),
                AgentStateError::SingletonSlotBesideResidentSet(slot).to_string()
            );
        }
    }

    #[test]
    fn counter_bounds_are_enforced() {
        for counter in [0, MAX_STATE_COUNTER + 1] {
            let mut s = AgentState::fresh();
            s.schema_version = AGENT_STATE_SCHEMA_VERSION_RESIDENT_SET.into();
            s.next_generation = Some(counter);
            assert_eq!(
                invalid(&s),
                AgentStateError::NextGenerationOutOfRange(counter).to_string()
            );
        }
    }

    #[test]
    fn unknown_root_fields_are_refused_at_0_2_and_ignored_at_0_1() {
        let raw =
            r#"{"schema_version":"0.2","store_version":1,"next_generation":1,"later_field":true}"#;
        match decode_agent_state(raw).expect_err("refused") {
            DecodeError::InvalidPayload(message) => assert_eq!(
                message,
                AgentStateError::UnknownRootField("later_field".into()).to_string()
            ),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
        let legacy = r#"{"schema_version":"0.1","store_version":1,"later_field":true}"#;
        assert_eq!(
            decode_agent_state(legacy).expect("lenient"),
            AgentState::fresh()
        );
    }

    #[test]
    fn a_repeated_key_is_an_error_not_last_wins() {
        let raw = serde_json::to_string(&set_state())
            .expect("serialize")
            .replacen(r#""revision":2"#, r#""revision":2,"revision":9"#, 1);
        assert!(matches!(
            decode_agent_state(&raw).expect_err("refused"),
            DecodeError::Malformed(_)
        ));
    }

    #[test]
    fn root_key_list_names_every_field() {
        let mut s = set_state();
        s.resident_set = None;
        s.active = Some(sample_deployment());
        s.previous_active = Some(sample_deployment());
        s.candidate = Some(sample_deployment());
        s.in_flight_transaction = Some(TransactionRecord {
            transaction_id: "tx".into(),
            deployment_id: "d".into(),
            phase: DeployState::Received,
            kind: TransactionKind::Deploy,
            bundle_digest: None,
            bundle_path: None,
            correlation_id: None,
            started_monotonic_ns: None,
            last_transition_monotonic_ns: None,
            failure: None,
        });
        s.last_error = Some(ErrorRecord::new(ErrorCode::Internal, "x"));
        s.quarantined = vec![QuarantineRecord {
            transaction_id: "tx".into(),
            deployment_id: "d".into(),
            bundle_digest: None,
            phase: DeployState::Received,
            error: ErrorRecord::new(ErrorCode::Internal, "x"),
            quarantined_monotonic_ns: None,
        }];
        let mut keys: Vec<String> = serde_json::to_value(&s)
            .expect("value")
            .as_object()
            .expect("object")
            .keys()
            .cloned()
            .collect();
        keys.push("resident_set".into());
        keys.sort();
        let mut expected: Vec<String> = AGENT_STATE_ROOT_KEYS
            .iter()
            .map(|k| (*k).to_string())
            .collect();
        expected.sort();
        assert_eq!(keys, expected);
    }

    #[test]
    fn successor_keeps_the_counter() {
        let before = set_state();
        let mut removed = before.clone();
        removed.next_generation = None;
        removed.resident_set = None;
        assert_eq!(
            before.validate_successor(&removed),
            Err(AgentStateError::GenerationCounterRemoved)
        );
        let mut counter_only = AgentState::fresh();
        counter_only.next_generation = Some(5);
        let mut lowered = counter_only.clone();
        lowered.next_generation = Some(4);
        assert_eq!(
            counter_only.validate_successor(&lowered),
            Err(AgentStateError::GenerationCounterLowered { from: 5, to: 4 })
        );
        let mut raised = counter_only.clone();
        raised.next_generation = Some(6);
        assert_eq!(counter_only.validate_successor(&raised), Ok(()));
    }

    #[test]
    fn successor_keeps_the_set_and_its_identity() {
        let before = set_state();
        let mut dropped = before.clone();
        dropped.resident_set = None;
        assert_eq!(
            before.validate_successor(&dropped),
            Err(AgentStateError::ResidentSetRemoved)
        );
        let mut renamed = before.clone();
        renamed.resident_set.as_mut().expect("set").set_id = "resident-set-2".into();
        assert_eq!(
            before.validate_successor(&renamed),
            Err(AgentStateError::SetIdChanged {
                from: "resident-set-1".into(),
                to: "resident-set-2".into()
            })
        );
    }

    #[test]
    fn successor_advances_the_revision_when_the_set_changes() {
        let before = set_state();
        let mut changed = before.clone();
        changed.resident_set.as_mut().expect("set").members[0].state = MemberState::Quarantined;
        changed
            .resident_set
            .as_mut()
            .expect("set")
            .endpoint_map
            .remove(0);
        assert_eq!(
            before.validate_successor(&changed),
            Err(AgentStateError::RevisionNotAdvanced { from: 2, to: 2 })
        );
        changed.resident_set.as_mut().expect("set").revision = 3;
        assert_eq!(before.validate_successor(&changed), Ok(()));

        let mut lowered = before.clone();
        lowered.resident_set.as_mut().expect("set").revision = 1;
        assert_eq!(
            before.validate_successor(&lowered),
            Err(AgentStateError::RevisionNotAdvanced { from: 2, to: 1 })
        );
        // An unchanged set at the same revision is no change at all.
        assert_eq!(before.validate_successor(&before.clone()), Ok(()));
    }

    #[test]
    fn successor_never_takes_a_generation_below_the_newest_it_names() {
        let mut before = set_state();
        before.next_generation = Some(6);
        let set = before.resident_set.as_mut().expect("set");
        set.members[0].generation = 3;
        set.members[1].generation = 5;
        set.endpoint_map[0].generation = 3;
        set.endpoint_map[1].generation = 5;

        // A member at a generation the set never named, below the newest
        // one it does: a retired generation issued again.
        let mut reissued = before.clone();
        let next_set = reissued.resident_set.as_mut().expect("set");
        next_set.revision += 1;
        next_set.members[1] = sample_member("other", 4);
        next_set.endpoint_map[1] = endpoint("other", 4);
        assert_eq!(
            before.validate_successor(&reissued),
            Err(AgentStateError::GenerationReissued {
                generation: 4,
                newest: 5
            })
        );

        // A freshly allocated generation joins; a generation the set already
        // names may move between a member and its retained previous.
        let mut fresh = before.clone();
        let generation = fresh.allocate_generation(0).expect("allocate");
        let next_set = fresh.resident_set.as_mut().expect("set");
        next_set.revision += 1;
        let mut replacement = sample_member("tts", generation);
        replacement.previous = Some(next_set.members[1].to_retained());
        next_set.members[1] = replacement;
        next_set.endpoint_map[1] = endpoint("tts", generation);
        assert_eq!(before.validate_successor(&fresh), Ok(()));

        // An empty set names nothing to be below: this rule cannot see a
        // generation retired before the set emptied. Only allocation from
        // the counter keeps generation 3 from being handed out again.
        let mut emptied = before.clone();
        let next_set = emptied.resident_set.as_mut().expect("set");
        next_set.revision += 1;
        next_set.members.clear();
        next_set.endpoint_map.clear();
        let mut refilled = emptied.clone();
        let next_set = refilled.resident_set.as_mut().expect("set");
        next_set.revision += 1;
        next_set.members.push(sample_member("stt", 3));
        next_set.endpoint_map.push(endpoint("stt", 3));
        assert_eq!(emptied.validate_successor(&refilled), Ok(()));
    }

    #[test]
    fn the_version_display_names_every_accepted_version() {
        assert_eq!(
            super::AGENT_STATE_SCHEMA_VERSIONS.join(" or "),
            super::AGENT_STATE_SCHEMA_VERSIONS_DISPLAY
        );
    }

    #[test]
    fn to_retained_carries_every_field_a_member_shares_with_it() {
        let mut member = sample_member("tts", 2);
        member.promoted_monotonic_ns = Some(7);
        member.labels.insert("team".into(), "speech".into());
        let mut expected = serde_json::to_value(&member).expect("member");
        let object = expected.as_object_mut().expect("object");
        object.remove("state");
        object.remove("previous");
        assert_eq!(
            serde_json::to_value(member.to_retained()).expect("retained"),
            expected
        );
    }

    #[test]
    fn unknown_record_fields_are_refused_at_0_2_and_ignored_at_0_1() {
        let mut s = AgentState::fresh();
        s.schema_version = AGENT_STATE_SCHEMA_VERSION_RESIDENT_SET.into();
        s.next_generation = Some(1);
        s.last_error = Some(ErrorRecord::new(ErrorCode::Internal, "x"));
        let mut value = serde_json::to_value(&s).expect("value");
        value["last_error"]["later"] = serde_json::json!(true);
        match decode_agent_state(&value.to_string()).expect_err("refused") {
            DecodeError::InvalidPayload(message) => assert_eq!(
                message,
                AgentStateError::UnknownRecordField {
                    record: "last_error".into(),
                    field: "later".into()
                }
                .to_string()
            ),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
        let mut legacy = serde_json::to_value(AgentState {
            last_error: Some(ErrorRecord::new(ErrorCode::Internal, "x")),
            ..AgentState::fresh()
        })
        .expect("value");
        legacy["last_error"]["later"] = serde_json::json!(true);
        decode_agent_state(&legacy.to_string()).expect("0.1 stays lenient");
    }
}
