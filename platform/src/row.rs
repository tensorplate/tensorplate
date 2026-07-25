// SPDX-License-Identifier: Apache-2.0
//
// Rust mirror of `config/schemas/platform_support_row.json`: one exact
// platform support row.
//
// Every field is exact-version, which is the point: evidence recorded on
// one row never transfers to another, so a row that names "Ubuntu 24.04"
// does not cover 22.04 and a row that names one accelerator SKU does not
// cover its neighbour. Memory accounting references the platform memory
// profile records by property name rather than re-specifying them.
//
// Like the other config schemas, this versions on its own track and
// validation failures map to `ErrorCode::ConfigInvalid`. Validation is
// unavoidable: the custom `Deserialize` impl routes every decoding path
// through the same checks, and decoded rows are read-only.

use serde::{Deserialize, Serialize};
use tensorplate_protocol::json_numbers;
use tensorplate_protocol::serde_shape::{
    deserialize_map_only, deserialize_optional_map_only, deserialize_vec_map_only,
    is_canonical_identifier, is_canonical_snake_identifier,
};
use tensorplate_protocol::PlatformMemoryProfileName;

use crate::error::PlatformRegistryError;

/// Version of `config/schemas/platform_support_row.json`. Config schemas
/// evolve independently of the cross-process protocol version.
pub const PLATFORM_SUPPORT_ROW_SCHEMA_VERSION: &str = "0.1";

macro_rules! string_only_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident { $( $(#[$vmeta:meta])* $variant:ident => $spelling:literal ),+ $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case", try_from = "String")]
        pub enum $name {
            $( $(#[$vmeta])* $variant, )+
        }

        impl $name {
            /// Stable serialized spelling. The exhaustive match makes a
            /// newly added variant a compile error rather than a silently
            /// unspelled one.
            #[must_use]
            pub fn as_str(self) -> &'static str {
                match self {
                    $( Self::$variant => $spelling, )+
                }
            }
        }

        impl TryFrom<String> for $name {
            type Error = String;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                match value.as_str() {
                    $( $spelling => Ok(Self::$variant), )+
                    other => Err(format!(
                        concat!("unknown ", stringify!($name), " `{}`"), other
                    )),
                }
            }
        }
    };
}

string_only_enum! {
    /// Host CPU architecture.
    pub enum CpuArchitecture {
        X86_64 => "x86_64",
        Arm64 => "arm64",
    }
}

string_only_enum! {
    /// Host CPU vendor. `Any` is permitted only on accelerator-less
    /// utility rows, which make no vendor-specific claim.
    pub enum CpuVendor {
        Amd => "amd",
        Intel => "intel",
        Apple => "apple",
        NvidiaSoc => "nvidia_soc",
        Any => "any",
    }
}

string_only_enum! {
    /// Accelerator partition posture.
    pub enum Partitioning {
        /// The platform can partition the accelerator, but this row
        /// rejects partitioned devices fail-closed.
        Unsupported => "unsupported",
        /// The accelerator has no partitioning concept.
        NotApplicable => "not_applicable",
    }
}

string_only_enum! {
    /// Package channel a backend path installs through.
    pub enum PackageChannel {
        Apt => "apt",
        Homebrew => "homebrew",
    }
}

string_only_enum! {
    /// Posture of one platform signal on a row.
    pub enum GateValue {
        /// The signal gates behavior: it is acted on, not just reported.
        LoadBearing => "load_bearing",
        /// The signal is reported for context only.
        ContextOnly => "context_only",
        /// The signal is absent on this row; a reason is required.
        NotApplicable => "not_applicable",
    }
}

string_only_enum! {
    /// How a row definition was authored.
    pub enum Provenance {
        /// Transcribed from a real run on this exact hardware.
        Recorded => "recorded",
        /// Authored from vendor specifications because no validated
        /// hardware exists yet; re-verified when hardware first runs.
        SpecAuthored => "spec_authored",
    }
}

string_only_enum! {
    /// Where a row is validated.
    pub enum ValidationEnvironmentKind {
        Physical => "physical",
        CloudInstance => "cloud_instance",
    }
}

/// Support level of a row or a model-class row. Serialized in the
/// capitalized form the release notes use.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub enum SupportLevel {
    Production,
    Preview,
    Experimental,
    Planned,
}

impl SupportLevel {
    /// Stable serialized spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Production => "Production",
            Self::Preview => "Preview",
            Self::Experimental => "Experimental",
            Self::Planned => "Planned",
        }
    }
}

impl TryFrom<String> for SupportLevel {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "Production" => Ok(Self::Production),
            "Preview" => Ok(Self::Preview),
            "Experimental" => Ok(Self::Experimental),
            "Planned" => Ok(Self::Planned),
            other => Err(format!("unknown support level `{other}`")),
        }
    }
}

