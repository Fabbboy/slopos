//! Anti-starvation backstop for the priority run queue.
//!
//! Selection is strict fixed priority, which is the right default for a
//! kernel that has latency-critical tiers (`High`, `KernelIo`) whose whole
//! purpose is to pre-empt everything below them. What strict priority cannot
//! do on its own is bound the wait of a tier that is always passed over: a
//! `Normal` task that never blocks starves every `Low` task indefinitely, and
//! nothing in a pure priority scan ever changes that.
//!
//! The backstop is a *passed-over* count per tier. Each time selection skips a
//! non-empty tier in favour of a higher one, that tier's count rises; at
//! [`AGING_THRESHOLD`] the tier is served once and its count resets. The
//! guarantee this buys is the one the finding asks for and no more: a runnable
//! task at tier `t` waits at most `AGING_THRESHOLD` dispatches per tier above
//! it before the queue is obliged to pick `t`.
//!
//! Why this rather than a virtual-deadline scheduler: EEVDF orders one flat
//! pool of tasks by lag, and expressing "`KernelIo` must pre-empt `Normal`
//! because a TX ring is draining" inside that pool means weights so extreme
//! that the fair ordering is decorative. The tiers here are not preferences,
//! they are correctness statements. A backstop leaves those statements intact
//! and bounds the cost they impose on everyone below.

use core::cell::Cell;

/// Dispatches a non-empty tier may be passed over before it is served.
///
/// Chosen for the property it has to have rather than tuned: large enough that
/// an ordinary priority-respecting burst is unaffected (a `High` task that
/// wakes, runs and blocks does not approach it), small enough that the induced
/// latency on a starved tier stays in the millisecond range at a
/// millisecond-scale tick.
pub const AGING_THRESHOLD: u32 = 16;

/// Number of tiers the backstop tracks. Mirrors the run queue's level count.
pub const NUM_TIERS: usize = 5;

/// The first tier the backstop may hold back.
///
/// `High` and `KernelIo` are not preferences, they are correctness statements:
/// `KernelIo` runs the paths whose progress the rest of the kernel depends on
/// (delivering packets, draining TX rings, firing TCP retransmit timers), and
/// a `Low` task served ahead of one of those does not merely add latency, it
/// stalls the work that makes the machine answer at all. Aging bounds the wait
/// of the tiers that are *policy* — `Normal`, `Low`, `Idle` — and never
/// reorders the two above them.
pub const FIRST_AGEABLE_TIER: usize = 2;

/// Per-CPU passed-over counts, one per tier.
///
/// Not atomic and carrying no lock of its own: every method is called with the
/// owning queue's `queue_lock` held, which is also what serialises the
/// `ready_queues` these counts describe. A count that drifted from its queue
/// would only mis-schedule, never corrupt, but keeping both under one lock
/// means it cannot drift at all.
#[derive(Debug)]
pub struct AgingState {
    passed_over: [Cell<u32>; NUM_TIERS],
}

impl AgingState {
    pub const fn new() -> Self {
        Self {
            passed_over: [const { Cell::new(0) }; NUM_TIERS],
        }
    }

    pub fn reset(&self) {
        for slot in &self.passed_over {
            slot.set(0);
        }
    }

    /// The tier owed a dispatch, if any: the highest-priority tier whose
    /// passed-over count has reached the threshold.
    ///
    /// Highest-first so that when two tiers are both owed, the more urgent one
    /// is served first and the other stays owed rather than being reordered
    /// behind it.
    #[inline]
    pub fn tier_owed(&self, non_empty: &[bool; NUM_TIERS]) -> Option<usize> {
        // No debt may displace a privileged tier that has work.
        for tier in 0..FIRST_AGEABLE_TIER {
            if non_empty[tier] {
                return None;
            }
        }
        for tier in FIRST_AGEABLE_TIER..NUM_TIERS {
            if non_empty[tier] && self.passed_over[tier].get() >= AGING_THRESHOLD {
                return Some(tier);
            }
        }
        None
    }

    /// Record that `selected` was dispatched: every non-empty tier below it
    /// was passed over, and `selected` starts its own wait afresh.
    #[inline]
    pub fn note_dispatch(&self, selected: usize, non_empty: &[bool; NUM_TIERS]) {
        if selected < NUM_TIERS {
            self.passed_over[selected].set(0);
        }
        for tier in (selected + 1).max(FIRST_AGEABLE_TIER)..NUM_TIERS {
            if non_empty[tier] {
                let slot = &self.passed_over[tier];
                slot.set(slot.get().saturating_add(1));
            }
        }
    }

    #[inline]
    pub fn passed_over(&self, tier: usize) -> u32 {
        self.passed_over.get(tier).map_or(0, Cell::get)
    }
}

impl Default for AgingState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_starved_tier_is_eventually_owed() {
        let aging = AgingState::new();
        let non_empty = [false, false, true, true, false];
        for _ in 0..AGING_THRESHOLD {
            assert!(aging.tier_owed(&non_empty).is_none());
            aging.note_dispatch(2, &non_empty);
        }
        assert_eq!(aging.tier_owed(&non_empty), Some(3));
    }

    #[test]
    fn serving_the_owed_tier_clears_it() {
        let aging = AgingState::new();
        let non_empty = [false, false, true, true, false];
        for _ in 0..AGING_THRESHOLD {
            aging.note_dispatch(2, &non_empty);
        }
        aging.note_dispatch(3, &non_empty);
        assert!(aging.tier_owed(&non_empty).is_none());
    }

    #[test]
    fn a_privileged_tier_is_never_held_back() {
        let aging = AgingState::new();
        let non_empty = [false, true, false, true, false];
        for _ in 0..(AGING_THRESHOLD * 4) {
            aging.note_dispatch(1, &non_empty);
        }
        // Low has waited a long time, but KernelIo still has work: the strict
        // scan must keep winning, or packet delivery stalls behind background
        // work.
        assert_eq!(aging.tier_owed(&non_empty), None);

        // Once KernelIo drains, the debt Low accrued is honoured.
        let only_low = [false, false, false, true, false];
        assert_eq!(aging.tier_owed(&only_low), Some(3));
    }

    #[test]
    fn an_empty_tier_is_never_owed() {
        let aging = AgingState::new();
        let non_empty = [false, false, true, false, false];
        for _ in 0..(AGING_THRESHOLD * 4) {
            aging.note_dispatch(2, &non_empty);
        }
        assert!(aging.tier_owed(&non_empty).is_none());
    }
}
