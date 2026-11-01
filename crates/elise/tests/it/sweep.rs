//! The sweep alongside the mutator, after a concurrent marking. On the
//! tests' `Stepping` platform the sweep's job runs only when a test says:
//! until then the pages wait, and an allocation that finds none of its
//! class sweeps one itself; a safepoint applies what the job swept; the
//! next marking, or a collect, sweeps the rest in its pause. A page with an
//! object to finalize on the heap's thread (a `LocalExternal`'s) waits for
//! the safepoint after the pause, and its finalizers run there; the job
//! finalizes the others (`External`s', of `Send` values). Every heap runs
//! the verifier, which checks each page as its sweep is applied, and the
//! pages' accounts once the sweep is done.

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

use elise::{External, GcObject, Heap, HeapOptions, LocalExternal, Member, Persistent, ThreadPool};

use crate::concurrent::{Stepping, finish, start_marking, stepping_heap_with};

#[derive(GcObject)]
struct Node {
    next: Member<Node>,
    value: Cell<u32>,
}

/// Of another size class.
#[derive(GcObject)]
struct Wide {
    words: [Cell<u64>; 20],
}

/// Of a third class, about 1 KiB: a page holds a couple of hundred.
#[derive(GcObject)]
struct Mid {
    next: Member<Mid>,
    words: [Cell<u64>; 120],
}

fn mid<'gc>(cx: &elise::Mutation<'gc>, next: Option<elise::Gc<'gc, Mid>>) -> elise::Gc<'gc, Mid> {
    cx.alloc(MidInit {
        next,
        words: std::array::from_fn(|_| Cell::new(0)),
    })
}

/// An object with drop glue: counts its finalizations.
#[derive(GcObject)]
struct Finalized {
    drops: LocalExternal<Drops>,
    /// As large as `SendFinalized`: of its class.
    _pad: Cell<u64>,
}

/// A large object with drop glue.
#[derive(GcObject)]
struct Big {
    words: [Cell<u64>; 5000],
    drops: LocalExternal<Drops>,
}

/// Of the same size as `Finalized`, its finalizer the sweep's.
#[derive(GcObject)]
struct SendFinalized {
    drops: External<SendDrops>,
    /// As `Finalized`'s.
    _pad: Cell<u64>,
}

/// A large object whose finalizer is the sweep's.
#[derive(GcObject)]
struct SendBig {
    words: [Cell<u64>; 5000],
    drops: External<SendDrops>,
}

/// Counts its drops from any thread.
struct SendDrops {
    count: Arc<AtomicU32>,
}

impl SendDrops {
    fn new(count: &Arc<AtomicU32>) -> External<Self> {
        External::new(Self {
            count: count.clone(),
        })
    }
}

impl Drop for SendDrops {
    fn drop(&mut self) {
        #[expect(clippy::disallowed_methods, reason = "a test's count of a few drops")]
        self.count.fetch_add(1, Ordering::Relaxed);
    }
}

/// Records the thread it is dropped on.
#[derive(GcObject)]
struct ThreadRecorded {
    threads: External<Recorder>,
}

struct Recorder(Arc<Mutex<Vec<ThreadId>>>);

impl Drop for Recorder {
    fn drop(&mut self) {
        self.0.lock().unwrap().push(std::thread::current().id());
    }
}

struct Drops(Rc<Cell<u32>>);

