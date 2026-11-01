//! The marking worklist (V8's and cppgc's `heap::base::Worklist`): each
//! thread pushes and pops a local pair of segments, and publishes a full
//! one to a global list, which idle threads take from whole. A thread
//! takes the lock only to publish or take a segment, once every
//! `SEGMENT` items.
//!
//! The joining thread (the one that started the work) starts with the
//! first items, as local ones: no other thread is woken until it publishes
//! a segment, so a small graph is marked on it alone.
//!
//! Termination: `busy` counts the threads holding items. A thread that
//! runs out of local items and finds no segment stops being busy; the
//! work is done once none is busy and no segment is left, since only a
//! busy thread makes new items. The joining thread (the one that started
//! the work) waits for that; the others return as soon as they find
//! nothing, and run again if more is published.
//!
//! Concurrent marking's workers run without a joining thread
//! (`run_background`): they take segments until none is left or they are
//! told to yield, then publish what they hold. Nothing waits for them;
//! the heap decides at a safepoint that marking is done, and finishes it
//! with a joining run (`rejoin`). The mutator's barrier pushes onto a
//! local list of its own (`Local::new`), published when full and at
//! safepoints (`flush`).

use crate::sync::{self, AtomicUsize, Condvar, Mutex, Ordering};

/// Items a segment holds: cppgc's marking worklist's. A smaller one hands
/// workers pieces of narrow graphs (a chain of small trees) that take less
/// marking than taking them does. Two under loom and eight under shuttle,
/// so that their small models publish and take segments.
pub(crate) const SEGMENT: usize = if cfg!(loom) {
    2
} else if cfg!(shuttle) {
    8
} else {
    512
};

/// How many items a posted job's run visits between two looks at whether
/// it should yield.
const YIELD_EVERY: usize = if cfg!(any(loom, shuttle)) { 1 } else { 64 };

pub(crate) struct Worklist {
    /// The joining thread's first items.
    seed: Mutex<Option<Vec<usize>>>,
    global: Mutex<Global>,
    /// The joining thread waits on it for a segment or the end.
    changed: Condvar,
    /// Segments in `global`, read without the lock by `max_concurrency`.
    available: AtomicUsize,
}

#[derive(Default)]
struct Global {
    segments: Vec<Vec<usize>>,
    busy: usize,
    done: bool,
    /// A thread panicked while busy: the joining thread stops waiting.
    poisoned: bool,
}

/// A thread's items: it pushes to `push`, and pops from `pop`, then from
/// `push`, then from the global list.
pub(crate) struct Local {
    push: Vec<usize>,
    pop: Vec<usize>,
    /// Segments it published, to tell the platform.
    published: bool,
}

impl Local {
    /// A local list for a thread outside a run: the mutator's barrier.
    pub(crate) fn new() -> Self {
        Self {
            push: Vec::with_capacity(SEGMENT),
            pop: Vec::new(),
            published: false,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.push.is_empty() && self.pop.is_empty()
    }

    /// Whether it published a segment since last asked.
    pub(crate) fn take_published(&mut self) -> bool {
        std::mem::take(&mut self.published)
    }

    pub(crate) fn push(&mut self, worklist: &Worklist, item: usize) {
        self.push.push(item);
        if self.push.len() >= SEGMENT {
            let full = std::mem::replace(&mut self.push, Vec::with_capacity(SEGMENT));
            worklist.publish(full);
            self.published = true;
        }
    }

    fn pop(&mut self) -> Option<usize> {
        if self.pop.is_empty() {
            std::mem::swap(&mut self.pop, &mut self.push);
        }
        self.pop.pop()
    }
}

impl Worklist {
    /// A worklist whose joining thread starts with `seed`: it is busy from
    /// the start.
    pub(crate) fn new(seed: Vec<usize>) -> Self {
        Self {
            seed: Mutex::new(Some(seed)),
            available: AtomicUsize::new(0),
            global: Mutex::new(Global {
                busy: 1,
                ..Global::default()
            }),
            changed: Condvar::new(),
        }
    }

    /// A worklist whose joining thread starts with `seed`, and with `items`
    /// published as segments for any thread to take.
    pub(crate) fn with_published(seed: Vec<usize>, items: Vec<usize>) -> Self {
        let segments: Vec<Vec<usize>> = items.chunks(SEGMENT).map(<[usize]>::to_vec).collect();
        Self {
            seed: Mutex::new(Some(seed)),
            available: AtomicUsize::new(segments.len()),
            global: Mutex::new(Global {
                segments,
                busy: 1,
                ..Global::default()
            }),
            changed: Condvar::new(),
        }
    }

