// SPDX-License-Identifier: Apache-2.0
//
// Rust mirror of `config/schemas/platform_memory_profile.json`: the
// property-named platform memory profile records (`unified_memory`,
// `discrete_gpu`) and the consolidated platform memory telemetry field
// names.
//
// Like the memory budget vocabulary, this is a config-level schema on its
// own version track ([`PLATFORM_MEMORY_PROFILE_SCHEMA_VERSION`]); failures
// map to `ErrorCode::ConfigInvalid`. The two canonical records are defined
// in code ([`PlatformMemoryProfile::unified_memory`] and
// [`PlatformMemoryProfile::discrete_gpu`]) and pinned by committed fixtures
// so platform rows and telemetry reference one definition. Records are
// purely descriptive — no numeric fields — so no numeric-lexeme validation
// applies here.

use serde::{Deserialize, Serialize};

use crate::{ErrorCode, ProtocolError};

/// Version of `config/schemas/platform_memory_profile.json`. Config schemas
/// evolve independently of the cross-process [`crate::PROTOCOL_VERSION`].
pub const PLATFORM_MEMORY_PROFILE_SCHEMA_VERSION: &str = "0.1";

/// Consolidated platform memory telemetry field names, in schema order.
/// Defined once here (mirroring the schema's `telemetry_field_name` enum);
/// platform registry and telemetry consumers reference these spellings and
/// never re-specify them. Budget-domain and identity qualifiers are
/// bounded-cardinality diagnostic fields on samples, never metric labels.
pub const PLATFORM_MEMORY_TELEMETRY_FIELD_NAMES: [&str; 13] = [
    "configured_budget_bytes",
    "projected_budget_bytes",
    "observed_peak_bytes",
    "headroom_bytes",
    "pressure_transitions",
    "cache_high_water_bytes",
    "ledger_active_sessions",
    "ledger_session_ceiling",
    "output_queue_observed_bytes",
    "sidecar_rss_bytes",
    "engine_pool_utilization",
    "engine_pool_preemptions",
    "gpu_memory_used_bytes",
];

/// Typed failures for platform memory profile record parsing. Static-config
/// validation failures; converts into a [`ProtocolError`] with
/// [`ErrorCode::ConfigInvalid`].
#[derive(Debug, thiserror::Error)]
pub enum PlatformMemoryProfileError {
    /// The document is not valid JSON or violates the record schema
    /// (unknown fields, missing required fields, non-enum values).
    #[error("malformed platform memory profile record: {0}")]
    Malformed(#[from] serde_json::Error),

    /// Top-level `schema_version` is missing or not a string.
    #[error("platform memory profile record is missing `schema_version`")]
    MissingSchemaVersion,

    /// `schema_version` does not match
    /// [`PLATFORM_MEMORY_PROFILE_SCHEMA_VERSION`].
    #[error("unsupported platform memory profile schema_version `{got}` (expected `{expected}`)")]
    UnsupportedSchemaVersion { got: String, expected: &'static str },

    /// The record contradicts the profile's frozen semantics (wrong budget
    /// domain set or copy-pressure posture for the named profile).
    #[error("invalid platform memory profile record: {reason}")]
    InvalidProfile { reason: &'static str },
}

impl From<PlatformMemoryProfileError> for ProtocolError {
    fn from(value: PlatformMemoryProfileError) -> Self {
        ProtocolError::new(
            ErrorCode::ConfigInvalid,
            "invalid platform memory profile record",
        )
        .with_context(value.to_string())
    }
}

/// Property-named profile identifier. Never vendor-named.
///
/// `try_from` pins decoding to the plain string form: serde's derived
/// `Deserialize` for a fieldless enum would also accept the
/// externally-tagged map form (`{"unified_memory": null}`), which the
/// schema rejects as `type: "string"`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "String")]
pub enum PlatformMemoryProfileName {
    /// One shared budget pool: CPU, accelerator, and OS reserve compete.
    UnifiedMemory,
    /// Two budget domains: guest RAM and device VRAM, each with its own
    /// budget and headroom.
    DiscreteGpu,
}

impl TryFrom<String> for PlatformMemoryProfileName {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "unified_memory" => Ok(Self::UnifiedMemory),
            "discrete_gpu" => Ok(Self::DiscreteGpu),
            other => Err(format!("unknown platform memory profile `{other}`")),
        }
    }
}

/// Budget-domain identifier, used as the telemetry per-domain qualifier.
/// String-only decoding, as for [`PlatformMemoryProfileName`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "String")]
pub enum BudgetDomainName {
    SharedPool,
    GuestRam,
    DeviceVram,
}

impl TryFrom<String> for BudgetDomainName {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "shared_pool" => Ok(Self::SharedPool),
            "guest_ram" => Ok(Self::GuestRam),
            "device_vram" => Ok(Self::DeviceVram),
            other => Err(format!("unknown budget domain `{other}`")),
        }
    }
}