/// OS identity. `version` is exact: evidence from one version never
/// satisfies another.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OsIdentity {
    pub name: String,
    pub version: String,
    /// Image or distribution identity where the OS version alone is not
    /// exact (e.g. a JetPack release on an Ubuntu base).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_identity: Option<String>,
}

/// One recorded component of the kernel/driver/runtime stack.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackComponent {
    pub component: String,
    pub version: String,
}

/// Kernel constraints and driver/runtime versions, recorded exactly at
/// evidence time. Planned rows declare an empty list until their first
/// evidence run records the stack.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelDriverStack {
    #[serde(deserialize_with = "deserialize_vec_map_only")]
    pub components: Vec<StackComponent>,
}

/// Host CPU identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuIdentity {
    pub architecture: CpuArchitecture,
    pub vendor: CpuVendor,
}

/// Accelerator identity. Names the exact SKU: a near-miss never matches.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Accelerator {
    pub family: String,
    pub sku: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "json_numbers::deserialize_optional_safe_bytes"
    )]
    pub memory_bytes: Option<u64>,
    /// Reference to the platform memory profile record of the same
    /// property name; the profile owns budget domains, measurement
    /// sources, and headroom computation.
    pub memory_profile: PlatformMemoryProfileName,
    pub partitioning: Partitioning,
}

/// Required backend package set and channel for one backend path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendPackageSet {
    pub backend_path: String,
    pub channel: PackageChannel,
    pub packages: Vec<String>,
}

/// Pointer to a model support row valid on this platform row.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelClassRowRef {
    pub model_class_row: String,
    pub support_level: SupportLevel,
}

/// Posture of one signal, with the reason required when the signal is
/// absent on the row. The reason is a free-text row fact, deliberately not
/// a typed platform reason: those describe why a platform is unsupported,
/// not why a sensor is missing.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gate {
    pub gate: GateValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Posture of every platform signal on a row. Every signal is declared; a
/// signal absent on the row is recorded `not_applicable` with a reason
/// rather than omitted, so "we did not measure it" and "it does not exist
/// here" never look alike.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateSemantics {
    #[serde(deserialize_with = "deserialize_map_only")]
    pub thermal: Gate,
    #[serde(deserialize_with = "deserialize_map_only")]
    pub power: Gate,
    #[serde(deserialize_with = "deserialize_map_only")]
    pub throttle: Gate,
    #[serde(deserialize_with = "deserialize_map_only")]
    pub memory: Gate,
    #[serde(deserialize_with = "deserialize_map_only")]
    pub gpu_utilization: Gate,
}

impl GateSemantics {
    /// Every gate with its signal name, in schema order.
    #[must_use]
    pub fn signals(&self) -> [(&'static str, &Gate); 5] {
        [
            ("thermal", &self.thermal),
            ("power", &self.power),
            ("throttle", &self.throttle),
            ("memory", &self.memory),
            ("gpu_utilization", &self.gpu_utilization),
        ]
    }
}

/// Exact machine type or physical device the row is validated on.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationEnvironment {
    pub kind: ValidationEnvironmentKind,
    pub identity: String,
}

/// Where the row's evidence is filed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub location: String,
}

/// One exact platform support row.
///
/// Fields are private and read-only: a decoded row cannot exist or be
/// mutated into a state that violates the schema. Parse with
/// [`PlatformSupportRow::from_json`] for typed errors; direct serde
/// deserialization runs the same validation through the custom
/// `Deserialize` impl.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlatformSupportRow {
    schema_version: String,
    row_id: String,
    os: OsIdentity,
    kernel_driver_stack: KernelDriverStack,
    cpu: CpuIdentity,
    #[serde(skip_serializing_if = "Option::is_none")]
    accelerator: Option<Accelerator>,
    backend_packages: Vec<BackendPackageSet>,
    model_class_rows: Vec<ModelClassRowRef>,
    gate_semantics: GateSemantics,
    support_level: SupportLevel,
    provenance: Provenance,
    validation_environment: ValidationEnvironment,
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence: Option<Evidence>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRow {
    schema_version: String,
    row_id: String,
    #[serde(deserialize_with = "deserialize_map_only")]
    os: OsIdentity,
    #[serde(deserialize_with = "deserialize_map_only")]
    kernel_driver_stack: KernelDriverStack,
    #[serde(deserialize_with = "deserialize_map_only")]
    cpu: CpuIdentity,
    #[serde(default, deserialize_with = "deserialize_optional_map_only")]
    accelerator: Option<Accelerator>,
    #[serde(deserialize_with = "deserialize_vec_map_only")]
    backend_packages: Vec<BackendPackageSet>,
    #[serde(deserialize_with = "deserialize_vec_map_only")]
    model_class_rows: Vec<ModelClassRowRef>,
    #[serde(deserialize_with = "deserialize_map_only")]
    gate_semantics: GateSemantics,
    support_level: SupportLevel,
    provenance: Provenance,
    #[serde(deserialize_with = "deserialize_map_only")]
    validation_environment: ValidationEnvironment,
    #[serde(default, deserialize_with = "deserialize_optional_map_only")]
    evidence: Option<Evidence>,
}