    /// Segments to take: how many more threads could work.
    pub(crate) fn available(&self) -> usize {
        self.available.load(Ordering::Relaxed)
    }

    /// Publishes whatever `local` holds.
    pub(crate) fn flush(&self, local: &mut Local) {
        for segment in [&mut local.push, &mut local.pop] {
            if !segment.is_empty() {
                self.publish(std::mem::replace(segment, Vec::with_capacity(SEGMENT)));
            }
        }
    }

    /// A segment to take, if any, without becoming busy.
    fn take_any(&self) -> Option<Vec<usize>> {
        let mut global = sync::lock(&self.global);
        let segment = global.segments.pop();
        self.available
            .store(global.segments.len(), Ordering::Relaxed);
        segment
    }

    /// A posted job's run: visits items of the segments it takes until none
    /// is left, or until `should_yield` (asked every `YIELD_EVERY` items),
    /// when it publishes what it holds; `published` tells the platform. It
    /// is never counted busy: nothing waits for it. Whether it ran out of
    /// segments: asked to yield with the last of them, it did.
    pub(crate) fn run_background(
        &self,
        should_yield: &dyn Fn() -> bool,
        published: &dyn Fn(),
        mut visit: impl FnMut(usize, &mut Local, &Self),
    ) -> bool {
        let mut visited = 0usize;
        while let Some(segment) = self.take_any() {
            let mut local = Local {
                push: Vec::with_capacity(SEGMENT),
                pop: segment,
                published: false,
            };
            while let Some(item) = local.pop() {
                visit(item, &mut local, self);
                if local.take_published() {
                    published();
                }
                visited = visited.wrapping_add(1);
                if visited.is_multiple_of(YIELD_EVERY) && should_yield() {
                    // Out of segments with this item: drained, not stopped,
                    // so the caller still turns to what a drain gives (the
                    // ephemerons whose key got marked).
                    if local.is_empty() && self.available() == 0 {
                        return true;
                    }
                    self.flush(&mut local);
                    return false;
                }
            }
        }
        true
    }

    /// The joining thread's next run (the end of a marking, again): it
    /// starts busy, with `items`, the work not done.
    pub(crate) fn reseed(&self, items: Vec<usize>) {
        *sync::lock(&self.seed) = Some(items);
        let mut global = sync::lock(&self.global);
        global.busy = 1;
        global.done = false;
    }

    /// Whether no segment is left and no background run (counted in
    /// `active`, before it takes one) is in progress. Read under the lock a
    /// run takes its segments under, so a run that took the last one is
    /// seen running.
    pub(crate) fn drained(&self, active: &AtomicUsize) -> bool {
        let global = sync::lock(&self.global);
        global.segments.is_empty() && active.load(Ordering::SeqCst) == 0
    }

    fn publish(&self, segment: Vec<usize>) {
        let mut global = sync::lock(&self.global);
        global.segments.push(segment);
        self.available
            .store(global.segments.len(), Ordering::Relaxed);
        drop(global);
        self.changed.notify_one();
    }

    /// Takes a segment, becoming busy: the joining thread waits for one or
    /// the end, another returns `None` when there is none.
    fn take(&self, joining: bool) -> Option<Vec<usize>> {
        let mut global = sync::lock(&self.global);
        loop {
            if global.done || global.poisoned {
                return None;
            }
            if let Some(segment) = global.segments.pop() {
                self.available
                    .store(global.segments.len(), Ordering::Relaxed);
                global.busy = global.busy.saturating_add(1);
                return Some(segment);
            }
            if !joining {
                return None;
            }
            if global.busy == 0 {
                // Nothing to take and no one to make more.
                global.done = true;
                self.changed.notify_all();
                return None;
            }
            global = sync::wait(&self.changed, global);
        }
    }

