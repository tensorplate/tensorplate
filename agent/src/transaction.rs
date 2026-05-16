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
// `failed`. `received`/`verified` are replayable: a restart that finds
// the agent paused there can re-run the work safely. Phases that touched
// the worker (`prepared`, `warmed`) are not replayable — the worker may
// already have side effects — and a candidate that crashed there is
// quarantined.

use tensorplate_protocol::deploy_transaction::DeployState;

/// Classification of a phase from the recovery planner's point of view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhaseClass {
    /// Safe to re-run from scratch on restart.
    Replayable,
    /// Touched the worker; must be revalidated, possibly quarantined.
    WorkerSideEffect,
    /// Active deployment. No further work needed.
    Active,
    /// Terminal failure / rollback state.
    Terminal,
}

/// Lookup the [`PhaseClass`] for `state`.
#[must_use]
pub fn classify(state: DeployState) -> PhaseClass {
    match state {
        DeployState::Received | DeployState::Verified | DeployState::Staged => {
            PhaseClass::Replayable
        }
        DeployState::CapacityChecked => PhaseClass::Replayable,
        DeployState::Prepared | DeployState::Warmed | DeployState::Promoted => {
            PhaseClass::WorkerSideEffect
        }
        DeployState::Active => PhaseClass::Active,
        DeployState::Failed | DeployState::RolledBack => PhaseClass::Terminal,
    }
}

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
    use super::{classify, is_permitted, is_promotion, is_success_transition, PhaseClass};
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

    #[test]
    fn phase_classification_matches_planner_expectations() {
        assert!(matches!(
            classify(DeployState::Received),
            PhaseClass::Replayable
        ));
        assert!(matches!(
            classify(DeployState::Prepared),
            PhaseClass::WorkerSideEffect
        ));
        assert!(matches!(
            classify(DeployState::Warmed),
            PhaseClass::WorkerSideEffect
        ));
        assert!(matches!(classify(DeployState::Active), PhaseClass::Active));
        assert!(matches!(
            classify(DeployState::Failed),
            PhaseClass::Terminal
        ));
    }
}
