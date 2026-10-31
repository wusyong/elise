//! How much the heap's thread marks at each step of a marking an engine
//! drives (V8's and cppgc's `heap::base::IncrementalMarkingSchedule`): the
//! marking is meant to take `MARKING_TIME`, so by now the threads should
//! have marked that share of what lives; a step marks what they are
//! behind, at least `min_step`. The engine's step bounds it in time too,
//! and the workers' share counts as they go.
//!
//! In objects, not bytes: the marker counts objects only (counting bytes
//! slows it). An object's bytes on average, from the last GC, turns the
//! heap's used bytes into the objects it should find, and cppgc's 64 KiB
//! into the step it makes when no progress is being made.

use std::time::Duration;

/// How long a marking should take (V8's `kEstimatedMarkingTime`).
pub(super) const MARKING_TIME: Duration = Duration::from_millis(500);

/// The bytes a step marks at least while marking makes no progress
/// (V8's `kStepSizeWhenNotMakingProgress`).
const STEP_BYTES_WITHOUT_PROGRESS: usize = 64 << 10;

/// An object's bytes on average before the heap's first GC measures it:
/// a DOM node's slot.
const DEFAULT_OBJECT_BYTES: usize = 64;

pub(super) struct Schedule {
    /// The bytes of an object on average, at least 1.
    object_bytes: usize,
    /// What the threads had marked at the last step.
    last_marked: usize,
}

impl Schedule {
    /// A marking's schedule, from the last GC's marked objects and their
    /// bytes (0 before the first).
    pub(super) fn new(last_objects: usize, last_bytes: usize) -> Self {
        let object_bytes = last_bytes
            .checked_div(last_objects)
            .filter(|&bytes| bytes > 0)
            .unwrap_or(DEFAULT_OBJECT_BYTES);
        Self {
            object_bytes,
            last_marked: 0,
        }
    }

    /// The objects the heap's thread marks at this step, `elapsed` into the
    /// marking, the heap using `used` bytes, the threads having marked
    /// `marked` objects so far.
    pub(super) fn step(&mut self, elapsed: Duration, used: usize, marked: usize) -> usize {
        // `object_bytes` is at least 1.
        let objects = |bytes: usize| bytes.checked_div(self.object_bytes).unwrap_or(bytes);
        let without_progress = objects(STEP_BYTES_WITHOUT_PROGRESS).max(1);
        let last = std::mem::replace(&mut self.last_marked, marked);
        if marked.saturating_sub(last) < without_progress {
            return without_progress;
        }
        let live = objects(used);
        // live × elapsed / MARKING_TIME, all of it once that is past.
        let expected = if elapsed >= MARKING_TIME {
            live
        } else {
            let share = elapsed
                .as_nanos()
                .saturating_mul(live as u128)
                .checked_div(MARKING_TIME.as_nanos())
                .unwrap_or(0);
            usize::try_from(share).unwrap_or(usize::MAX)
        };
        // Ahead of it: nothing (V8's minimum step is none).
        expected.saturating_sub(marked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn a_step_catches_up_with_the_share_due_by_now() {
        // 100 bytes an object; 100 MB used: a million objects to mark in
        // 500 ms, 2000 a millisecond.
        let mut schedule = Schedule::new(1000, 100_000);
        // The first step: no progress yet, a 64 KiB step.
        assert_eq!(schedule.step(MS, 100_000_000, 0), 655);
        // 10 ms in, 20 000 due; 5000 marked.
        assert_eq!(schedule.step(10 * MS, 100_000_000, 5000), 15_000);
        // Ahead of the share due: nothing.
        assert_eq!(schedule.step(11 * MS, 100_000_000, 30_000), 0);
        // Past the marking's time, all of it is due.
        assert_eq!(schedule.step(600 * MS, 100_000_000, 40_000), 960_000);
    }

    #[test]
    fn marking_that_makes_no_progress_steps_by_64_kib() {
        let mut schedule = Schedule::new(1000, 100_000);
        assert_eq!(schedule.step(MS, 100_000_000, 0), 655);
        assert_eq!(schedule.step(400 * MS, 100_000_000, 600), 655);
    }

    #[test]
    fn before_the_first_gc_an_object_is_a_node() {
        let mut schedule = Schedule::new(0, 0);
        assert_eq!(schedule.step(MS, 1 << 20, 0), 1024);
    }
}