/// Host-device transfer pressure posture for a profile. String-only
/// decoding, as for [`PlatformMemoryProfileName`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "String")]
pub enum CopyPressure {
    /// No transfer link exists (unified memory).
    NotApplicable,
    /// Recorded where the platform exposes it (discrete GPUs).
    RecordedWhereObservable,
}

impl TryFrom<String> for CopyPressure {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "not_applicable" => Ok(Self::NotApplicable),
            "recorded_where_observable" => Ok(Self::RecordedWhereObservable),
            other => Err(format!("unknown copy pressure posture `{other}`")),
        }
    }
}

/// Deserialize `T` from a JSON object only.
///
/// Serde's derived `Deserialize` for a struct accepts the sequence form
/// (`["shared_pool", "…", "…"]`) in addition to the map form, and
/// `deny_unknown_fields` does not cover that path — but the schema pins
/// `type: "object"`. Routing through `deserialize_map` keeps the decoder
/// from being weaker than the schema document, the same way `try_from =
/// "String"` pins the enums to their string form.
///
/// Object-encoding formats only. Formats that encode structs as sequences
/// (bincode, postcard, MessagePack in compact mode) cannot decode these
/// types, which is correct for a JSON config schema but would need
/// revisiting before the crate is used with such a format.
fn deserialize_map_only<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct MapOnly<T>(std::marker::PhantomData<T>);

    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for MapOnly<T> {
        type Value = T;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a JSON object")
        }

        fn visit_map<A>(self, map: A) -> Result<T, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            T::deserialize(serde::de::value::MapAccessDeserializer::new(map))
        }
    }

    deserializer.deserialize_map(MapOnly(std::marker::PhantomData))
}

/// One budget domain of a profile: what is measured and how headroom is
/// computed for it. Decodes from the object form only. Carries no
/// standalone invariants; its field constraints are enforced when it is
/// decoded as part of a [`PlatformMemoryProfile`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BudgetDomain {
    pub domain: BudgetDomainName,
    pub measurement_source: String,
    pub headroom_computation: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireBudgetDomain {
    domain: BudgetDomainName,
    measurement_source: String,
    headroom_computation: String,
}

impl<'de> Deserialize<'de> for BudgetDomain {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire: WireBudgetDomain = deserialize_map_only(deserializer)?;
        Ok(Self {
            domain: wire.domain,
            measurement_source: wire.measurement_source,
            headroom_computation: wire.headroom_computation,
        })
    }
}

/// A platform instance carrying the profile, with its measurement-source
/// mapping. New platforms add instances without any schema change.
/// Decodes from the object form only. Carries no standalone invariants;
/// its field constraints (identifier form, uniqueness, non-empty mapping)
/// are enforced when it is decoded as part of a [`PlatformMemoryProfile`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProfileInstance {
    pub instance: String,
    pub measurement_source_mapping: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireProfileInstance {
    instance: String,
    measurement_source_mapping: String,
}