    /// Takes part in the work: `visit` handles an item, pushing what it
    /// finds onto the local list it is given; `published` tells the
    /// platform when the thread published a segment. On the joining
    /// thread, returns once all the work is done (or a thread panicked).
    pub(crate) fn run(
        &self,
        joining: bool,
        published: &dyn Fn(),
        mut visit: impl FnMut(usize, &mut Local, &Self),
    ) {
        // The seed, counted busy already.
        let mut seed = if joining {
            sync::lock(&self.seed).take()
        } else {
            None
        };
        while let Some(segment) = seed.take().or_else(|| self.take(joining)) {
            let guard = Busy(self);
            let mut local = Local {
                push: Vec::with_capacity(SEGMENT),
                pop: segment,
                published: false,
            };
            loop {
                while let Some(item) = local.pop() {
                    visit(item, &mut local, self);
                    if std::mem::take(&mut local.published) {
                        published();
                    }
                }
                // Out of local items: another segment keeps it busy.
                let mut global = sync::lock(&self.global);
                if let Some(segment) = global.segments.pop() {
                    self.available
                        .store(global.segments.len(), Ordering::Relaxed);
                    local.pop = segment;
                    continue;
                }
                global.busy = global.busy.saturating_sub(1);
                if global.busy == 0 {
                    global.done = true;
                    self.changed.notify_all();
                }
                break;
            }
            std::mem::forget(guard);
            if !joining {
                return;
            }
        }
    }

    /// Whether a thread panicked while busy.
    pub(crate) fn poisoned(&self) -> bool {
        sync::lock(&self.global).poisoned
    }
}

/// Unwinds a busy thread: the others stop waiting for it.
struct Busy<'a>(&'a Worklist);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        let mut global = sync::lock(&self.0.global);
        global.busy = global.busy.saturating_sub(1);
        global.poisoned = true;
        self.0.changed.notify_all();
    }
}

/// The items of a small binary tree, `0..count`: item `n`'s children are
/// `2n + 1` and `2n + 2`. Each is visited once, by whichever thread pops it.
#[cfg(test)]
fn visit_tree(
    count: usize,
    seen: &[crate::sync::AtomicBool],
) -> impl Fn(usize, &mut Local, &Worklist) + '_ {
    move |item, local, worklist| {
        assert!(
            !seen[item].swap(true, Ordering::Relaxed),
            "item {item} visited twice"
        );
        for child in [2 * item + 1, 2 * item + 2] {
            if child < count {
                local.push(worklist, child);
            }
        }
    }
}

#[cfg(all(test, not(any(loom, shuttle))))]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use super::*;

    #[test]
    fn every_item_is_visited_once_by_one_thread() {
        let seen: Vec<AtomicBool> = (0..1000).map(|_| AtomicBool::new(false)).collect();
        let worklist = Worklist::new(vec![0]);
        worklist.run(true, &|| {}, visit_tree(1000, &seen));
        assert!(seen.iter().all(|seen| seen.load(Ordering::Relaxed)));
        assert!(!worklist.poisoned());
    }

    #[test]
    fn threads_share_the_work_and_all_of_it_is_done() {
        let count = if cfg!(miri) { 2000 } else { 100_000 };
        let seen: Arc<Vec<AtomicBool>> =
            Arc::new((0..count).map(|_| AtomicBool::new(false)).collect());
        let worklist = Arc::new(Worklist::new(vec![0]));
        let helpers: Vec<_> = (0..3)
            .map(|_| {
                let (worklist, seen) = (worklist.clone(), seen.clone());
                std::thread::spawn(move || {
                    // A worker runs again while there are segments, as a
                    // platform does.
                    while !sync::lock(&worklist.global).done {
                        worklist.run(false, &|| {}, visit_tree(count, &seen));
                        std::thread::yield_now();
                    }
                })
            })
            .collect();
        worklist.run(true, &|| {}, visit_tree(count, &seen));
        // Done once the joining thread returns, whatever the others do.
        assert!(seen.iter().all(|seen| seen.load(Ordering::Relaxed)));
        for helper in helpers {
            helper.join().unwrap();
        }
    }

    /// A background run asked to yield stops at the next check, every
    /// `YIELD_EVERY` items, its rest published for the next run.
    #[test]
    fn a_background_run_yields_and_publishes_the_rest() {
        let worklist = Worklist::with_published(Vec::new(), (0..1000).collect());
        let mut visited = 0usize;
        worklist.run_background(&|| true, &|| {}, |_, _, _| visited += 1);
        assert_eq!(visited, YIELD_EVERY);
        let mut rest = 0usize;
        worklist.run_background(&|| false, &|| {}, |_, _, _| rest += 1);
        assert_eq!(visited + rest, 1000, "nothing lost");
    }

    /// A background run asked to yield with its last item has drained the
    /// worklist all the same.
    #[test]
    fn a_background_run_asked_to_yield_with_its_last_item_drained() {
        let worklist = Worklist::with_published(Vec::new(), (0..YIELD_EVERY).collect());
        let mut visited = 0usize;
        assert!(worklist.run_background(&|| true, &|| {}, |_, _, _| visited += 1));
        assert_eq!(visited, YIELD_EVERY);
    }

    /// A thread that panics while busy poisons the work: the joining thread
    /// stops waiting for it, rather than hang.
    #[test]
    fn a_panicking_thread_poisons_the_work() {
        let worklist = Arc::new(Worklist::new(Vec::new()));
        // A segment for another thread to take, and the joining thread busy.
        worklist.publish((0..SEGMENT).collect());
        let other = {
            let worklist = worklist.clone();
            std::thread::spawn(move || {
                worklist.run(false, &|| {}, |_, _, _| panic!("a visit's panic"))
            })
        };
        assert!(other.join().is_err());
        worklist.run(true, &|| {}, |_, _, _| {});
        assert!(worklist.poisoned());
    }
}

