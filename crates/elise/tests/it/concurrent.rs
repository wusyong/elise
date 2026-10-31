//! Marking alongside the mutator. A platform of the tests' (`Stepping`)
//! keeps the posted job and runs it only when a test says, so that the
//! mutator acts at a known point of the marking: before the marker reached
//! an object, and with objects it never scans (black, allocated since).
//! The barrier keeps what the snapshot reached when its last edge is cut;
//! what is allocated while marking is black; promotion while marking is
//! black too, its young object's edges kept by the barrier; a marking
//! finishes once the job found nothing left, or at the old generation's
//! limit with the rest in the pause. Then the same on a real pool. Every
//! heap runs the verifier, which checks after each full GC that what the
//! roots reach is marked, and that what else is marked was in the snapshot
//! or allocated while marking.

use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use elise::{
    GcObject, Heap, HeapOptions, Job, JobDelegate, JobHandle, Member, Persistent, Platform,
    ThreadPool,
};

/// A platform whose posted jobs (a marking's, the discarding of pages)
/// run only in `step`.
#[derive(Default)]
pub(crate) struct Stepping {
    /// By the number each was posted with.
    posted: Mutex<Vec<(usize, Arc<dyn Job + Send>)>>,
    next: Mutex<usize>,
    /// The times the heap told a posted job's handle there is more to do.
    notified: AtomicUsize,
}

impl Stepping {
    /// Runs each posted job until it finds nothing to do.
    pub(crate) fn step(&self) {
        let jobs: Vec<_> = self
            .posted
            .lock()
            .unwrap()
            .iter()
            .map(|(_, job)| job.clone())
            .collect();
        for job in jobs {
            job.run(&Worker);
        }
    }

    /// The jobs posted, to run on a thread of the test's.
    pub(crate) fn jobs(&self) -> Vec<Arc<dyn Job + Send>> {
        self.posted
            .lock()
            .unwrap()
            .iter()
            .map(|(_, job)| job.clone())
            .collect()
    }

    /// The times the heap told a posted job's handle there is more to do
    /// (`JobHandle::notify_concurrency_increase`): woke the workers.
    pub(crate) fn notified(&self) -> usize {
        self.notified.load(Ordering::Relaxed)
    }

    /// Whether a posted job wants to run.
    pub(crate) fn posted(&self) -> bool {
        self.posted
            .lock()
            .unwrap()
            .iter()
            .any(|(_, job)| job.max_concurrency(0) > 0)
    }
}

struct Worker;

impl JobDelegate for Worker {
    fn notify_concurrency_increase(&self) {}

    fn is_joining_thread(&self) -> bool {
        false
    }
}

/// Its handle: cancelling takes the job back.
struct Handle(Arc<Stepping>, usize);

impl JobHandle for Handle {
    #[expect(clippy::disallowed_methods, reason = "tests' counters")]
    fn notify_concurrency_increase(&self) {
        self.0.notified.fetch_add(1, Ordering::Relaxed);
    }

    fn cancel(&self) {
        self.0
            .posted
            .lock()
            .unwrap()
            .retain(|&(id, _)| id != self.1);
    }
}

/// The heap's platform: an `Arc` of the test's `Stepping`.
struct Steps(Arc<Stepping>);

impl Platform for Steps {
    fn run(&self, job: &dyn Job) {
        elise::InlinePlatform.run(job);
    }

    fn post_job(&self, job: Arc<dyn Job + Send>) -> Option<Box<dyn JobHandle>> {
        let mut next = self.0.next.lock().unwrap();
        *next += 1;
        self.0.posted.lock().unwrap().push((*next, job));
        Some(Box::new(Handle(self.0.clone(), *next)))
    }

    fn posts_jobs(&self) -> bool {
        true
    }
}

#[derive(GcObject)]
struct Node {
    next: Member<Node>,
    value: Cell<u32>,
}