impl<'de> Deserialize<'de> for ProfileInstance {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire: WireProfileInstance = deserialize_map_only(deserializer)?;
        Ok(Self {
            instance: wire.instance,
            measurement_source_mapping: wire.measurement_source_mapping,
        })
    }
}

/// Platform memory profile record, mirroring
/// `config/schemas/platform_memory_profile.json`. Validation is
/// unavoidable: the custom [`Deserialize`] impl routes every decoding
/// path — including generic serde configuration loaders — through the
/// same schema-version gate and frozen-semantics checks as
/// [`PlatformMemoryProfile::from_json`], and fields are private and
/// read-only, so a decoded record cannot exist or be mutated in an
/// invalid state. Prefer `from_json`, which reports failures as typed
/// [`PlatformMemoryProfileError`] values instead of serde messages.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlatformMemoryProfile {
    schema_version: String,
    profile: PlatformMemoryProfileName,
    budget_domains: Vec<BudgetDomain>,
    headroom_reporting: String,
    copy_pressure: CopyPressure,
    gate_semantics_note: String,
    instances: Vec<ProfileInstance>,
}

/// Private wire form — the only type serde derives deserialization for.
/// [`PlatformMemoryProfile`]'s `Deserialize` impl converts it through the
/// validating constructor so no decoding path can skip validation.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireProfile {
    schema_version: String,
    profile: PlatformMemoryProfileName,
    budget_domains: Vec<BudgetDomain>,
    headroom_reporting: String,
    copy_pressure: CopyPressure,
    gate_semantics_note: String,
    instances: Vec<ProfileInstance>,
}

impl<'de> Deserialize<'de> for PlatformMemoryProfile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire: WireProfile = deserialize_map_only(deserializer)?;
        Self::from_wire(wire).map_err(serde::de::Error::custom)
    }
}