/// loom: every interleaving (up to three preemptions) of a joining thread
/// and two workers on a tree of fifteen items, with segments of two: each
/// item is visited exactly once, the joining thread returns only when all
/// are, and nothing deadlocks.
#[cfg(all(test, loom))]
mod loom_tests {
    use std::sync::atomic::{AtomicBool as StdBool, Ordering as StdOrdering};

    use loom::sync::Arc;
    use loom::sync::atomic::AtomicBool;

    use super::*;

    const ITEMS: usize = 15;
    const WORKERS: usize = 2;

    #[test]
    fn the_worklist_terminates_and_visits_each_item_once() {
        // Whether any interleaving had a worker visit an item (a model where
        // none does tests nothing of sharing), and how many it explored.
        static WORKER_VISITED: StdBool = StdBool::new(false);
        static RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = Some(3);
        builder.check(|| {
            RUNS.fetch_add(1, StdOrdering::Relaxed);
            let seen: Arc<Vec<AtomicBool>> =
                Arc::new((0..ITEMS).map(|_| AtomicBool::new(false)).collect());
            let worklist = Arc::new(Worklist::new(vec![0]));
            let workers: Vec<_> = (0..WORKERS)
                .map(|_| {
                    let (worklist, seen) = (worklist.clone(), seen.clone());
                    loom::thread::spawn(move || {
                        let visit = visit_tree(ITEMS, &seen);
                        worklist.run(false, &|| {}, |item, local, worklist| {
                            WORKER_VISITED.store(true, StdOrdering::Relaxed);
                            visit(item, local, worklist);
                        });
                    })
                })
                .collect();
            worklist.run(true, &|| {}, visit_tree(ITEMS, &seen));
            for item in seen.iter() {
                assert!(
                    item.load(Ordering::Relaxed),
                    "the joining thread returned before the end"
                );
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        assert!(
            WORKER_VISITED.load(StdOrdering::Relaxed),
            "no interleaving shared the work"
        );
        // About 60 000 at this size: fewer means the model shrank.
        assert!(
            RUNS.load(StdOrdering::Relaxed) > 10_000,
            "the model explores too few interleavings"
        );
    }

    /// A posted job's run takes the last segment while the heap asks
    /// whether marking is drained: it is only once the run has visited
    /// everything, the child it pushed too (the run is counted active
    /// before it takes a segment, as the marker's are).
    #[test]
    fn drained_waits_for_a_run_holding_the_last_segment() {
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = Some(3);
        builder.check(|| {
            let worklist = Arc::new(Worklist::with_published(Vec::new(), vec![0, 1]));
            let active = Arc::new(AtomicUsize::new(0));
            let visited = Arc::new(AtomicUsize::new(0));
            let worker = {
                let (worklist, active, visited) =
                    (worklist.clone(), active.clone(), visited.clone());
                loom::thread::spawn(move || {
                    active.fetch_add(1, Ordering::SeqCst);
                    worklist.run_background(&|| false, &|| {}, |item, local, worklist| {
                        visited.fetch_add(1, Ordering::Relaxed);
                        if item == 0 {
                            local.push(worklist, 2);
                        }
                    });
                    active.fetch_sub(1, Ordering::SeqCst);
                })
            };
            while !worklist.drained(&active) {
                loom::thread::yield_now();
            }
            assert_eq!(visited.load(Ordering::Relaxed), 3, "drained before the end");
            worker.join().unwrap();
        });
    }
}