impl Drop for Drops {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

/// A heap on a `Stepping` platform, a full GC after 64 KiB (marking from
/// 32 KiB), with a nursery of `nursery_bytes`.
fn heap(nursery_bytes: usize) -> (Heap, Arc<Stepping>) {
    stepping_heap_with(HeapOptions {
        min_allocation_bytes: 64 << 10,
        min_overshoot_bytes: 0,
        nursery_bytes,
        ..crate::verified()
    })
}

/// A list of `len` nodes in the old generation, valued 0 up, then a
/// collect (its sweep done in its pause).
fn old_list(heap: &mut Heap, len: u32) -> Persistent<Node> {
    let list = heap.mutate(|cx| {
        cx.pretenured(|cx| {
            let mut head = None;
            for value in (0..len).rev() {
                head = Some(cx.alloc(NodeInit {
                    next: head,
                    value: Cell::new(value),
                }));
            }
            Persistent::new(cx, head.unwrap())
        })
    });
    heap.collect();
    list
}

fn check_list(heap: &mut Heap, list: &Persistent<Node>, len: u32) {
    heap.mutate(|cx| {
        let mut node = Some(list.get(cx));
        for value in 0..len {
            let here = node.unwrap();
            assert_eq!(here.value.get(), value);
            node = here.next.get(cx);
        }
        assert!(node.is_none());
    });
}

/// Pages of garbage of another class than the list's, then a concurrent
/// marking started and finished (its job run through): the sweep posted,
/// not run.
fn marked(heap: &mut Heap, steps: &Stepping) {
    heap.mutate(|cx| {
        cx.pretenured(|cx| {
            for _ in 0..150 {
                cx.alloc(WideInit {
                    words: std::array::from_fn(|_| Cell::new(0)),
                });
            }
        });
    });
    start_marking(heap);
    finish(heap, steps);
    assert!(heap.is_sweeping());
}

/// After a marking, the pages wait for the sweep, and the heap uses what
/// marking found; an allocation of a class with no page swept sweeps one
/// on the heap's thread.
#[test]
fn an_allocation_sweeps_a_page_of_its_class() {
    let (mut heap, steps) = heap(0);
    let list = old_list(&mut heap, 200);
    marked(&mut heap, &steps);
    let stats = heap.stats();
    heap.mutate(|cx| {
        cx.pretenured(|cx| {
            cx.alloc(WideInit {
                words: std::array::from_fn(|_| Cell::new(7)),
            })
        });
    });
    // The class's one page (all garbage, freed), none of the list's.
    assert_eq!(
        heap.stats().pages_swept_on_main,
        stats.pages_swept_on_main + 1
    );
    assert!(heap.is_sweeping(), "the other pages still wait");
    check_list(&mut heap, &list, 200);
}

/// Until the sweep is done the heap counts what it used before the GC
/// (garbage too); once done, what the sweep found live, which is also the
/// GC's marked bytes (and the next limit).
#[test]
fn the_heap_counts_what_the_sweep_finds_live() {
    let (mut heap, steps) = heap(0);
    let list = old_list(&mut heap, 200);
    marked(&mut heap, &steps);
    let during = heap.stats();
    assert_eq!(during.last_gc.marked_bytes, 0, "not swept yet");
    steps.step();
    heap.mutate(|_| {});
    assert!(!heap.is_sweeping());
    let after = heap.stats();
    assert!(after.last_gc.marked_bytes > 0);
    assert_eq!(after.used_bytes, after.last_gc.marked_bytes);
    assert!(during.used_bytes > after.used_bytes, "garbage until swept");
    check_list(&mut heap, &list, 200);
}

/// The old generation's fragmentation is measured when a concurrent
/// marking starts, and once the sweep is done, not while it runs; the range the heap's thread allocates from then
/// counts as free: the pages' slots, free and in use, add up to what is
/// used.
#[test]
fn fragmentation_is_measured_once_the_sweep_is_done() {
    let (mut heap, steps) = stepping_heap_with(HeapOptions {
        min_allocation_bytes: 64 << 10,
        min_overshoot_bytes: 0,
        nursery_bytes: 0,
        measure_fragmentation: true,
        ..crate::verified()
    });
    let list = old_list(&mut heap, 200);
    let sweeps = heap.stats().fragmentation.count;
    let markings = heap.stats().fragmentation_at_marking.count;
    marked(&mut heap, &steps);
    assert_eq!(
        heap.stats().fragmentation_at_marking.count,
        markings + 1,
        "a concurrent marking's start"
    );
    // Sweeps a page of the class itself, and takes a range of it.
    heap.mutate(|cx| {
        cx.pretenured(|cx| {
            cx.alloc(NodeInit {
                next: None,
                value: Cell::new(1),
            })
        });
    });
    assert_eq!(heap.stats().fragmentation.count, sweeps, "while sweeping");
    steps.step();
    heap.mutate(|_| {});
    assert!(!heap.is_sweeping());
    let stats = heap.stats();
    let measured = stats.fragmentation;
    assert_eq!(measured.count, sweeps + 1);
    assert_eq!(measured.slot_bytes - measured.free_bytes, stats.used_bytes);
    check_list(&mut heap, &list, 200);
}

/// The nursery's pages, in use when the sweep is done, aren't counted as
/// the old generation's.
#[test]
fn the_nursery_isnt_measured_as_old() {
    let (mut heap, steps) = stepping_heap_with(HeapOptions {
        min_allocation_bytes: 64 << 10,
        min_overshoot_bytes: 0,
        nursery_bytes: 1 << 20,
        measure_fragmentation: true,
        ..crate::verified()
    });
    let list = old_list(&mut heap, 200);
    marked(&mut heap, &steps);
    heap.mutate(|cx| {
        cx.alloc(NodeInit {
            next: None,
            value: Cell::new(1),
        });
    });
    steps.step();
    heap.mutate(|_| {});
    assert!(!heap.is_sweeping());
    let stats = heap.stats();
    assert!(stats.young_bytes > 0, "{stats:?}");
    assert!(
        stats.fragmentation.old_committed_bytes + (256 << 10) <= stats.committed_bytes,
        "{stats:?}"
    );
    check_list(&mut heap, &list, 200);
}

/// The job sweeps the pages; the next safepoint applies what it found,
/// and the sweep is done: allocation then sweeps nothing itself, and takes
/// the room the sweep found on the list's pages first.
#[test]
fn a_safepoint_applies_the_jobs_sweep() {
    let (mut heap, steps) = heap(0);
    let list = old_list(&mut heap, 200);
    marked(&mut heap, &steps);
    let swept = heap.stats().pages_swept_on_main;
    steps.step();
    assert!(heap.is_sweeping(), "until a safepoint");
    heap.mutate(|_| {});
    assert!(!heap.is_sweeping());
    let page = |text: String| {
        usize::from_str_radix(text.trim_start_matches("Gc(0x").trim_end_matches(')'), 16).unwrap()
            / (256 << 10)
    };
    heap.mutate(|cx| {
        let list_page = page(format!("{:?}", list.get(cx)));
        let node = cx.pretenured(|cx| {
            cx.alloc(NodeInit {
                next: None,
                value: Cell::new(7),
            })
        });
        assert_eq!(
            page(format!("{node:?}")),
            list_page,
            "a page the sweep found room on"
        );
        cx.pretenured(|cx| {
            for _ in 0..100 {
                cx.alloc(WideInit {
                    words: std::array::from_fn(|_| Cell::new(7)),
                });
            }
        });
    });
    assert_eq!(heap.stats().pages_swept_on_main, swept);
    assert!(heap.stats().background_sweep_ns > 0);
    check_list(&mut heap, &list, 200);
}

/// A scavenge's safepoint applies the job's sweep too.
#[test]
fn a_scavenge_applies_the_jobs_sweep() {
    let (mut heap, steps) = heap(0);
    let _list = old_list(&mut heap, 200);
    marked(&mut heap, &steps);
    steps.step();
    heap.scavenge();
    assert!(!heap.is_sweeping());
}

/// An allocation sweeps a page only when its page runs out of free ranges:
/// the links of a list of a class of its own alternate with garbage over
/// two pages, so the first page swept has ranges of one slot, lowest first.
#[test]
fn an_allocation_sweeps_once_its_page_runs_out() {
    let (mut heap, steps) = heap(0);
    let list = heap.mutate(|cx| {
        cx.pretenured(|cx| {
            let mut head = None;
            for _ in 0..300 {
                head = Some(mid(cx, head));
                mid(cx, None);
            }
            Persistent::new(cx, head.unwrap())
        })
    });
    heap.collect();
    marked(&mut heap, &steps);
    let swept = heap.stats().pages_swept_on_main;
    heap.mutate(|cx| {
        cx.pretenured(|cx| {
            for _ in 0..3 {
                mid(cx, None);
            }
        });
    });
    assert_eq!(heap.stats().pages_swept_on_main, swept + 1);
    drop(list);
}

/// The next marking sweeps what is left in its start's pause (its marking
/// needs the marks the sweep reads cleared), as a collect does.
#[test]
fn the_next_gc_sweeps_the_rest_first() {
    let (mut heap, steps) = heap(0);
    let list = old_list(&mut heap, 200);
    marked(&mut heap, &steps);
    // The garbage that starts the next marking is of the list's class: the
    // other class's pages wait to the end.
    start_marking(&mut heap);
    assert!(!heap.is_sweeping());
    finish(&mut heap, &steps);
    check_list(&mut heap, &list, 200);
    assert!(heap.is_sweeping());
    heap.collect();
    assert!(!heap.is_sweeping(), "a collect sweeps in its pause");
    check_list(&mut heap, &list, 200);
}

/// A scavenge while the sweep runs promotes to pages swept (or free), not
/// to those waiting: what it promoted stays once they are swept and
/// allocated on.
#[test]
fn a_scavenge_promotes_around_the_waiting_pages() {
    let (mut heap, steps) = heap(256 << 10);
    let list = old_list(&mut heap, 200);
    marked(&mut heap, &steps);
    let young = heap.mutate(|cx| {
        let mut head = None;
        for value in (0..100).rev() {
            head = Some(cx.alloc(NodeInit {
                next: head,
                value: Cell::new(value),
            }));
        }
        Persistent::new(cx, head.unwrap())
    });
    heap.scavenge();
    heap.scavenge();
    assert!(heap.is_sweeping());
    steps.step();
    heap.mutate(|cx| {
        cx.pretenured(|cx| {
            for value in 0..2000 {
                cx.alloc(NodeInit {
                    next: None,
                    value: Cell::new(value),
                });
            }
        });
    });
    check_list(&mut heap, &young, 100);
    check_list(&mut heap, &list, 200);
}

/// Finalizers don't run in a concurrent marking's pause: their pages wait
/// for the heap's thread, at the next safepoint; a large object's too.
#[test]
fn finalizers_run_at_the_safepoint_after_the_pause() {
    let (mut heap, steps) = heap(0);
    let drops = Rc::new(Cell::new(0));
    let list = old_list(&mut heap, 50);
    heap.mutate(|cx| {
        cx.pretenured(|cx| {
            for _ in 0..10 {
                cx.alloc(FinalizedInit {
                    drops: LocalExternal::new(Drops(drops.clone())),
                    _pad: Cell::new(0),
                });
            }
            cx.alloc(BigInit {
                words: std::array::from_fn(|_| Cell::new(0)),
                drops: LocalExternal::new(Drops(drops.clone())),
            });
        });
    });
    marked(&mut heap, &steps);
    assert_eq!(drops.get(), 0, "not in the pause");
    heap.mutate(|_| {});
    assert_eq!(drops.get(), 11);
    check_list(&mut heap, &list, 50);
    // The large object's pages freed (the verifier checks no dead one is
    // kept) and the finalizers run once.
    heap.collect();
    assert_eq!(drops.get(), 11);
}

/// A collect sweeps in its pause, the finalizers too (cppgc's forced GC).
#[test]
fn a_collect_sweeps_and_finalizes_in_its_pause() {
    let (mut heap, _steps) = heap(0);
    let drops = Rc::new(Cell::new(0));
    heap.mutate(|cx| {
        cx.pretenured(|cx| {
            for _ in 0..10 {
                cx.alloc(FinalizedInit {
                    drops: LocalExternal::new(Drops(drops.clone())),
                    _pad: Cell::new(0),
                });
            }
        });
    });
    heap.collect();
    assert_eq!(drops.get(), 10);
    assert!(!heap.is_sweeping());
}

/// A heap that goes while its sweep waits cancels the job (which reads its
/// pages' marks).
#[test]
fn a_heap_that_goes_cancels_its_sweep() {
    let (mut heap, steps) = heap(0);
    let list = old_list(&mut heap, 200);
    marked(&mut heap, &steps);
    assert!(steps.posted());
    // A root goes before its heap.
    drop(list);
    drop(heap);
    assert!(!steps.posted());
}

/// Dead objects whose finalizers are the sweep's (`External`s'): not run
/// at the safepoints, but by the job; a large one's pages freed once the
/// job finalized it.
#[test]
fn the_sweep_finalizes_the_send_objects() {
    let (mut heap, steps) = heap(0);
    let count = Arc::new(AtomicU32::new(0));
    let list = old_list(&mut heap, 50);
    heap.mutate(|cx| {
        cx.pretenured(|cx| {
            for _ in 0..10 {
                cx.alloc(SendFinalizedInit {
                    drops: SendDrops::new(&count),
                    _pad: Cell::new(0),
                });
            }
            cx.alloc(SendBigInit {
                words: std::array::from_fn(|_| Cell::new(0)),
                drops: SendDrops::new(&count),
            });
        });
    });
    marked(&mut heap, &steps);
    // An allocation of the class takes another page, not one the job
    // finalizes (mid-operation).
    let kept = heap.mutate(|cx| {
        let kept = cx.pretenured(|cx| {
            cx.alloc(SendFinalizedInit {
                drops: SendDrops::new(&count),
                _pad: Cell::new(0),
            })
        });
        Persistent::new(cx, kept)
    });
    assert_eq!(
        count.load(Ordering::Relaxed),
        0,
        "the job's, not the safepoint's"
    );
    assert!(heap.is_sweeping());
    steps.step();
    assert_eq!(count.load(Ordering::Relaxed), 11);
    heap.mutate(|_| {});
    assert!(!heap.is_sweeping());
    // The large object's pages are free: another takes them.
    let committed = heap.stats().committed_bytes;
    let big = heap.mutate(|cx| {
        let big = cx.alloc(SendBigInit {
            words: std::array::from_fn(|_| Cell::new(0)),
            drops: SendDrops::new(&count),
        });
        Persistent::new(cx, big)
    });
    assert_eq!(heap.stats().committed_bytes, committed);
    drop((kept, big));
    heap.collect();
    count.fetch_sub(2, Ordering::Relaxed);
    assert_eq!(count.load(Ordering::Relaxed), 11, "once");
    check_list(&mut heap, &list, 50);
}

/// A page with an object to finalize on the heap's thread is that
/// thread's: its `Send` objects' finalizers run there too, at the next
/// safepoint.
#[test]
fn a_page_with_a_local_finalizer_is_the_heaps_threads() {
    let (mut heap, steps) = heap(0);
    let local = Rc::new(Cell::new(0));
    let sent = Arc::new(AtomicU32::new(0));
    let _list = old_list(&mut heap, 50);
    heap.mutate(|cx| {
        cx.pretenured(|cx| {
            for _ in 0..10 {
                cx.alloc(FinalizedInit {
                    drops: LocalExternal::new(Drops(local.clone())),
                    _pad: Cell::new(0),
                });
                cx.alloc(SendFinalizedInit {
                    drops: SendDrops::new(&sent),
                    _pad: Cell::new(0),
                });
            }
        });
    });
    marked(&mut heap, &steps);
    heap.mutate(|_| {});
    assert_eq!((local.get(), sent.load(Ordering::Relaxed)), (10, 10));
}

/// Counts its drops in a static, and panics in each: what is left
/// unfinalized owns no memory (Miri fails a test that leaks).
struct PanickingDrops(&'static AtomicU32);

impl Drop for PanickingDrops {
    fn drop(&mut self) {
        #[expect(clippy::disallowed_methods, reason = "a test's count of a few drops")]
        self.0.fetch_add(1, Ordering::Relaxed);
        panic!("a finalizer panics");
    }
}

#[derive(GcObject)]
struct PanickingFinalized {
    drops: External<PanickingDrops>,
}

/// A finalizer that panics on the job reaches the thread that ran it (here
/// the test's); its page waits again, its other dead objects left
/// unfinalized (leaked), and the next GC sweeps it: nothing is finalized
/// twice.
#[test]
fn a_finalizer_that_panics_finalizes_nothing_twice() {
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let (mut heap, steps) = heap(0);
    let _list = old_list(&mut heap, 50);
    heap.mutate(|cx| {
        cx.pretenured(|cx| {
            for _ in 0..2 {
                cx.alloc(PanickingFinalizedInit {
                    drops: External::new(PanickingDrops(&COUNT)),
                });
            }
        });
    });
    marked(&mut heap, &steps);
    let panicked = catch_unwind(AssertUnwindSafe(|| steps.step()));
    assert!(panicked.is_err());
    assert_eq!(COUNT.load(Ordering::Relaxed), 1, "the other is left");
    assert!(heap.is_sweeping());
    heap.collect();
    assert!(!heap.is_sweeping());
    assert_eq!(COUNT.load(Ordering::Relaxed), 1, "never twice");
}

/// On a pool, the sweep's finalizers run on its workers, not the heap's
/// thread.
#[test]
fn the_send_finalizers_run_on_a_worker() {
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
    let _list = old_list(&mut heap, 50);
    heap.mutate(|cx| {
        cx.pretenured(|cx| {
            for _ in 0..10 {
                cx.alloc(ThreadRecordedInit {
                    threads: External::new(Recorder(threads.clone())),
                });
            }
        });
    });
    // A marking started and finished, then its sweep done alongside.
    while !heap.is_marking() {
        heap.mutate(|cx| {
            cx.pretenured(|cx| {
                cx.alloc(WideInit {
                    words: std::array::from_fn(|_| Cell::new(0)),
                })
            });
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
    assert_eq!(threads.len(), 10);
    let here = std::thread::current().id();
    assert!(threads.iter().all(|&thread| thread != here), "{threads:?}");
}
