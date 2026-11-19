//! Concurrency slot accounting.
//!
//! A run holds a slot only while its sandbox is running or being provisioned. [`Slots`] is the
//! one place that counts them, so the engine and the M2 `join` (a parent waiting on children
//! releases its slot and reacquires it afterwards) share the same rules.

// The engine does not use Slots yet (tests come first).
#![allow(dead_code)]

use std::collections::HashSet;

use kestrel_types::RunId;

/// At most `cap` runs hold a slot at once. Each run holds at most one.
#[derive(Debug, Clone)]
pub(crate) struct Slots {
    cap: usize,
    held: HashSet<RunId>,
}

impl Slots {
    /// A pool of `cap` slots, all free.
    pub(crate) fn new(cap: usize) -> Slots {
        let _ = cap;
        todo!("Slots::new")
    }

    /// The configured number of slots.
    pub(crate) fn cap(&self) -> usize {
        todo!("Slots::cap")
    }

    /// Slots currently held.
    pub(crate) fn used(&self) -> usize {
        todo!("Slots::used")
    }

    /// Slots not held (`cap - used`).
    pub(crate) fn free(&self) -> usize {
        todo!("Slots::free")
    }

    /// Whether `run` holds a slot.
    pub(crate) fn holds(&self, run: RunId) -> bool {
        let _ = run;
        todo!("Slots::holds")
    }

    /// Takes a slot for `run`. True if `run` now holds one: it got a free slot, or it already
    /// held one (idempotent, never takes a second). False, changing nothing, when the pool is
    /// full and `run` holds none.
    pub(crate) fn try_acquire(&mut self, run: RunId) -> bool {
        let _ = run;
        todo!("Slots::try_acquire")
    }

    /// Takes a slot back for a run that released it earlier (a parent after `join`). Same rules
    /// as [`Slots::try_acquire`]; the separate name documents the intent at the call site.
    pub(crate) fn reacquire(&mut self, run: RunId) -> bool {
        let _ = run;
        todo!("Slots::reacquire")
    }

