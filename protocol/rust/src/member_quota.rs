// SPDX-License-Identifier: Apache-2.0
//
// Session quota assigned to one resident-set member.
//
// Each budget domain's residual capacity is to be partitioned into explicit
// per-member session quotas before any session count is derived, so two
// co-resident members never draw on the same remaining pool. This type is
// the record of one such assignment: the durable state file keeps it per
// member, and it is shaped to be the value the worker control channel
// carries to the worker that enforces it.

use serde::{Deserialize, Serialize};

use crate::json_numbers::deserialize_some_safe_bytes;
use crate::platform_memory_profile::BudgetDomainName;
use crate::serde_shape::deserialize_map_only;

/// Most concurrent sessions one member's quota may assign. A member's
/// session ledger reports every session it holds, one admission timestamp
/// for each reserved or active session, and this bound keeps the largest
/// ledger answer on the runtime control channel inside one frame.
pub const MAX_MEMBER_SESSIONS: u32 = 2_048;

/// Per-domain session quota bytes, keyed by budget domain name.
///
/// The keys are the [`BudgetDomainName`] spellings. A discrete-GPU host
/// partitions `guest_ram` and `device_vram` independently; a unified-memory
/// host uses `shared_pool` alone, so `shared_pool` never appears beside
/// either discrete domain. A domain that is absent carries no session quota
/// for this member. Each value is a byte count in `[0, 2^53)`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomainQuotaBytes {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some_safe_bytes"
    )]
    pub shared_pool: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some_safe_bytes"
    )]
    pub guest_ram: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some_safe_bytes"
    )]
    pub device_vram: Option<u64>,
}

impl DomainQuotaBytes {
    /// Quota bytes assigned in `domain`, if any.
    #[must_use]
    pub fn get(&self, domain: BudgetDomainName) -> Option<u64> {
        match domain {
            BudgetDomainName::SharedPool => self.shared_pool,
            BudgetDomainName::GuestRam => self.guest_ram,
            BudgetDomainName::DeviceVram => self.device_vram,
        }
    }

    /// Whether no domain carries a quota.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shared_pool.is_none() && self.guest_ram.is_none() && self.device_vram.is_none()
    }

    fn validate(&self) -> Result<(), MemberQuotaError> {
        if self.shared_pool.is_some() && (self.guest_ram.is_some() || self.device_vram.is_some()) {
            return Err(MemberQuotaError::SharedPoolWithDiscreteDomain);
        }
        Ok(())
    }
}

/// Session quota assigned to one member: the session count and the
/// per-domain bytes that back it.
///
/// `session_count` is the number of concurrent logical sessions assigned to
/// the member, at most [`MAX_MEMBER_SESSIONS`]; a worker's own ceiling may
/// only lower it. For a member admitted in qualification mode it records
/// the operator's test count for the member. A member that serves no
/// logical sessions (a vision or VLA deployment) carries zero sessions and
/// no domain bytes. A positive count always names the domains that back it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberQuota {
    pub session_count: u32,
    #[serde(deserialize_with = "deserialize_map_only")]
    pub domain_bytes: DomainQuotaBytes,
}

impl MemberQuota {
    /// Check the rules JSON field types cannot express.
    ///
    /// # Errors
    ///
    /// Returns [`MemberQuotaError`] naming the violated rule.
    pub fn validate(&self) -> Result<(), MemberQuotaError> {
        self.domain_bytes.validate()?;
        if self.session_count > MAX_MEMBER_SESSIONS {
            return Err(MemberQuotaError::TooManySessions(self.session_count));
        }
        if self.session_count > 0 && self.domain_bytes.is_empty() {
            return Err(MemberQuotaError::SessionsWithoutDomainBytes);
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum MemberQuotaError {
    #[error("quota.domain_bytes must not combine shared_pool with guest_ram or device_vram")]
    SharedPoolWithDiscreteDomain,
    #[error("quota.session_count is positive but quota.domain_bytes names no domain")]
    SessionsWithoutDomainBytes,
    #[error(
        "quota.session_count {0} exceeds the {MAX_MEMBER_SESSIONS} sessions one member may hold"
    )]
    TooManySessions(u32),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{DomainQuotaBytes, MemberQuota, MemberQuotaError};
    use crate::platform_memory_profile::BudgetDomainName;

    fn l4_quota() -> MemberQuota {
        MemberQuota {
            session_count: 1,
            domain_bytes: DomainQuotaBytes {
                shared_pool: None,
                guest_ram: Some(268_435_456),
                device_vram: Some(536_870_912),
            },
        }
    }

    #[test]
    fn discrete_domains_validate_and_round_trip() {
        let quota = l4_quota();
        quota.validate().expect("valid");
        let raw = serde_json::to_string(&quota).expect("encode");
        assert_eq!(
            raw,
            r#"{"session_count":1,"domain_bytes":{"guest_ram":268435456,"device_vram":536870912}}"#
        );
        let back: MemberQuota = serde_json::from_str(&raw).expect("decode");
        assert_eq!(back, quota);
        assert_eq!(
            back.domain_bytes.get(BudgetDomainName::GuestRam),
            Some(268_435_456)
        );
        assert_eq!(back.domain_bytes.get(BudgetDomainName::SharedPool), None);
    }

    #[test]
    fn zero_sessions_need_no_domains() {
        MemberQuota::default()
            .validate()
            .expect("a non-session member");
    }

    #[test]
    fn shared_pool_is_exclusive() {
        let mut quota = l4_quota();
        quota.domain_bytes.shared_pool = Some(1);
        assert_eq!(
            quota.validate(),
            Err(MemberQuotaError::SharedPoolWithDiscreteDomain)
        );
        quota.domain_bytes.guest_ram = None;
        assert_eq!(
            quota.validate(),
            Err(MemberQuotaError::SharedPoolWithDiscreteDomain)
        );
        quota.domain_bytes.device_vram = None;
        quota.validate().expect("shared_pool alone");
    }

    #[test]
    fn positive_count_needs_a_domain() {
        let quota = MemberQuota {
            session_count: 1,
            domain_bytes: DomainQuotaBytes::default(),
        };
        assert_eq!(
            quota.validate(),
            Err(MemberQuotaError::SessionsWithoutDomainBytes)
        );
    }

    #[test]
    fn decoder_rejects_unknown_domains_null_and_unsafe_bytes() {
        for raw in [
            r#"{"session_count":1,"domain_bytes":{"host_ram":1}}"#,
            r#"{"session_count":1,"domain_bytes":{"guest_ram":null}}"#,
            r#"{"session_count":1,"domain_bytes":{"guest_ram":-1}}"#,
            r#"{"session_count":1,"domain_bytes":{"guest_ram":1.5}}"#,
            r#"{"session_count":1,"domain_bytes":{"guest_ram":9007199254740992}}"#,
            r#"{"session_count":1,"domain_bytes":{},"extra":true}"#,
            r#"{"session_count":-1,"domain_bytes":{}}"#,
            r#"{"session_count":4294967296,"domain_bytes":{}}"#,
            r#"{"domain_bytes":{}}"#,
            r#"{"session_count":0,"domain_bytes":[]}"#,
            r#"{"session_count":1,"domain_bytes":[1,2,3]}"#,
        ] {
            assert!(
                serde_json::from_str::<MemberQuota>(raw).is_err(),
                "decoder accepted {raw}"
            );
        }
    }
}