/// A heap on a `Stepping` platform: a full GC after 64 KiB (marking from
/// 32 KiB, finished in the pause past 96 KiB: the limit's half beyond),
/// with a nursery of `nursery_bytes`.
pub(crate) fn stepping_heap(nursery_bytes: usize) -> (Heap, Arc<Stepping>) {
    stepping_heap_with(HeapOptions {
        min_allocation_bytes: 64 << 10,
        min_overshoot_bytes: 0,
        nursery_bytes,
        ..crate::verified()
    })
}

pub(crate) fn stepping_heap_with(options: HeapOptions) -> (Heap, Arc<Stepping>) {
    let steps = Arc::new(Stepping::default());
    let heap = Heap::with_platform(options, Arc::new(Steps(steps.clone())));
    (heap, steps)
}

/// Garbage in the old generation until a marking starts.
pub(crate) fn start_marking(heap: &mut Heap) {
    for _ in 0..1000 {
        if heap.is_marking() {
            return;
        }
        heap.mutate(|cx| {
            cx.pretenured(|cx| {
                for _ in 0..100 {
                    node(cx, 0, None);
                }
            });
        });
    }
    panic!("no marking started");
}

fn node<'gc>(
    cx: &elise::Mutation<'gc>,
    value: u32,
    next: Option<elise::Gc<'gc, Node>>,
) -> elise::Gc<'gc, Node> {
    cx.alloc(NodeInit {
        next,
        value: Cell::new(value),
    })
}

/// A safepoint (which publishes what the barrier marked), the job run
/// through, and the marking finished at the next safepoint: concurrently,
/// as the heap's record says.
pub(crate) fn finish(heap: &mut Heap, steps: &Stepping) {
    heap.mutate(|_| {});
    steps.step();
    heap.mutate(|_| {});
    assert!(!heap.is_marking());
    assert!(heap.stats().last_gc.concurrent);
}

/// `a` → `x`, rooted at `a`, old.
fn old_pair(heap: &mut Heap) -> Persistent<Node> {
    heap.mutate(|cx| {
        cx.pretenured(|cx| {
            let x = node(cx, 7, None);
            Persistent::new(cx, node(cx, 1, Some(x)))
        })
    })
}

/// The snapshot reaches `x` only through `a`'s edge. While marking, before
/// the marker reaches `a`, the mutator moves `x` to a new object (black:
/// never scanned) and cuts `a`'s edge: the barrier marks `x`.
#[test]
fn the_barrier_keeps_what_the_snapshot_reached() {
    let (mut heap, steps) = stepping_heap(0);
    let a = old_pair(&mut heap);
    start_marking(&mut heap);
    assert!(steps.posted(), "the job is posted");
    let b = heap.mutate(|cx| {
        let a = a.get(cx);
        let b = node(cx, 2, a.next.get(cx));
        a.next.set(cx, None);
        Persistent::new(cx, b)
    });
    finish(&mut heap, &steps);
    heap.mutate(|cx| assert_eq!(b.get(cx).next.get(cx).unwrap().value.get(), 7));
}

/// What is allocated while marking is black: a new object that only
/// another new one points to survives, though the marker never sees it.
#[test]
fn what_is_allocated_while_marking_is_black() {
    let (mut heap, steps) = stepping_heap(0);
    start_marking(&mut heap);
    let b = heap.mutate(|cx| {
        let n = node(cx, 9, None);
        Persistent::new(cx, node(cx, 2, Some(n)))
    });
    finish(&mut heap, &steps);
    heap.mutate(|cx| assert_eq!(b.get(cx).next.get(cx).unwrap().value.get(), 9));
}

