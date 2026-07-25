// SPDX-License-Identifier: Apache-2.0
//
// Rust mirror of `config/schemas/roadmap_target.json`.
//
// A roadmap target is deliberately NOT a platform support row: it has no
// row id, no support level, no model-class rows, no gate semantics, and no
// evidence. It is never matched against a detected platform and never
// counts as a supported or Planned combination. Promoting a target creates
// a new exact row with its own fixtures and evidence; it never mutates the
// descriptor into a row. The two types are separate so that "we intend to
// support this" can never be read as "this is supported".

use serde::{Deserialize, Serialize};
use tensorplate_protocol::serde_shape::{deserialize_map_only, is_canonical_identifier};

use crate::error::PlatformRegistryError;

/// Version of `config/schemas/roadmap_target.json`.
pub const ROADMAP_TARGET_SCHEMA_VERSION: &str = "0.1";

/// A future target not yet exact enough to be a support row.
///
/// Fields are private and read-only, and
/// [`RoadmapTarget::from_json`] is the only way to obtain one: the type
/// does not implement `Deserialize`, so no loader can construct an
/// unvalidated target.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RoadmapTarget {
    schema_version: String,
    target_id: String,
    intended_release: String,
    target: String,
    blocking_dependency: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireTarget {
    schema_version: String,
    target_id: String,
    intended_release: String,
    target: String,
    blocking_dependency: String,
}

impl RoadmapTarget {
    /// Parse and validate a roadmap-target document. All failures are
    /// fail-closed typed errors mapping to `ErrorCode::ConfigInvalid`.
    pub fn from_json(json: &str) -> Result<Self, PlatformRegistryError> {
        let value: serde_json::Value = serde_json::from_str(json)?;
        let observed = value
            .get("schema_version")
            .and_then(serde_json::Value::as_str)
            .ok_or(PlatformRegistryError::MissingSchemaVersion)?;
        if observed != ROADMAP_TARGET_SCHEMA_VERSION {
            return Err(PlatformRegistryError::UnsupportedSchemaVersion {
                got: observed.to_string(),
                expected: ROADMAP_TARGET_SCHEMA_VERSION,
            });
        }
        let mut de = serde_json::Deserializer::from_str(json);
        let wire: WireTarget = deserialize_map_only(&mut de)?;
        de.end()?;
        Self::from_wire(wire)
    }

    fn from_wire(wire: WireTarget) -> Result<Self, PlatformRegistryError> {
        if wire.schema_version != ROADMAP_TARGET_SCHEMA_VERSION {
            return Err(PlatformRegistryError::UnsupportedSchemaVersion {
                got: wire.schema_version,
                expected: ROADMAP_TARGET_SCHEMA_VERSION,
            });
        }
        let target = Self {
            schema_version: wire.schema_version,
            target_id: wire.target_id,
            intended_release: wire.intended_release,
            target: wire.target,
            blocking_dependency: wire.blocking_dependency,
        };
        target.validate()?;
        Ok(target)
    }

    fn validate(&self) -> Result<(), PlatformRegistryError> {
        if !is_canonical_identifier(&self.target_id) {
            return Err(PlatformRegistryError::InvalidRoadmapTarget {
                reason: "target_id must be lowercase alphanumeric segments separated by \
                         single hyphens",
            });
        }
        if [
            &self.intended_release,
            &self.target,
            &self.blocking_dependency,
        ]
        .iter()
        .any(|field| field.trim().is_empty())
        {
            return Err(PlatformRegistryError::InvalidRoadmapTarget {
                reason: "intended_release, target, and blocking_dependency must not be blank",
            });
        }
        Ok(())
    }

    /// Stable target identifier, disjoint from platform row identifiers.
    #[must_use]
    pub fn target_id(&self) -> &str {
        &self.target_id
    }

    /// Schema version of the target document.
    #[must_use]
    pub fn schema_version(&self) -> &str {
        &self.schema_version
    }

    /// Release line the target is aimed at.
    #[must_use]
    pub fn intended_release(&self) -> &str {
        &self.intended_release
    }

    /// What the target is.
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    /// What must exist before this target could become an exact row.
    #[must_use]
    pub fn blocking_dependency(&self) -> &str {
        &self.blocking_dependency
    }
}