/// Mirror of the schema's instance-identifier pattern
/// `^[a-z0-9]+(-[a-z0-9]+)*$`: lowercase alphanumeric segments joined by
/// single hyphens. Rejecting near-duplicates by form (case and stray
/// whitespace) keeps identifier resolution unambiguous for the registry
/// and telemetry consumers, and subsumes the non-empty check.
fn is_canonical_instance_id(id: &str) -> bool {
    !id.is_empty()
        && id.split('-').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

impl PlatformMemoryProfile {
    /// The canonical `unified_memory` record: one shared pool where CPU,
    /// accelerator, and OS reserve compete. v0.2 instances are the Jetson
    /// Orin rows plus Apple M1 Pro for platform-lifecycle accounting.
    #[must_use]
    pub fn unified_memory() -> Self {
        Self {
            schema_version: PLATFORM_MEMORY_PROFILE_SCHEMA_VERSION.to_string(),
            profile: PlatformMemoryProfileName::UnifiedMemory,
            budget_domains: vec![BudgetDomain {
                domain: BudgetDomainName::SharedPool,
                measurement_source: "Platform memory stats plus the accelerator working-set \
                                     bound; CPU, accelerator, and OS reserve compete in one pool."
                    .to_string(),
                headroom_computation: "row_budget - observed_peak(shared_pool)".to_string(),
            }],
            headroom_reporting: "Row budget minus observed peak of the single shared pool."
                .to_string(),
            copy_pressure: CopyPressure::NotApplicable,
            gate_semantics_note: "Gate values are row-owned; load_bearing on physical edge rows."
                .to_string(),
            instances: vec![
                ProfileInstance {
                    instance: "jetson-orin".to_string(),
                    measurement_source_mapping:
                        "Platform memory stats plus NVML/tegrastats-class accelerator \
                         working-set bound."
                            .to_string(),
                },
                ProfileInstance {
                    instance: "apple-m1-pro".to_string(),
                    measurement_source_mapping:
                        "Host statistics plus Metal working-set bound. Platform-lifecycle \
                         accounting only; no model-serving support claim."
                            .to_string(),
                },
            ],
        }
    }

    /// The canonical `discrete_gpu` record: guest RAM and device VRAM as
    /// separate budget domains. v0.2 instances are the GCP NVIDIA rows.
    #[must_use]
    pub fn discrete_gpu() -> Self {
        Self {
            schema_version: PLATFORM_MEMORY_PROFILE_SCHEMA_VERSION.to_string(),
            profile: PlatformMemoryProfileName::DiscreteGpu,
            budget_domains: vec![
                BudgetDomain {
                    domain: BudgetDomainName::GuestRam,
                    measurement_source: "Process RSS on the host side.".to_string(),
                    headroom_computation: "guest_ram_budget - observed_peak(guest_ram)".to_string(),
                },
                BudgetDomain {
                    domain: BudgetDomainName::DeviceVram,
                    measurement_source: "NVML-class device memory.".to_string(),
                    headroom_computation: "device_vram_budget - observed_peak(device_vram)"
                        .to_string(),
                },
            ],
            headroom_reporting: "Computed per domain; the binding constraint is reported."
                .to_string(),
            copy_pressure: CopyPressure::RecordedWhereObservable,
            gate_semantics_note: "Gate values are row-owned; memory load_bearing, thermal \
                                  context_only on cloud rows."
                .to_string(),
            instances: vec![
                ProfileInstance {
                    instance: "gcp-nvidia-l4-24gb".to_string(),
                    measurement_source_mapping:
                        "NVML-class device memory for device_vram; process RSS for guest_ram."
                            .to_string(),
                },
                ProfileInstance {
                    instance: "gcp-nvidia-rtx-pro-6000-se".to_string(),
                    measurement_source_mapping:
                        "NVML-class device memory for device_vram; process RSS for guest_ram."
                            .to_string(),
                },
                ProfileInstance {
                    instance: "gcp-nvidia-a100-40gb".to_string(),
                    measurement_source_mapping:
                        "NVML-class device memory for device_vram; process RSS for guest_ram."
                            .to_string(),
                },
            ],
        }
    }

    /// Look up the canonical record for a profile name. This is the
    /// reference platform rows resolve when they name their profile.
    #[must_use]
    pub fn canonical(name: PlatformMemoryProfileName) -> Self {
        match name {
            PlatformMemoryProfileName::UnifiedMemory => Self::unified_memory(),
            PlatformMemoryProfileName::DiscreteGpu => Self::discrete_gpu(),
        }
    }

    /// Parse and validate a profile record document, enforcing
    /// [`PLATFORM_MEMORY_PROFILE_SCHEMA_VERSION`] and the frozen
    /// per-profile semantics. All failures are fail-closed typed errors
    /// that map to [`ErrorCode::ConfigInvalid`]. Direct serde
    /// deserialization runs the same validation via the custom
    /// `Deserialize` impl; this entry point additionally reports typed
    /// errors instead of serde messages.
    pub fn from_json(json: &str) -> Result<Self, PlatformMemoryProfileError> {
        let value: serde_json::Value = serde_json::from_str(json)?;
        let observed = value
            .get("schema_version")
            .and_then(serde_json::Value::as_str)
            .ok_or(PlatformMemoryProfileError::MissingSchemaVersion)?;
        if observed != PLATFORM_MEMORY_PROFILE_SCHEMA_VERSION {
            return Err(PlatformMemoryProfileError::UnsupportedSchemaVersion {
                got: observed.to_string(),
                expected: PLATFORM_MEMORY_PROFILE_SCHEMA_VERSION,
            });
        }
        // Decode from the original text, not the `Value`: parsing through
        // `Value` collapses duplicate JSON keys last-wins, which would make
        // this path weaker than direct serde deserialization. The `Value`
        // above serves only the typed version pre-check.
        let mut de = serde_json::Deserializer::from_str(json);
        let wire: WireProfile = deserialize_map_only(&mut de)?;
        de.end()?;
        Self::from_wire(wire)
    }

    /// Validating constructor shared by [`Self::from_json`] and the custom
    /// `Deserialize` impl: every decoding path funnels through here.
    fn from_wire(wire: WireProfile) -> Result<Self, PlatformMemoryProfileError> {
        if wire.schema_version != PLATFORM_MEMORY_PROFILE_SCHEMA_VERSION {
            return Err(PlatformMemoryProfileError::UnsupportedSchemaVersion {
                got: wire.schema_version,
                expected: PLATFORM_MEMORY_PROFILE_SCHEMA_VERSION,
            });
        }
        let record = Self {
            schema_version: wire.schema_version,
            profile: wire.profile,
            budget_domains: wire.budget_domains,
            headroom_reporting: wire.headroom_reporting,
            copy_pressure: wire.copy_pressure,
            gate_semantics_note: wire.gate_semantics_note,
            instances: wire.instances,
        };
        record.validate_profile_semantics()?;
        Ok(record)
    }

    /// Schema version of the record (always
    /// [`PLATFORM_MEMORY_PROFILE_SCHEMA_VERSION`] for a decoded record).
    #[must_use]
    pub fn schema_version(&self) -> &str {
        &self.schema_version
    }

    /// The property-named profile identifier.
    #[must_use]
    pub fn profile(&self) -> PlatformMemoryProfileName {
        self.profile
    }

    /// The profile's frozen budget domains.
    #[must_use]
    pub fn budget_domains(&self) -> &[BudgetDomain] {
        &self.budget_domains
    }

    /// How headroom is reported for the profile.
    #[must_use]
    pub fn headroom_reporting(&self) -> &str {
        &self.headroom_reporting
    }

    /// Host-device transfer pressure posture.
    #[must_use]
    pub fn copy_pressure(&self) -> CopyPressure {
        self.copy_pressure
    }

    /// Note on row-owned gate semantics.
    #[must_use]
    pub fn gate_semantics_note(&self) -> &str {
        &self.gate_semantics_note
    }

    /// Platform instances carrying this profile.
    #[must_use]
    pub fn instances(&self) -> &[ProfileInstance] {
        &self.instances
    }

    /// The budget domain set and copy-pressure posture are frozen per
    /// profile: adding a platform never adds profile fields or domains,
    /// only a new instance.
    fn validate_profile_semantics(&self) -> Result<(), PlatformMemoryProfileError> {
        let domains: Vec<BudgetDomainName> = self.budget_domains.iter().map(|d| d.domain).collect();
        match self.profile {
            PlatformMemoryProfileName::UnifiedMemory => {
                if domains != [BudgetDomainName::SharedPool] {
                    return Err(PlatformMemoryProfileError::InvalidProfile {
                        reason: "unified_memory has exactly one budget domain: shared_pool",
                    });
                }
                if self.copy_pressure != CopyPressure::NotApplicable {
                    return Err(PlatformMemoryProfileError::InvalidProfile {
                        reason: "unified_memory has no transfer link; copy_pressure must be \
                                 not_applicable",
                    });
                }
            }
            PlatformMemoryProfileName::DiscreteGpu => {
                if domains != [BudgetDomainName::GuestRam, BudgetDomainName::DeviceVram] {
                    return Err(PlatformMemoryProfileError::InvalidProfile {
                        reason: "discrete_gpu has exactly two budget domains: guest_ram then \
                                 device_vram",
                    });
                }
                if self.copy_pressure != CopyPressure::RecordedWhereObservable {
                    return Err(PlatformMemoryProfileError::InvalidProfile {
                        reason: "discrete_gpu records copy pressure where observable",
                    });
                }
            }
        }
        // Mirror the schema's minItems/minLength constraints so the decoder
        // is never weaker than the schema document.
        if self.instances.is_empty() {
            return Err(PlatformMemoryProfileError::InvalidProfile {
                reason: "instances must not be empty",
            });
        }
        if self.headroom_reporting.is_empty() {
            return Err(PlatformMemoryProfileError::InvalidProfile {
                reason: "headroom_reporting must not be empty",
            });
        }
        if self.gate_semantics_note.is_empty() {
            return Err(PlatformMemoryProfileError::InvalidProfile {
                reason: "gate_semantics_note must not be empty",
            });
        }
        for domain in &self.budget_domains {
            if domain.measurement_source.is_empty() {
                return Err(PlatformMemoryProfileError::InvalidProfile {
                    reason: "budget domain measurement_source must not be empty",
                });
            }
            if domain.headroom_computation.is_empty() {
                return Err(PlatformMemoryProfileError::InvalidProfile {
                    reason: "budget domain headroom_computation must not be empty",
                });
            }
        }
        let mut seen_instances = std::collections::HashSet::new();
        for instance in &self.instances {
            if !is_canonical_instance_id(&instance.instance) {
                return Err(PlatformMemoryProfileError::InvalidProfile {
                    reason: "instance identifiers must be lowercase alphanumeric segments \
                             separated by single hyphens",
                });
            }
            if instance.measurement_source_mapping.is_empty() {
                return Err(PlatformMemoryProfileError::InvalidProfile {
                    reason: "instance measurement_source_mapping must not be empty",
                });
            }
            // Uniqueness lives here, not in the schema: Draft-07 cannot
            // express by-key uniqueness for an array of objects, and an
            // id-keyed JSON object would let duplicate keys collapse
            // last-wins instead of rejecting. Decoder-stricter, fail-closed.
            if !seen_instances.insert(instance.instance.as_str()) {
                return Err(PlatformMemoryProfileError::InvalidProfile {
                    reason: "instance identifiers must be unique",
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{
        BudgetDomainName, CopyPressure, PlatformMemoryProfile, PlatformMemoryProfileError,
        PlatformMemoryProfileName, PLATFORM_MEMORY_PROFILE_SCHEMA_VERSION,
    };
    use crate::{ErrorCode, ProtocolError};

    #[test]
    fn canonical_records_pass_their_own_validation() {
        for record in [
            PlatformMemoryProfile::unified_memory(),
            PlatformMemoryProfile::discrete_gpu(),
        ] {
            let json = serde_json::to_string(&record).expect("serialize");
            let back = PlatformMemoryProfile::from_json(&json).expect("canonical record decodes");
            assert_eq!(record, back);
        }
    }

    #[test]
    fn canonical_lookup_matches_constructors() {
        assert_eq!(
            PlatformMemoryProfile::canonical(PlatformMemoryProfileName::UnifiedMemory),
            PlatformMemoryProfile::unified_memory()
        );
        assert_eq!(
            PlatformMemoryProfile::canonical(PlatformMemoryProfileName::DiscreteGpu),
            PlatformMemoryProfile::discrete_gpu()
        );
    }

    #[test]
    fn unified_memory_semantics() {
        let p = PlatformMemoryProfile::unified_memory();
        assert_eq!(p.budget_domains.len(), 1);
        assert_eq!(p.budget_domains[0].domain, BudgetDomainName::SharedPool);
        assert_eq!(p.copy_pressure, CopyPressure::NotApplicable);
        assert_eq!(p.instances.len(), 2, "Jetson Orin + Apple M1 Pro");
    }

    #[test]
    fn discrete_gpu_semantics() {
        let p = PlatformMemoryProfile::discrete_gpu();
        let domains: Vec<BudgetDomainName> = p.budget_domains.iter().map(|d| d.domain).collect();
        assert_eq!(
            domains,
            [BudgetDomainName::GuestRam, BudgetDomainName::DeviceVram]
        );
        assert_eq!(p.copy_pressure, CopyPressure::RecordedWhereObservable);
        assert_eq!(p.instances.len(), 3, "three GCP NVIDIA rows");
    }

    #[test]
    fn wrong_domain_set_rejects() {
        let mut record = PlatformMemoryProfile::unified_memory();
        record.budget_domains = PlatformMemoryProfile::discrete_gpu().budget_domains;
        let json = serde_json::to_string(&record).expect("serialize");
        let err = PlatformMemoryProfile::from_json(&json).expect_err("domain set is frozen");
        assert!(matches!(
            err,
            PlatformMemoryProfileError::InvalidProfile { .. }
        ));
    }

    #[test]
    fn wrong_copy_pressure_rejects() {
        let mut record = PlatformMemoryProfile::discrete_gpu();
        record.copy_pressure = CopyPressure::NotApplicable;
        let json = serde_json::to_string(&record).expect("serialize");
        let err = PlatformMemoryProfile::from_json(&json).expect_err("posture is frozen");
        assert!(matches!(
            err,
            PlatformMemoryProfileError::InvalidProfile { .. }
        ));
    }

    #[test]
    fn enum_spellings_round_trip() {
        // Serialization uses `rename_all`, deserialization uses the
        // `TryFrom` literals; this pins the two together so a rename on
        // one side cannot drift from the other. The exhaustive matches
        // below make a newly added variant a compile error here rather
        // than a silently untested one.
        fn profile_spelling(v: PlatformMemoryProfileName) -> &'static str {
            match v {
                PlatformMemoryProfileName::UnifiedMemory => "unified_memory",
                PlatformMemoryProfileName::DiscreteGpu => "discrete_gpu",
            }
        }
        fn domain_spelling(v: BudgetDomainName) -> &'static str {
            match v {
                BudgetDomainName::SharedPool => "shared_pool",
                BudgetDomainName::GuestRam => "guest_ram",
                BudgetDomainName::DeviceVram => "device_vram",
            }
        }
        fn pressure_spelling(v: CopyPressure) -> &'static str {
            match v {
                CopyPressure::NotApplicable => "not_applicable",
                CopyPressure::RecordedWhereObservable => "recorded_where_observable",
            }
        }

        for variant in [
            PlatformMemoryProfileName::UnifiedMemory,
            PlatformMemoryProfileName::DiscreteGpu,
        ] {
            let spelling = profile_spelling(variant);
            let json = serde_json::to_string(&variant).expect("serialize");
            assert_eq!(json, format!("\"{spelling}\""));
            let back: PlatformMemoryProfileName = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, variant);
        }
        for variant in [
            BudgetDomainName::SharedPool,
            BudgetDomainName::GuestRam,
            BudgetDomainName::DeviceVram,
        ] {
            let spelling = domain_spelling(variant);
            let json = serde_json::to_string(&variant).expect("serialize");
            assert_eq!(json, format!("\"{spelling}\""));
            let back: BudgetDomainName = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, variant);
        }
        for variant in [
            CopyPressure::NotApplicable,
            CopyPressure::RecordedWhereObservable,
        ] {
            let spelling = pressure_spelling(variant);
            let json = serde_json::to_string(&variant).expect("serialize");
            assert_eq!(json, format!("\"{spelling}\""));
            let back: CopyPressure = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, variant);
        }
    }

    #[test]
    fn enum_map_form_rejects() {
        // The externally-tagged map form must not decode on any enum.
        serde_json::from_str::<PlatformMemoryProfileName>(r#"{"unified_memory":null}"#)
            .expect_err("profile name is string-only");
        serde_json::from_str::<BudgetDomainName>(r#"{"shared_pool":null}"#)
            .expect_err("budget domain is string-only");
        serde_json::from_str::<CopyPressure>(r#"{"not_applicable":null}"#)
            .expect_err("copy pressure is string-only");
    }

    #[test]
    fn instance_identifier_form_rejects_near_duplicates() {
        for bad in [
            "Jetson-Orin",
            "jetson-orin ",
            "jetson--orin",
            "-jetson",
            "jetson_orin",
        ] {
            let mut record = PlatformMemoryProfile::unified_memory();
            record.instances[0].instance = bad.to_string();
            let json = serde_json::to_string(&record).expect("serialize");
            let err = PlatformMemoryProfile::from_json(&json)
                .expect_err("near-duplicate identifier must reject");
            assert!(
                err.to_string().contains("instance identifiers"),
                "`{bad}` should be rejected on identifier form: {err}"
            );
        }
    }

    #[test]
    fn duplicate_instance_identifiers_reject() {
        let mut record = PlatformMemoryProfile::unified_memory();
        let duplicate = record.instances[0].clone();
        record.instances.push(duplicate);
        let json = serde_json::to_string(&record).expect("serialize");
        let err = PlatformMemoryProfile::from_json(&json).expect_err("duplicates must reject");
        assert!(
            err.to_string().contains("unique"),
            "reason should name uniqueness: {err}"
        );
    }

    #[test]
    fn direct_serde_deserialization_validates() {
        // The custom Deserialize impl makes validation unavoidable: a
        // generic serde loader gets the same fail-closed behavior as
        // from_json.
        let good =
            serde_json::to_string(&PlatformMemoryProfile::unified_memory()).expect("serialize");
        let decoded: PlatformMemoryProfile =
            serde_json::from_str(&good).expect("canonical record decodes directly");
        assert_eq!(decoded, PlatformMemoryProfile::unified_memory());

        let wrong_version =
            good.replace("\"schema_version\":\"0.1\"", "\"schema_version\":\"9.9\"");
        serde_json::from_str::<PlatformMemoryProfile>(&wrong_version)
            .expect_err("direct serde must reject unsupported versions");

        let mut wrong_semantics = PlatformMemoryProfile::unified_memory();
        wrong_semantics.copy_pressure = CopyPressure::RecordedWhereObservable;
        let raw = serde_json::to_string(&wrong_semantics).expect("serialize");
        serde_json::from_str::<PlatformMemoryProfile>(&raw)
            .expect_err("direct serde must reject frozen-semantics violations");
    }

    #[test]
    fn unknown_field_rejects_fail_closed() {
        let record = PlatformMemoryProfile::unified_memory();
        let mut value = serde_json::to_value(record).expect("serialize");
        value["vendor"] = serde_json::json!("nvidia");
        let json = serde_json::to_string(&value).expect("serialize");
        let err = PlatformMemoryProfile::from_json(&json).expect_err("unknown field must reject");
        assert!(matches!(err, PlatformMemoryProfileError::Malformed(_)));
    }

    #[test]
    fn vendor_named_profile_rejects() {
        let json = serde_json::to_string(&PlatformMemoryProfile::unified_memory())
            .expect("serialize")
            .replace("unified_memory", "apple_silicon");
        PlatformMemoryProfile::from_json(&json).expect_err("profile names are property-named");
    }

    #[test]
    fn unsupported_schema_version_rejects_and_maps_to_config_invalid() {
        let mut record = PlatformMemoryProfile::unified_memory();
        record.schema_version = "9.9".to_string();
        let json = serde_json::to_string(&record).expect("serialize");
        let err = PlatformMemoryProfile::from_json(&json).expect_err("wrong version must reject");
        assert!(matches!(
            err,
            PlatformMemoryProfileError::UnsupportedSchemaVersion { .. }
        ));
        let protocol_error = ProtocolError::from(err);
        assert_eq!(protocol_error.code, ErrorCode::ConfigInvalid);
    }

    #[test]
    fn missing_schema_version_rejects() {
        let mut value =
            serde_json::to_value(PlatformMemoryProfile::unified_memory()).expect("serialize");
        value
            .as_object_mut()
            .expect("object")
            .remove("schema_version");
        let json = serde_json::to_string(&value).expect("serialize");
        let err = PlatformMemoryProfile::from_json(&json).expect_err("missing version rejects");
        assert!(matches!(
            err,
            PlatformMemoryProfileError::MissingSchemaVersion
        ));
    }

    #[test]
    fn schema_version_constant_is_current() {
        assert_eq!(PLATFORM_MEMORY_PROFILE_SCHEMA_VERSION, "0.1");
    }
}