impl<'de> Deserialize<'de> for PlatformSupportRow {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire: WireRow = deserialize_map_only(deserializer)?;
        Self::from_wire(wire).map_err(serde::de::Error::custom)
    }
}

impl PlatformSupportRow {
    /// Parse and validate a row document. All failures are fail-closed
    /// typed errors mapping to `ErrorCode::ConfigInvalid`.
    ///
    /// Byte-valued fields are validated on their exact decimal lexeme
    /// before parsing, so a declared size can never be silently rounded.
    /// **Invariant:** every number token in a row document is a byte
    /// value; adding a non-byte numeric field requires a field-scoped
    /// check instead of the document-level canonicalization used here.
    pub fn from_json(json: &str) -> Result<Self, PlatformRegistryError> {
        let canonical =
            json_numbers::canonicalize_byte_lexemes(json).map_err(|(token, reason)| {
                PlatformRegistryError::InvalidNumberLexeme { token, reason }
            })?;
        let value: serde_json::Value = serde_json::from_str(&canonical)?;
        let observed = value
            .get("schema_version")
            .and_then(serde_json::Value::as_str)
            .ok_or(PlatformRegistryError::MissingSchemaVersion)?;
        if observed != PLATFORM_SUPPORT_ROW_SCHEMA_VERSION {
            return Err(PlatformRegistryError::UnsupportedSchemaVersion {
                got: observed.to_string(),
                expected: PLATFORM_SUPPORT_ROW_SCHEMA_VERSION,
            });
        }
        // Decode from the canonical text, not the `Value`: parsing through
        // `Value` collapses duplicate JSON keys last-wins.
        let mut de = serde_json::Deserializer::from_str(&canonical);
        let wire: WireRow = deserialize_map_only(&mut de)?;
        de.end()?;
        Self::from_wire(wire)
    }

    /// Validating constructor shared by [`Self::from_json`] and the custom
    /// `Deserialize` impl: every decoding path funnels through here.
    fn from_wire(wire: WireRow) -> Result<Self, PlatformRegistryError> {
        if wire.schema_version != PLATFORM_SUPPORT_ROW_SCHEMA_VERSION {
            return Err(PlatformRegistryError::UnsupportedSchemaVersion {
                got: wire.schema_version,
                expected: PLATFORM_SUPPORT_ROW_SCHEMA_VERSION,
            });
        }
        let row = Self {
            schema_version: wire.schema_version,
            row_id: wire.row_id,
            os: wire.os,
            kernel_driver_stack: wire.kernel_driver_stack,
            cpu: wire.cpu,
            accelerator: wire.accelerator,
            backend_packages: wire.backend_packages,
            model_class_rows: wire.model_class_rows,
            gate_semantics: wire.gate_semantics,
            support_level: wire.support_level,
            provenance: wire.provenance,
            validation_environment: wire.validation_environment,
            evidence: wire.evidence,
        };
        row.validate()?;
        Ok(row)
    }

