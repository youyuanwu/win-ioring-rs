//! The handle-mode arm — the same backends measured through both handle modes.
//!
//! # The question
//!
//! `win_ioring::file::File::open` now sets `FILE_FLAG_OVERLAPPED`. Before it
//! did, every one of the twenty `win-ioring` cells in the published matrix was
//! measured through a handle carrying `FILE_SYNCHRONOUS_IO_NONALERT`, which
//! makes the kernel serialise at the file object and take a per-operation lock.
//! `docs/performance.md` argued that this could not matter under a warm cache —
//! nothing waits, so there is nothing to overlap — but that was a mechanism
//! argument, not a measurement, and it was conceded as such.
//!
//! This arm measures it.
//!
//! # Why it is a separate target rather than two more matrix columns
//!
//! Not preference. `combinations % backends == 0` is asserted in
//! [`crate::harness`], the matrix has ten combinations, and five backends
//! divide it where six or seven do not. That assertion's message says relaxing
//! it would silently give up the position balance the rotation exists to
//! provide, so the matrix cannot absorb the extra arms.
//!
//! The constraint turned out to be load-bearing rather than merely survived.
//! Forced into its own target with its own budget, the A/B can afford to cover
//! **both** ring backends at three depths — which the main-matrix design wanted
//! and could not afford. The guard produced a better experiment than the one it
//! blocked.
//!
//! # Why a paired arm rather than a before/after comparison
//!
//! The obvious design is to treat the published matrix as "before" and a fresh
//! run as "after". `docs/performance.md` states the matrix is a single-run
//! artefact, and its own repeat-run analysis shows between-run drift large
//! enough to swamp the effect being looked for. Comparing across runs on
//! different days confounds handle mode with drift. Here both modes are present
//! in one run, under one set of conditions, with the rotation and fairness
//! discipline that made the compio comparison trustworthy.
//!
//! # What these numbers are not
//!
//! **They are not comparable to main-matrix cells.** Two independent reasons,
//! and both must travel with the figures rather than staying in a methods note:
//!
//! 1. This arm opens files **outside** the timed region; the matrix opens
//!    inside it. See [`crate::scenario::run_on_open_file`] for why that is
//!    required here — briefly, an open is a place the flag can cost something,
//!    and charging it per iteration would fold per-open cost into the A/B delta
//!    *and* destroy the depth-1 negative control.
//! 2. This arm runs its own depth set flat across both scenarios rather than
//!    inheriting the matrix's per-scenario depths.
//!
//! Within the arm the comparison is exact, because both sides are subject to
//! both differences equally. Across arms it is not, and no cross-arm ratio
//! should be computed from these figures.
//!
//! # Guards live here, not in the bench target
//!
//! `benches/handle-mode.rs` is `harness = false`, so a `#[test]` inside it is
//! compiled but **never executed** — `cargo test --all-targets` reports success
//! with an unconditionally panicking test in such a file. Guards written there
//! are type-checked scenery. See `docs/testing.md`.

use std::time::Duration;

use crate::account::Budget;
use crate::harness::Which;
use crate::scenario::Scenario;

/// The wall-clock budget this arm is sized against.
///
/// Deliberately not [`crate::account::RUN_BUDGET`], which sizes the fifty-cell
/// matrix, and deliberately not [`crate::account::UNBUFFERED_RUN_BUDGET`],
/// which sizes a device-bound arm whose floor-to-wall behaviour is its own.
/// This arm is warm-cache, so the matrix's measured multipliers apply.
///
/// # The margin, as a number
///
/// The affordability convention is `floor() * 2 <= BUDGET`, so with a 108 s
/// floor the constant must be at least 216 s. **216 s exactly was rejected**:
/// a proposal that meets its own threshold with zero margin is not really
/// meeting a threshold, and this work had already rejected a different sizing
/// option for that defect before nearly shipping the same defect here. See
/// `docs/testing.md`.
///
/// 320 s leaves **104 s of margin, 48% above the 216 s requirement**. Derived
/// from the projection rather than picked: 108 s floor times the measured
/// 1.79x–2.06x floor-to-wall multipliers is 193 s–222 s of expected wall, so
/// 320 s clears the worst observed multiplier by 98 s.
pub const HANDLE_MODE_RUN_BUDGET: Duration = Duration::from_secs(320);

