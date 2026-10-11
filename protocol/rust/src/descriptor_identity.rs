// SPDX-License-Identifier: Apache-2.0

//! Which deployment descriptor a durable record or a candidate reference
//! names.

use crate::agent_control::looks_like_digest;
use crate::resident_set::MAX_STATE_COUNTER;

/// The descriptor verified for one generation: the generation and the
/// descriptor's two digests, under the names and shapes a resident-set
/// member gives them. A record or reference carries all three or none.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DescriptorIdentity<'a> {
    pub generation: u64,
    pub descriptor_digest: &'a str,
    pub configuration_digest: &'a str,
}

#[derive(Clone, Debug, thiserror::Error, Eq, PartialEq)]
pub enum DescriptorIdentityError {
    #[error(
        "carries some of `generation`, `descriptor_digest` and `configuration_digest`; it must carry all three or none"
    )]
    Partial,
    #[error("generation {0} is outside [1, 2^53)")]
    GenerationOutOfRange(u64),
    #[error("`{0}` is not a digest of the form `<algorithm>:<hex>`")]
    InvalidDigest(&'static str),
}

impl<'a> DescriptorIdentity<'a> {
    /// Read the three optional fields of a record or reference.
    ///
    /// # Errors
    ///
    /// [`DescriptorIdentityError`] when only some of the three are present,
    /// or one is out of range or not digest-shaped.
    pub fn from_fields(
        generation: Option<u64>,
        descriptor_digest: Option<&'a str>,
        configuration_digest: Option<&'a str>,
    ) -> Result<Option<Self>, DescriptorIdentityError> {
        let (generation, descriptor_digest, configuration_digest) =
            match (generation, descriptor_digest, configuration_digest) {
                (None, None, None) => return Ok(None),
                (Some(generation), Some(descriptor), Some(configuration)) => {
                    (generation, descriptor, configuration)
                }
                _ => return Err(DescriptorIdentityError::Partial),
            };
        if !(1..=MAX_STATE_COUNTER).contains(&generation) {
            return Err(DescriptorIdentityError::GenerationOutOfRange(generation));
        }
        for (field, digest) in [
            ("descriptor_digest", descriptor_digest),
            ("configuration_digest", configuration_digest),
        ] {
            if !looks_like_digest(digest) {
                return Err(DescriptorIdentityError::InvalidDigest(field));
            }
        }
        Ok(Some(Self {
            generation,
            descriptor_digest,
            configuration_digest,
        }))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{DescriptorIdentity, DescriptorIdentityError};
    use crate::resident_set::MAX_STATE_COUNTER;

    const D: &str = "sha256:d0";
    const C: &str = "sha256:c0";

    #[test]
    fn all_three_or_none() {
        assert_eq!(DescriptorIdentity::from_fields(None, None, None), Ok(None));
        let whole = DescriptorIdentity::from_fields(Some(3), Some(D), Some(C))
            .expect("whole")
            .expect("present");
        assert_eq!(
            (
                whole.generation,
                whole.descriptor_digest,
                whole.configuration_digest
            ),
            (3, D, C)
        );
        for (generation, descriptor, configuration) in [
            (Some(3), None, None),
            (None, Some(D), None),
            (None, None, Some(C)),
            (None, Some(D), Some(C)),
            (Some(3), None, Some(C)),
            (Some(3), Some(D), None),
        ] {
            assert_eq!(
                DescriptorIdentity::from_fields(generation, descriptor, configuration),
                Err(DescriptorIdentityError::Partial)
            );
        }
    }

    #[test]
    fn each_field_keeps_the_members_shape() {
        for generation in [0, MAX_STATE_COUNTER + 1] {
            assert_eq!(
                DescriptorIdentity::from_fields(Some(generation), Some(D), Some(C)),
                Err(DescriptorIdentityError::GenerationOutOfRange(generation))
            );
        }
        assert!(DescriptorIdentity::from_fields(Some(MAX_STATE_COUNTER), Some(D), Some(C)).is_ok());
        assert_eq!(
            DescriptorIdentity::from_fields(Some(3), Some("d0"), Some(C)),
            Err(DescriptorIdentityError::InvalidDigest("descriptor_digest"))
        );
        assert_eq!(
            DescriptorIdentity::from_fields(Some(3), Some(D), Some("sha256:")),
            Err(DescriptorIdentityError::InvalidDigest(
                "configuration_digest"
            ))
        );
    }
}
