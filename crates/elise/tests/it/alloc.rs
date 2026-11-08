//! Allocation's fast path: in a heap with no nursery, a bump of its size
//! class's cursor and its header; the cursor's range counted as it is
//! taken, its tail taken back where the count must be exact; no site
//! looked at.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

use elise::{
    Gc, GcObject, Heap, HeapOptions, HeapVector, LocalExternal, Member, Persistent, Site,
    ThreadPool,
};

static SITE: Site = Site::new();

#[derive(GcObject)]
struct Plain {
    next: Member<Plain>,
    value: Cell<u32>,
}

/// With drop glue: its drops counted.
#[derive(GcObject)]
struct Dropped {
    drops: LocalExternal<Drops>,
}

struct Drops(Rc<Cell<u32>>);

/// Its drop's thread recorded: a `LocalExternal`'s, so the heap's.
#[derive(GcObject)]
struct Recorded {
    threads: LocalExternal<Recorder>,
}

struct Recorder(Arc<Mutex<Vec<ThreadId>>>);

impl Drop for Recorder {
    fn drop(&mut self) {
        self.0.lock().unwrap().push(std::thread::current().id());
    }
}

impl Drop for Drops {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

fn no_nursery() -> Heap {
    Heap::new(HeapOptions {
        nursery_bytes: 0,
        ..crate::verified()
    })
}

/// Objects of one size class take the slow path only as their cursor
/// takes a new range; what the heap uses is their slots, exactly, not the
/// rest of the range.
#[test]
fn a_heap_with_no_nursery_allocates_by_a_bump() {
    let mut heap = no_nursery();
    let before = heap.stats();
    heap.mutate(|cx| {
        cx.alloc(PlainInit {
            next: None,
            value: Cell::new(0),
        });
    });
    let slot = heap.used_bytes() - before.used_bytes;
    assert!(slot > 0 && slot <= 64, "one slot, not its range: {slot}");
    let root = heap.mutate(|cx| {
        let mut last: Option<Gc<'_, Plain>> = None;
        for value in 0..10_000 {
            last = Some(cx.alloc(PlainInit {
                next: last,
                value: Cell::new(value),
            }));
        }
        Persistent::new(cx, last.unwrap())
    });
    let stats = heap.stats();
    assert_eq!(stats.used_bytes - before.used_bytes, 10_001 * slot);
    let ranges = stats.old_ranges - before.old_ranges;
    assert!(ranges >= 1);
    assert_eq!(
        stats.slow_allocations - before.slow_allocations,
        ranges,
        "the slow path only for a new range"
    );
    heap.collect();
    heap.mutate(|cx| assert_eq!(root.get(cx).value.get(), 9_999));
    assert_eq!(
        heap.stats().used_bytes,
        10_000 * slot,
        "the one dropped freed"
    );
}

/// A heap with no nursery decides nothing by site: it neither looks up nor
/// counts them; one with a nursery counts its objects by their site.
#[test]
fn a_heap_with_no_nursery_counts_no_site() {
    let mut heap = no_nursery();
    heap.mutate(|cx| {
        for _ in 0..100 {
            cx.alloc_in(
                &SITE,
                PlainInit {
                    next: None,
                    value: Cell::new(0),
                },
            );
        }
    });
    let stats = heap.site(&SITE);
    assert_eq!((stats.created, stats.entered_old), (0, 0));
    let mut heap = Heap::new(crate::verified());
    heap.mutate(|cx| {
        for _ in 0..100 {
            cx.alloc_in(
                &SITE,
                PlainInit {
                    next: None,
                    value: Cell::new(0),
                },
            );
        }
    });
    assert_eq!(heap.site(&SITE).created, 100);
}

/// Objects with drop glue from the fast path are finalized once, as they
/// die: their page counts them (the verifier checks the counts against
/// the bits).
#[test]
fn the_fast_paths_objects_with_drop_glue_are_finalized_once() {
    let drops = Rc::default();
    let mut heap = no_nursery();
    let kept = heap.mutate(|cx| {
        for _ in 0..1_000 {
            cx.alloc(DroppedInit {
                drops: LocalExternal::new(Drops(Rc::clone(&drops))),
            });
        }
        Persistent::new(
            cx,
            cx.alloc(DroppedInit {
                drops: LocalExternal::new(Drops(Rc::clone(&drops))),
            }),
        )
    });
    assert_eq!(heap.stats().slow_allocations, heap.stats().old_ranges);
    heap.collect();
    assert_eq!(drops.get(), 1_000);
    heap.collect();
    assert_eq!(drops.get(), 1_000, "never twice");
    drop(kept);
    heap.collect();
    assert_eq!(drops.get(), 1_001);
}

/// A vector and its store (`HeapVector::new`) take the fast path too.
#[test]
fn a_vector_is_allocated_by_a_bump() {
    let mut heap = no_nursery();
    let before = heap.stats();
    let _vectors = heap.mutate(|cx| {
        (0..1_000)
            .map(|_| Persistent::new(cx, HeapVector::<Plain>::new(cx, 2)))
            .collect::<Vec<_>>()
    });
    let stats = heap.stats();
    assert_eq!(
        stats.slow_allocations - before.slow_allocations,
        stats.old_ranges - before.old_ranges,
        "the slow path only for a new range"
    );
}

/// The fast path's objects whose drop glue is the heap thread's
/// (`LocalExternal`) are finalized there, though the sweep runs on a
/// pool's workers: their page counts them as the heap thread's.
#[test]
fn the_fast_paths_local_drops_run_on_the_heaps_thread() {
    let mut heap = Heap::with_platform(
        HeapOptions {
            min_allocation_bytes: 64 << 10,
            min_overshoot_bytes: 0,
            nursery_bytes: 0,
            ..crate::verified()
        },
        Arc::new(ThreadPool::new(2)),
    );
    let threads = Arc::new(Mutex::new(Vec::new()));
    heap.mutate(|cx| {
        // Its range taken by an object of the same size class with no drop
        // glue (the slow path's `publish` counts its own object): the rest
        // on the page from the fast path alone.
        cx.alloc(PlainInit {
            next: None,
            value: Cell::new(0),
        });
        for _ in 0..100 {
            cx.alloc(RecordedInit {
                threads: LocalExternal::new(Recorder(threads.clone())),
            });
        }
    });
    // A marking started and finished, then its sweep done alongside.
    while !heap.is_marking() {
        heap.mutate(|cx| {
            for _ in 0..100 {
                cx.alloc(PlainInit {
                    next: None,
                    value: Cell::new(0),
                });
            }
        });
    }
    // Under Miri, a hundred times slower, and slower still beside other
    // tests, minutes.
    let limit = if cfg!(miri) { 600 } else { 10 };
    let started = std::time::Instant::now();
    while heap.is_marking() || heap.is_sweeping() {
        assert!(
            started.elapsed().as_secs() < limit,
            "the workers mark and sweep"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
        heap.mutate(|_| {});
    }
    let threads = threads.lock().unwrap();
    assert_eq!(threads.len(), 100);
    let here = std::thread::current().id();
    assert!(threads.iter().all(|&thread| thread == here), "{threads:?}");
}
