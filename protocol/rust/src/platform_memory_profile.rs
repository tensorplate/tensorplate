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
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformMemoryProfileName {
    /// One shared budget pool: CPU, accelerator, and OS reserve compete.
    UnifiedMemory,
    /// Two budget domains: guest RAM and device VRAM, each with its own
    /// budget and headroom.
    DiscreteGpu,
}

/// Budget-domain identifier, used as the telemetry per-domain qualifier.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetDomainName {
    SharedPool,
    GuestRam,
    DeviceVram,
}

/// Host-device transfer pressure posture for a profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyPressure {
    /// No transfer link exists (unified memory).
    NotApplicable,
    /// Recorded where the platform exposes it (discrete GPUs).
    RecordedWhereObservable,
}

/// One budget domain of a profile: what is measured and how headroom is
/// computed for it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetDomain {
    pub domain: BudgetDomainName,
    pub measurement_source: String,
    pub headroom_computation: String,
}

/// A platform instance carrying the profile, with its measurement-source
/// mapping. New platforms add instances without any schema change.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileInstance {
    pub instance: String,
    pub measurement_source_mapping: String,
}

/// Platform memory profile record, mirroring
/// `config/schemas/platform_memory_profile.json`. Parse with
/// [`PlatformMemoryProfile::from_json`], the validated entry point that
/// enforces the schema-scoped version track and the frozen per-profile
/// semantics.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformMemoryProfile {
    pub schema_version: String,
    pub profile: PlatformMemoryProfileName,
    pub budget_domains: Vec<BudgetDomain>,
    pub headroom_reporting: String,
    pub copy_pressure: CopyPressure,
    pub gate_semantics_note: String,
    pub instances: Vec<ProfileInstance>,
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
    /// that map to [`ErrorCode::ConfigInvalid`]. This is the validated
    /// entry point; deserializing the types directly with serde skips both
    /// the schema_version gate and the semantic validation.
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
        let record: Self = serde_json::from_value(value)?;
        record.validate_profile_semantics()?;
        Ok(record)
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
        for instance in &self.instances {
            if instance.instance.is_empty() {
                return Err(PlatformMemoryProfileError::InvalidProfile {
                    reason: "instance identifier must not be empty",
                });
            }
            if instance.measurement_source_mapping.is_empty() {
                return Err(PlatformMemoryProfileError::InvalidProfile {
                    reason: "instance measurement_source_mapping must not be empty",
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