/// A young object points to an old one the snapshot reached only through
/// an edge then cut; a scavenge while marking promotes the young object
/// (black, never scanned): the barrier kept the old one. The marker
/// doesn't walk the young.
#[test]
fn promotion_while_marking_is_black_and_the_barrier_keeps_its_edges() {
    let (mut heap, steps) = stepping_heap(1 << 18);
    let a = old_pair(&mut heap);
    start_marking(&mut heap);
    let y = heap.mutate(|cx| {
        let a = a.get(cx);
        let y = node(cx, 3, a.next.get(cx));
        a.next.set(cx, None);
        Persistent::new(cx, y)
    });
    // Twice: copied, then promoted, both while marking.
    heap.scavenge();
    heap.scavenge();
    assert!(heap.is_marking());
    assert!(steps.posted(), "posted again after each scavenge");
    finish(&mut heap, &steps);
    heap.mutate(|cx| assert_eq!(y.get(cx).next.get(cx).unwrap().value.get(), 7));
}

/// A job that never runs: the old generation reaches its limit and half
/// of it beyond (the overshoot margin, its least 0 here), and the marking
/// is finished in the pause, all of it.
#[test]
fn at_the_limit_the_pause_finishes_the_marking() {
    let (mut heap, _steps) = stepping_heap(0);
    let a = old_pair(&mut heap);
    start_marking(&mut heap);
    let start = heap.stats().used_bytes;
    let gcs = heap.stats().gcs;
    let mut grown = 0;
    while heap.stats().gcs == gcs {
        grown = heap.stats().used_bytes - start;
        heap.mutate(|cx| {
            cx.pretenured(|cx| {
                for _ in 0..100 {
                    node(cx, 0, None);
                }
            });
        });
    }
    // From half the limit (64 KiB) to its half past it.
    assert!((56 << 10..64 << 10).contains(&grown), "{grown}");
    assert!(!heap.is_marking(), "finished");
    assert!(heap.stats().last_gc.concurrent);
    assert_eq!(
        (
            heap.stats().concurrent_markings,
            heap.stats().markings_at_limit
        ),
        (1, 1)
    );
    heap.mutate(|cx| assert_eq!(a.get(cx).next.get(cx).unwrap().value.get(), 7));
}

/// While the job hasn't run, the old generation grows past its limit by
/// the overshoot margin (here more than half the limit) before the pause
/// finishes the marking: the safepoints until then don't collect.
#[test]
fn a_marking_runs_past_the_limit_by_the_margin() {
    let (mut heap, _steps) = stepping_heap_with(HeapOptions {
        min_allocation_bytes: 64 << 10,
        min_overshoot_bytes: 256 << 10,
        nursery_bytes: 0,
        ..crate::verified()
    });
    let a = old_pair(&mut heap);
    start_marking(&mut heap);
    let start = heap.stats().used_bytes;
    let gcs = heap.stats().gcs;
    let mut grown = 0;
    while heap.stats().gcs == gcs {
        grown = heap.stats().used_bytes - start;
        if (128 << 10..256 << 10).contains(&grown) {
            // Past the limit (64 KiB, the heap being smaller) and its half.
            assert!(heap.is_marking() && !heap.should_collect(), "{grown}");
        }
        heap.mutate(|cx| {
            cx.pretenured(|cx| {
                for _ in 0..100 {
                    node(cx, 0, None);
                }
            });
        });
    }
    // The limit and the margin, less what of the limit was allocated before
    // the marking started.
    assert!((256 << 10..(64 + 256) << 10).contains(&grown), "{grown}");
    assert!(!heap.is_marking(), "finished");
    assert_eq!(heap.stats().markings_at_limit, 1);
    heap.mutate(|cx| assert_eq!(a.get(cx).next.get(cx).unwrap().value.get(), 7));
}

/// About 16 KiB: a few hundred fill megabytes.
#[cfg(miri)]
#[derive(GcObject)]
struct Wide {
    words: [Cell<u64>; 2000],
}

