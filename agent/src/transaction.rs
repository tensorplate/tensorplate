// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F04: Deploy transaction state machine.
//
// Phases (forward-only along the success path; failed/rolled_back are
// terminal):
//
//   received -> verified -> staged -> capacity_checked -> prepared
//             -> warmed -> promoted -> active
//
// Failure transitions are typed; every candidate phase can transition to
// `failed`. `promoted` is recorded by the commit write, so a start reads
// it, with the records, as committed. No phase is run again at a start:
// a transaction found before its commit is ended as failed. The agent
// records no phase after `promoted`: the write that ends a transaction
// clears it.

use tensorplate_protocol::deploy_transaction::DeployState;

/// True if `next` is a permitted successor of `current` along the
/// success path. Terminal transitions (any phase to `failed` /
/// `rolled_back`) are handled separately.
#[must_use]
pub fn is_success_transition(current: DeployState, next: DeployState) -> bool {
    use DeployState::{
        Active, CapacityChecked, Failed, Prepared, Promoted, Received, RolledBack, Staged,
        Verified, Warmed,
    };
    matches!(
        (current, next),
        (Received, Verified)
            | (Verified, Staged)
            | (Staged, CapacityChecked)
            | (CapacityChecked, Prepared)
            | (Prepared, Warmed)
            | (Warmed, Promoted)
            | (Promoted, Active)
    ) && !matches!(next, Failed | RolledBack)
}

/// True if `next` is any permitted transition out of `current` — success,
/// failure, or rollback.
#[must_use]
pub fn is_permitted(current: DeployState, next: DeployState) -> bool {
    if is_success_transition(current, next) {
        return true;
    }
    matches!(
        (current, next),
        (
            DeployState::Received
                | DeployState::Verified
                | DeployState::Staged
                | DeployState::CapacityChecked
                | DeployState::Prepared
                | DeployState::Warmed
                | DeployState::Promoted,
            DeployState::Failed,
        ) | (
            DeployState::Active | DeployState::Promoted,
            DeployState::RolledBack
        )
    )
}

/// True if `state` is the only phase that mutates the desired active
/// deployment. Used by the coordinator to centralize the rotation.
#[must_use]
pub fn is_promotion(state: DeployState) -> bool {
    matches!(state, DeployState::Promoted)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_used,
        clippy::default_trait_access
    )]
    use super::{is_permitted, is_promotion, is_success_transition};
    use tensorplate_protocol::deploy_transaction::DeployState;

    #[test]
    fn happy_path_transitions_are_permitted() {
        let path = [
            DeployState::Received,
            DeployState::Verified,
            DeployState::Staged,
            DeployState::CapacityChecked,
            DeployState::Prepared,
            DeployState::Warmed,
            DeployState::Promoted,
            DeployState::Active,
        ];
        for w in path.windows(2) {
            assert!(
                is_success_transition(w[0], w[1]),
                "{:?} -> {:?} should be a success transition",
                w[0],
                w[1]
            );
            assert!(is_permitted(w[0], w[1]));
        }
    }

    #[test]
    fn invalid_skips_are_rejected() {
        // Skipping verified is not permitted.
        assert!(!is_success_transition(
            DeployState::Received,
            DeployState::Staged
        ));
        // Going backwards is not permitted.
        assert!(!is_permitted(DeployState::Active, DeployState::Warmed));
    }

    #[test]
    fn any_candidate_phase_may_fail() {
        for s in [
            DeployState::Received,
            DeployState::Verified,
            DeployState::Staged,
            DeployState::CapacityChecked,
            DeployState::Prepared,
            DeployState::Warmed,
            DeployState::Promoted,
        ] {
            assert!(is_permitted(s, DeployState::Failed));
        }
    }

    #[test]
    fn promotion_is_unique() {
        assert!(is_promotion(DeployState::Promoted));
        for s in [
            DeployState::Received,
            DeployState::Verified,
            DeployState::Staged,
            DeployState::CapacityChecked,
            DeployState::Prepared,
            DeployState::Warmed,
            DeployState::Active,
        ] {
            assert!(!is_promotion(s));
        }
    }
}