/// The depths this arm measures, flat across both scenarios.
///
/// Declared rather than inherited from [`crate::config::Config::depths_for`],
/// which special-cases the bulk-read scenario to its maximum depth only. This
/// arm needs the same depths in both scenarios, because depth is the axis the
/// hypothesis is stated along.
///
/// Depth 1 is the **negative control**, not padding. The mechanism under test
/// is serialisation of concurrent operations at the file object, and one
/// operation at a time cannot be serialised further, so the pre-registered
/// prediction is that the effect is absent or negligible here. A depth-1
/// difference is evidence of run-level drift or of a confound, not of the
/// hypothesis — which is exactly why the open had to come out of the timed
/// region, since a per-open cost difference would have shown up here and been
/// misread as drift.
pub const DEPTHS: [usize; 3] = [1, 8, 64];

/// The scenarios this arm measures.
///
/// Both are pure reads. The write scenario is excluded because
/// [`crate::scenario::run_on_open_file`] has no pre-opened form for it, and
/// because the hypothesis is about read concurrency.
pub const SCENARIOS: [Scenario; 2] = [Scenario::SequentialRead, Scenario::RandomRead];

/// The backend configurations this arm runs.
///
/// Six: both ring backends in both handle modes, plus compio and the
/// single-thread pool as reference points. The two references are what make an
/// A/B delta interpretable — without them a run cannot say whether an unusual
/// number is the effect or the host.
///
/// `tokio::fs` appears at pool width 1 only. It is included knowing that it
/// opens through `std` and therefore gets a **synchronous** handle, which is
/// disclosed rather than corrected: changing it inside this work would confound
/// the variable the experiment is built around. Recorded in
/// `docs/pending-work.md` as a follow-up.
pub const CONFIGS: [Which; 6] = [
    Which::RingPlain,
    Which::RingPlainSync,
    Which::RingRegistered,
    Which::RingRegisteredSync,
    Which::Compio,
    Which::TokioOne,
];

/// The Criterion group name for a scenario in this arm.
///
/// **The prefix is load-bearing and is not cosmetic.** Criterion keys are
/// `<group>/<slug>/<depth>`, and four of the six configurations reuse slugs
/// that already exist in the published matrix — `ioring-owned`,
/// `ioring-registered`, `compio-iocp` and `tokio-pool-1`. Grouping by the bare
/// scenario slug would write `sequential-read/ioring-owned/8`, which is the
/// published matrix's own key, and would overwrite a stored baseline.
///
/// Note that a slug-uniqueness test **cannot** catch this: the four slugs
/// collide by construction and are supposed to. The collision is in the group
/// name, and it is asserted separately below.
#[must_use]
pub fn group_name(scenario: Scenario) -> String {
    format!("handle-mode-{}", scenario.slug())
}

/// One cell of this arm's grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    /// Which scenario.
    pub scenario: Scenario,
    /// Outstanding operations.
    pub depth: usize,
    /// Which backend configuration.
    pub config: Which,
}

/// The backend order for the `index`th combination.
///
/// Rotated so no configuration is systematically advantaged by always running
/// first on a freshly settled machine, and deterministic so two runs visit the
/// same order.
///
/// Written for this arm rather than inherited. The unbuffered arm has no
/// rotation, no ledger and no balance test, so copying its structure would have
/// silently dropped the property that makes a comparison worth running. The
/// matrix's [`crate::harness::rotated_order`] cannot be reused directly because
/// it rotates over the five matrix backends, not these six.
///
/// With six configurations and six combinations, `6 % 6 == 0`, so every
/// configuration occupies every position exactly once and the balance property
/// is exact rather than approximate.
#[must_use]
pub fn rotated_order(index: usize) -> [Which; 6] {
    let mut order = CONFIGS;
    let count = order.len();
    order.rotate_left(index % count);
    order
}