    /// Frees `run`'s slot. True if it held one; false, changing nothing, if it did not (so a
    /// double release can never push the free count above the cap).
    pub(crate) fn release(&mut self, run: RunId) -> bool {
        let _ = run;
        todo!("Slots::release")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn ids(n: usize) -> Vec<RunId> {
        (0..n).map(|_| RunId::new()).collect()
    }

    #[test]
    fn s1_new_pool_is_all_free() {
        let s = Slots::new(3);
        assert_eq!((s.cap(), s.used(), s.free()), (3, 0, 3));
        assert!(!s.holds(RunId::new()));
    }

    #[test]
    fn s1_acquire_up_to_the_cap_then_refuse_without_changing_anything() {
        let r = ids(3);
        let mut s = Slots::new(2);
        assert!(s.try_acquire(r[0]));
        assert!(s.try_acquire(r[1]));
        assert!(!s.try_acquire(r[2]));
        assert!(!s.holds(r[2]));
        assert_eq!((s.used(), s.free()), (2, 0));
    }

    #[test]
    fn s1_acquire_is_idempotent_for_a_holder() {
        let r = ids(2);
        let mut s = Slots::new(2);
        assert!(s.try_acquire(r[0]));
        assert!(s.try_acquire(r[0]), "a holder asking again still holds");
        assert_eq!(s.used(), 1, "never takes a second slot");
        assert!(s.try_acquire(r[1]), "the other slot is still free");
    }

    #[test]
    fn s1_a_holder_keeps_its_slot_when_the_pool_is_full() {
        let r = ids(2);
        let mut s = Slots::new(1);
        assert!(s.try_acquire(r[0]));
        assert!(s.try_acquire(r[0]));
        assert!(!s.try_acquire(r[1]));
    }

    #[test]
    fn s1_release_frees_the_slot_for_someone_else() {
        let r = ids(2);
        let mut s = Slots::new(1);
        assert!(s.try_acquire(r[0]));
        assert!(!s.try_acquire(r[1]));
        assert!(s.release(r[0]));
        assert!(!s.holds(r[0]));
        assert!(s.try_acquire(r[1]));
    }

    #[test]
    fn s7_release_is_idempotent_and_never_pushes_free_above_the_cap() {
        let r = ids(2);
        let mut s = Slots::new(2);
        assert!(s.try_acquire(r[0]));
        assert!(s.release(r[0]));
        assert!(!s.release(r[0]), "second release is a no-op");
        assert!(
            !s.release(r[1]),
            "releasing a run that never held one is a no-op"
        );
        assert_eq!((s.used(), s.free()), (0, 2));
        // Two real holders still cap at two after the stray releases.
        assert!(s.try_acquire(r[0]));
        assert!(s.try_acquire(r[1]));
        assert!(!s.try_acquire(RunId::new()));
    }

    #[test]
    fn s7_double_release_of_one_run_does_not_free_another_runs_slot() {
        let r = ids(3);
        let mut s = Slots::new(2);
        assert!(s.try_acquire(r[0]));
        assert!(s.try_acquire(r[1]));
        assert!(s.release(r[0]));
        assert!(!s.release(r[0]));
        assert!(s.try_acquire(r[2]));
        assert!(
            !s.try_acquire(r[0]),
            "r1 still holds its slot, r3 took r0's"
        );
        assert!(s.holds(r[1]));
    }

    #[test]
    fn s6_reacquire_after_release_takes_a_slot_again() {
        let r = ids(2);
        let mut s = Slots::new(1);
        assert!(s.try_acquire(r[0]));
        assert!(s.release(r[0]));
        assert!(s.try_acquire(r[1]));
        assert!(!s.reacquire(r[0]), "the pool is full, the parent must wait");
        assert!(s.release(r[1]));
        assert!(s.reacquire(r[0]));
        assert!(s.holds(r[0]));
        assert!(s.reacquire(r[0]), "reacquire by a holder is idempotent");
        assert_eq!(s.used(), 1);
    }

    #[test]
    fn s6_a_zero_cap_pool_never_hands_out_a_slot() {
        let mut s = Slots::new(0);
        assert!(!s.try_acquire(RunId::new()));
        assert_eq!((s.used(), s.free()), (0, 0));
    }

    /// The M2 deadlock, on the accounting alone: with every slot held by a parent that waits
    /// for a queued child, the children can only start if the parents give their slots up.
    fn join_fan_out(cap: usize) {
        let parents = ids(cap);
        let children = ids(cap);
        let mut s = Slots::new(cap);
        for p in &parents {
            assert!(s.try_acquire(*p));
        }
        for c in &children {
            assert!(
                !s.try_acquire(*c),
                "children are stuck while parents hold on"
            );
        }
        // Each parent calls join: it releases its slot and parks.
        for p in &parents {
            assert!(s.release(*p));
        }
        for c in &children {
            assert!(s.try_acquire(*c), "a child starts once a parent parked");
        }
        // The parents wake up with their children's results but find the pool full.
        for p in &parents {
            assert!(!s.reacquire(*p));
        }
        for (c, p) in children.iter().zip(&parents) {
            assert!(s.release(*c), "child finishes");
            assert!(s.reacquire(*p), "the parent takes the freed slot");
        }
        for p in &parents {
            assert!(s.release(*p));
        }
        assert_eq!(s.free(), cap);
    }

    #[test]
    fn s6_two_parents_each_waiting_on_one_queued_child_under_cap_two_all_finish() {
        join_fan_out(2);
    }

    #[test]
    fn s6_four_parents_fanning_out_under_cap_four_all_finish() {
        join_fan_out(4);
    }

    #[derive(Debug, Clone, Copy)]
    enum Op {
        Acquire(usize),
        Reacquire(usize),
        Release(usize),
    }

    fn op() -> impl Strategy<Value = Op> {
        (0usize..3, 0usize..8).prop_map(|(k, i)| match k {
            0 => Op::Acquire(i),
            1 => Op::Reacquire(i),
            _ => Op::Release(i),
        })
    }

    proptest! {
        /// Any mix of submit (acquire), resume (reacquire), suspend and finish (release,
        /// possibly twice) keeps the pool within its cap and agrees with a plain set model.
        #[test]
        fn s8_the_cap_is_never_exceeded_over_a_random_mix(
            cap in 0usize..5,
            ops in proptest::collection::vec(op(), 0..200),
        ) {
            let runs = ids(8);
            let mut slots = Slots::new(cap);
            let mut model: HashSet<RunId> = HashSet::new();
            for op in ops {
                match op {
                    Op::Acquire(i) | Op::Reacquire(i) => {
                        let r = runs[i];
                        let expect = model.contains(&r) || model.len() < cap;
                        let got = if matches!(op, Op::Acquire(_)) {
                            slots.try_acquire(r)
                        } else {
                            slots.reacquire(r)
                        };
                        prop_assert_eq!(got, expect);
                        if expect {
                            model.insert(r);
                        }
                    }
                    Op::Release(i) => {
                        let r = runs[i];
                        prop_assert_eq!(slots.release(r), model.remove(&r));
                    }
                }
                prop_assert!(slots.used() <= cap);
                prop_assert_eq!(slots.used(), model.len());
                prop_assert_eq!(slots.free(), cap - model.len());
                for r in &runs {
                    prop_assert_eq!(slots.holds(*r), model.contains(r));
                }
            }
        }
    }
}