    #[allow(clippy::too_many_lines)]
    fn validate(&self) -> Result<(), PlatformRegistryError> {
        let invalid = |reason: &'static str| PlatformRegistryError::InvalidRow { reason };

        if !is_canonical_identifier(&self.row_id) {
            return Err(invalid(
                "row_id must be lowercase alphanumeric segments separated by single hyphens",
            ));
        }
        if self.os.name.is_empty() || self.os.version.is_empty() {
            return Err(invalid("os name and version must not be empty"));
        }
        if self
            .os
            .image_identity
            .as_ref()
            .is_some_and(String::is_empty)
        {
            return Err(invalid("os image_identity must not be empty when present"));
        }
        for component in &self.kernel_driver_stack.components {
            if !is_canonical_snake_identifier(&component.component) {
                return Err(invalid(
                    "stack component identifiers must be lower_snake_case",
                ));
            }
            if component.version.is_empty() {
                return Err(invalid("stack component versions must not be empty"));
            }
        }
        if self.backend_packages.is_empty() {
            return Err(invalid("a row must declare at least one backend path"));
        }
        for set in &self.backend_packages {
            if !is_canonical_snake_identifier(&set.backend_path) {
                return Err(invalid("backend_path must be lower_snake_case"));
            }
            if set.packages.is_empty() || set.packages.iter().any(String::is_empty) {
                return Err(invalid(
                    "each backend path must declare at least one non-empty package",
                ));
            }
        }
        if self
            .model_class_rows
            .iter()
            .any(|r| r.model_class_row.is_empty())
        {
            return Err(invalid("model_class_row pointers must not be empty"));
        }
        for (signal, gate) in self.gate_semantics.signals() {
            match (gate.gate, gate.reason.as_deref()) {
                (GateValue::NotApplicable, None | Some("")) => {
                    return Err(match signal {
                        "thermal" => invalid("thermal `not_applicable` requires a reason"),
                        "power" => invalid("power `not_applicable` requires a reason"),
                        "throttle" => invalid("throttle `not_applicable` requires a reason"),
                        "memory" => invalid("memory `not_applicable` requires a reason"),
                        _ => invalid("gpu_utilization `not_applicable` requires a reason"),
                    })
                }
                (_, Some("")) => return Err(invalid("a gate reason must not be empty")),
                _ => {}
            }
        }
        if self.validation_environment.identity.is_empty() {
            return Err(invalid("validation_environment identity must not be empty"));
        }
        if self
            .evidence
            .as_ref()
            .is_some_and(|e| e.location.is_empty())
        {
            return Err(invalid("evidence location must not be empty"));
        }
        if let Some(accelerator) = &self.accelerator {
            if accelerator.family.is_empty() || accelerator.sku.is_empty() {
                return Err(invalid("accelerator family and sku must not be empty"));
            }
        } else {
            // An accelerator-less row cannot report GPU utilization, and
            // makes no CPU-vendor claim.
            if self.gate_semantics.gpu_utilization.gate != GateValue::NotApplicable {
                return Err(invalid(
                    "a row without an accelerator must record gpu_utilization as not_applicable",
                ));
            }
        }
        if self.cpu.vendor == CpuVendor::Any && self.accelerator.is_some() {
            return Err(invalid(
                "cpu vendor `any` is permitted only on accelerator-less rows",
            ));
        }
        match self.support_level {
            SupportLevel::Production if self.evidence.is_none() => {
                return Err(invalid("Production rows must declare evidence"))
            }
            SupportLevel::Planned if self.evidence.is_some() => {
                return Err(invalid("Planned rows carry no evidence claim"))
            }
            SupportLevel::Planned if self.provenance != Provenance::SpecAuthored => {
                return Err(invalid(
                    "Planned rows are spec-authored until their hardware is validated",
                ))
            }
            SupportLevel::Planned if !self.model_class_rows.is_empty() => {
                return Err(invalid(
                    "Planned rows carry no model-class claims until they are validated",
                ))
            }
            _ => {}
        }
        Ok(())
    }

    /// Stable row identifier.
    #[must_use]
    pub fn row_id(&self) -> &str {
        &self.row_id
    }

    /// Schema version of the row document.
    #[must_use]
    pub fn schema_version(&self) -> &str {
        &self.schema_version
    }

    /// Exact OS identity.
    #[must_use]
    pub fn os(&self) -> &OsIdentity {
        &self.os
    }

    /// Kernel/driver/runtime stack recorded at evidence time.
    #[must_use]
    pub fn kernel_driver_stack(&self) -> &KernelDriverStack {
        &self.kernel_driver_stack
    }

    /// Host CPU identity.
    #[must_use]
    pub fn cpu(&self) -> CpuIdentity {
        self.cpu
    }

    /// Accelerator identity, absent on accelerator-less rows.
    #[must_use]
    pub fn accelerator(&self) -> Option<&Accelerator> {
        self.accelerator.as_ref()
    }

    /// Required backend package sets.
    #[must_use]
    pub fn backend_packages(&self) -> &[BackendPackageSet] {
        &self.backend_packages
    }

    /// Model support rows valid on this platform row.
    #[must_use]
    pub fn model_class_rows(&self) -> &[ModelClassRowRef] {
        &self.model_class_rows
    }

    /// Signal posture on this row.
    #[must_use]
    pub fn gate_semantics(&self) -> &GateSemantics {
        &self.gate_semantics
    }

    /// Appliance-lifecycle support level.
    #[must_use]
    pub fn support_level(&self) -> SupportLevel {
        self.support_level
    }

    /// How this row definition was authored.
    #[must_use]
    pub fn provenance(&self) -> Provenance {
        self.provenance
    }

    /// Where the row is validated.
    #[must_use]
    pub fn validation_environment(&self) -> &ValidationEnvironment {
        &self.validation_environment
    }

    /// Where the row's evidence is filed, absent on rows that carry no
    /// evidence claim.
    #[must_use]
    pub fn evidence(&self) -> Option<&Evidence> {
        self.evidence.as_ref()
    }

    /// Whether this row counts as a supported combination in release
    /// notes. Planned rows are defined but never claimed.
    #[must_use]
    pub fn is_supported_combination(&self) -> bool {
        !matches!(self.support_level, SupportLevel::Planned)
    }
}