/// Under Miri the cage is 16 MiB: the default margin (32 MiB, V8's
/// `kMinOvershoot`) is capped at half of what the cage has left, so the
/// pause finishes the marking well before the cage is full.
#[cfg(miri)]
#[test]
fn the_margin_is_at_most_half_of_the_cage_left() {
    let (mut heap, _steps) = stepping_heap_with(HeapOptions {
        min_allocation_bytes: 64 << 10,
        nursery_bytes: 0,
        ..crate::verified()
    });
    let a = old_pair(&mut heap);
    start_marking(&mut heap);
    let start = heap.stats().used_bytes;
    let gcs = heap.stats().gcs;
    let mut grown = 0;
    while heap.stats().gcs == gcs {
        grown = heap.stats().used_bytes - start;
        heap.mutate(|cx| {
            cx.pretenured(|cx| {
                for _ in 0..16 {
                    cx.alloc(WideInit {
                        // Not `from_fn`: a call for each word, most of
                        // the test's time under Miri.
                        words: [const { Cell::new(0) }; 2000],
                    });
                }
            });
        });
    }
    // Half of the 16 MiB left, less the heap's own, and the limit.
    assert!((7 << 20..9 << 20).contains(&grown), "{grown}");
    assert_eq!(heap.stats().markings_at_limit, 1);
    heap.mutate(|cx| assert_eq!(a.get(cx).next.get(cx).unwrap().value.get(), 7));
}

/// Once the job found nothing left, the next safepoint finishes.
#[test]
fn a_marking_finishes_at_the_safepoint_after_it_is_done() {
    let (mut heap, steps) = stepping_heap(0);
    let a = old_pair(&mut heap);
    start_marking(&mut heap);
    assert!(!heap.should_collect() || heap.stats().young_bytes > 0);
    steps.step();
    assert!(heap.should_collect(), "done: the next safepoint finishes");
    heap.mutate(|_| {});
    assert!(!heap.is_marking());
    // The pair: the garbage is unmarked.
    assert_eq!(heap.stats().last_gc.marked_objects, 2);
    assert_eq!(
        (
            heap.stats().concurrent_markings,
            heap.stats().markings_at_limit
        ),
        (1, 0)
    );
    assert!(
        heap.stats().background_mark_ns > 0,
        "the job's run is timed"
    );
    drop(a);
}

/// On a pool: markings start at half the limit, run on its workers while
/// the mutator builds lists in the old generation and cuts the last, and
/// finish; what the list keeps stays, checked by the verifier after each.
#[test]
fn markings_run_alongside_on_a_pool() {
    let mut heap = Heap::with_platform(
        HeapOptions {
            min_allocation_bytes: 64 << 10,
            ..crate::verified()
        },
        Arc::new(ThreadPool::new(2)),
    );
    let head = heap.mutate(|cx| Persistent::new(cx, node(cx, 0, None)));
    let mut concurrent = 0;
    for round in 0..200u32 {
        heap.mutate(|cx| {
            let head = head.get(cx);
            // A new list of 100 behind the head, the old one garbage.
            let mut next = None;
            cx.pretenured(|cx| {
                for i in 0..100 {
                    next = Some(node(cx, round * 100 + i, next));
                }
            });
            head.next.set(cx, next);
        });
        if heap.stats().last_gc.concurrent {
            concurrent += 1;
        }
    }
    // The rounds take milliseconds, and a marking ends only once the
    // workers are done (the overshoot margin is 32 MiB away): on a busy
    // machine they may not have run yet. Safepoints until one is done, a
    // while, not forever.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while concurrent == 0 && heap.is_marking() && std::time::Instant::now() < deadline {
        heap.mutate(|_| {});
        if heap.stats().last_gc.concurrent {
            concurrent += 1;
        }
        std::thread::yield_now();
    }
    heap.collect();
    assert!(concurrent > 0, "no marking ran alongside");
    heap.mutate(|cx| {
        let mut values = Vec::new();
        let mut next = head.get(cx).next.get(cx);
        while let Some(node) = next {
            values.push(node.value.get());
            next = node.next.get(cx);
        }
        assert_eq!(
            values,
            (0..100).rev().map(|i| 199 * 100 + i).collect::<Vec<_>>()
        );
    });
}