/// Every cell this arm runs, in a fixed order.
///
/// The rotation is applied per combination, so the configurations are visited
/// in a different order in each.
#[must_use]
pub fn grid() -> Vec<Cell> {
    let mut cells = Vec::new();
    let mut combination = 0_usize;
    for scenario in SCENARIOS {
        for depth in DEPTHS {
            for config in rotated_order(combination) {
                cells.push(Cell {
                    scenario,
                    depth,
                    config,
                });
            }
            combination += 1;
        }
    }
    cells
}

/// How many combinations this arm has.
#[must_use]
pub fn combinations() -> usize {
    SCENARIOS.len() * DEPTHS.len()
}

/// How many benchmarks a full run of this arm produces.
#[must_use]
pub fn benchmarks() -> usize {
    grid().len()
}

/// Whether the projected floor fits inside this arm's budget.
///
/// The half-budget convention matches the other arms: the floor is required to
/// fit in half the budget, because measured floor-to-wall multipliers on the
/// warm-cache matrix were 1.79x to 2.06x. Unlike the unbuffered arm, those
/// multipliers are a *measurement* here rather than an analogy, because this
/// arm is warm-cache too.
#[must_use]
pub fn affordable() -> bool {
    Budget::CHOSEN.floor(benchmarks()) * 2 <= HANDLE_MODE_RUN_BUDGET
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::ioring::HandleMode;

    #[test]
    fn the_grid_is_six_configurations_over_six_combinations() {
        assert_eq!(combinations(), 6, "the combination count changed");
        assert_eq!(CONFIGS.len(), 6, "the configuration count changed");
        assert_eq!(
            benchmarks(),
            36,
            "the benchmark count changed; re-derive the floor and re-check \
             HANDLE_MODE_RUN_BUDGET's margin before accepting this"
        );
    }

    /// The A/B must actually straddle the variable.
    ///
    /// Both ring backends must appear in both modes. An arm that ran only
    /// overlapped configurations would produce a full set of numbers and answer
    /// nothing, which is the failure mode this whole target exists to avoid.
    #[test]
    fn both_ring_backends_appear_in_both_handle_modes() {
        for twin in [Which::RingPlain, Which::RingRegistered] {
            let pair: Vec<Which> = CONFIGS
                .iter()
                .copied()
                .filter(|w| w.overlapped_twin() == twin)
                .collect();
            assert_eq!(
                pair.len(),
                2,
                "{twin:?} does not appear as a pair in CONFIGS, so it is not \
                 A/B'd at all"
            );
            let modes: Vec<HandleMode> = pair.iter().map(|w| w.handle_mode()).collect();
            assert!(
                modes.contains(&HandleMode::Overlapped) && modes.contains(&HandleMode::Synchronous),
                "{twin:?}'s pair does not span both handle modes, so its A/B \
                 compares two identical configurations"
            );
        }
    }

    /// The group prefix keeps this arm off the published baselines.
    ///
    /// Asserted against the bare scenario slug, which is what the matrix uses,
    /// because that is the collision that would overwrite published data.
    #[test]
    fn group_names_do_not_collide_with_the_published_matrix() {
        for scenario in Scenario::all() {
            let group = group_name(scenario);
            assert_ne!(
                group,
                scenario.slug(),
                "this arm's group name for {scenario:?} is the matrix's own \
                 group name, so its benchmarks would overwrite the published \
                 baselines for every slug the two arms share"
            );
            assert!(
                group.starts_with("handle-mode-"),
                "{group} is not namespaced to this arm"
            );
        }
        // The specific collision, spelled out: four configurations reuse
        // published slugs, so only the group prefix separates the keys.
        let shared: Vec<Which> = CONFIGS
            .iter()
            .copied()
            .filter(|w| Which::all().contains(w))
            .collect();
        assert!(
            shared.len() >= 4,
            "fewer configurations share slugs with the matrix than this test \
             assumes; the group-prefix argument may no longer be the thing \
             protecting the baselines"
        );
    }

    /// Every configuration occupies every position equally often.
    ///
    /// Ported from the matrix's balance test rather than skipped, because the
    /// unbuffered arm's lack of one is a gap, not a precedent.
    #[test]
    fn every_configuration_gets_every_position_equally() {
        let combos = combinations();
        let count = CONFIGS.len();
        assert!(combos > 0, "with no combinations the loops below are empty");
        assert_eq!(
            combos % count,
            0,
            "the schedule only balances when the combination count is a whole \
             number of cycles; at {combos} combinations and {count} \
             configurations it is not"
        );
        let expected = combos / count;
        for position in 0..count {
            for config in CONFIGS {
                let seen = (0..combos)
                    .filter(|i| rotated_order(*i)[position] == config)
                    .count();
                assert_eq!(
                    seen, expected,
                    "{config:?} ran in position {position} {seen} times, not \
                     {expected}"
                );
            }
        }
    }

    /// The rotation turns, and turns the way it is supposed to.
    ///
    /// Balance alone does not pin direction: `rotate_right` is exactly as
    /// balanced as `rotate_left`. Direction has to be asserted in its own right
    /// or a flip goes unnoticed.
    #[test]
    fn the_rotation_turns_left() {
        assert_eq!(rotated_order(0), CONFIGS);
        assert_eq!(
            rotated_order(1)[0],
            CONFIGS[1],
            "the rotation is not turning left"
        );
        assert_eq!(
            rotated_order(CONFIGS.len()),
            CONFIGS,
            "the rotation does not return to its start after a full cycle"
        );
    }

    #[test]
    fn every_rotation_contains_every_configuration_once() {
        for index in 0..combinations() {
            let order = rotated_order(index);
            for config in CONFIGS {
                assert_eq!(
                    order.iter().filter(|c| **c == config).count(),
                    1,
                    "rotation {index} did not contain {config:?} exactly once"
                );
            }
        }
    }

    /// Depth 1 is present, because it is the negative control.
    #[test]
    fn the_negative_control_depth_is_present() {
        assert!(
            DEPTHS.contains(&1),
            "depth 1 is the negative control: without it the arm cannot \
             distinguish the predicted effect from run-level drift"
        );
        assert!(
            DEPTHS.iter().any(|d| *d > 1),
            "with only depth 1 the arm measures nothing the hypothesis predicts"
        );
    }

    /// The budget covers the arm, with the margin stated rather than assumed.
    #[test]
    fn the_arm_is_affordable_with_real_margin() {
        assert!(
            affordable(),
            "the handle-mode arm does not fit its budget: floor {:?} at {} \
             benchmarks against a budget of {HANDLE_MODE_RUN_BUDGET:?}",
            Budget::CHOSEN.floor(benchmarks()),
            benchmarks()
        );
        // A proposal that meets its threshold exactly is not meeting a
        // threshold. This asserts real headroom, not mere sufficiency.
        let required = Budget::CHOSEN.floor(benchmarks()) * 2;
        assert!(
            HANDLE_MODE_RUN_BUDGET >= required + Duration::from_secs(60),
            "the budget clears its requirement by less than 60 s ({required:?} \
             required, {HANDLE_MODE_RUN_BUDGET:?} set), which is the \
             zero-margin defect this arm's budget comment records rejecting \
             twice"
        );
    }

    /// The reference configurations are present.
    ///
    /// An A/B with no reference points cannot tell an unusual number caused by
    /// the effect from one caused by the host on the day.
    #[test]
    fn the_arm_carries_reference_configurations() {
        assert!(CONFIGS.contains(&Which::Compio));
        assert!(CONFIGS.contains(&Which::TokioOne));
    }

    /// Every cell in the grid is distinct.
    #[test]
    fn the_grid_has_no_duplicate_cells() {
        let cells = grid();
        for (i, a) in cells.iter().enumerate() {
            for b in &cells[i + 1..] {
                assert_ne!(a, b, "the grid contains {a:?} twice");
            }
        }
        assert_eq!(cells.len(), benchmarks());
    }
}
