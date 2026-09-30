// SPDX-License-Identifier: Apache-2.0

//! Configuration only; live admission consumes these limits when it is enabled.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tensorplate_protocol::serde_shape::{
    deserialize_map_only, deserialize_some, is_canonical_identifier,
};
use tensorplate_protocol::{json_numbers::MAX_SAFE_BYTES, BudgetDomainName};

use crate::error::{AgentError, AgentResult};

/// A cap is optional until measurement; a reserve must be explicitly supplied.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryDomainConfig {
    #[serde(
        default,
        deserialize_with = "deserialize_some",
        skip_serializing_if = "Option::is_none"
    )]
    pub cap_bytes: Option<u64>,
    pub reserve_bytes: u64,
}

impl MemoryDomainConfig {
    /// Resolve an omitted cap without a nominal-capacity fallback.
    pub fn resolve_cap(&self, measured_capacity: Option<u64>) -> AgentResult<u64> {
        self.validate()?;
        let measured = measured_capacity
            .filter(|v| *v > 0 && *v <= MAX_SAFE_BYTES)
            .ok_or_else(|| {
                AgentError::Config("memory capacity measurement unavailable or invalid".into())
            })?;
        let cap = self.cap_bytes.unwrap_or(measured);
        if cap > measured || self.reserve_bytes > cap {
            return Err(AgentError::Config(
                "memory cap exceeds measurement or reserve exceeds cap".into(),
            ));
        }
        Ok(cap)
    }

    fn validate(&self) -> AgentResult<()> {
        if self.reserve_bytes > MAX_SAFE_BYTES
            || self
                .cap_bytes
                .is_some_and(|cap| cap == 0 || cap > MAX_SAFE_BYTES || self.reserve_bytes > cap)
        {
            return Err(AgentError::Config(
                "invalid memory admission cap or reserve".into(),
            ));
        }
        Ok(())
    }
}

/// The row/profile join is declarative until the profile reader is enabled.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryAdmissionConfig {
    pub row_id: String,
    pub memory_profile_instance_id: String,
    #[serde(deserialize_with = "deserialize_domains")]
    pub domains: HashMap<BudgetDomainName, MemoryDomainConfig>,
}

fn deserialize_domains<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<HashMap<BudgetDomainName, MemoryDomainConfig>, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Domains {
        #[serde(deserialize_with = "deserialize_map_only")]
        guest_ram: MemoryDomainConfig,
        #[serde(deserialize_with = "deserialize_map_only")]
        device_vram: MemoryDomainConfig,
    }
    let domains: Domains = deserialize_map_only(deserializer)?;
    Ok(HashMap::from([
        (BudgetDomainName::GuestRam, domains.guest_ram),
        (BudgetDomainName::DeviceVram, domains.device_vram),
    ]))
}

impl MemoryAdmissionConfig {
    pub fn validate(&self) -> AgentResult<()> {
        if !is_canonical_identifier(&self.row_id)
            || !is_canonical_identifier(&self.memory_profile_instance_id)
        {
            return Err(AgentError::Config(
                "memory admission row/profile identifiers must be lowercase hyphenated identifiers"
                    .into(),
            ));
        }
        if self.domains.len() != 2
            || !self.domains.contains_key(&BudgetDomainName::GuestRam)
            || !self.domains.contains_key(&BudgetDomainName::DeviceVram)
        {
            return Err(AgentError::Config(
                "memory admission requires guest_ram and device_vram domains".into(),
            ));
        }
        for domain in self.domains.values() {
            domain.validate()?;
        }
        Ok(())
    }
}
