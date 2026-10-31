//! shuttle: random schedules of a GC marking a tree with garbage on two
//! workers, of a scavenge of young objects that old ones on several pages
//! and a root all point to, and of a marking alongside the mutator, which
//! moves a list's tail to new objects and cuts it while workers mark, and
//! of a sweep alongside the mutator, which allocates (sweeping a page
//! itself when none of its class is swept) and scavenges while workers
//! sweep. Every schedule keeps and frees the same
//! as one thread, counts as many of a site's objects marked or promoted
//! (each thread counts its own), and some have the workers do work (scan
//! objects, update a slot set's page) and two threads copy the same object
//! (else the test tests nothing of sharing). Run with `--cfg shuttle`
//! (`python3 scripts/test.py models`).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use elise::{
    External, GcObject, Heap, HeapOptions, HeapVector, LocalExternal, Member, Persistent, Site,
    ThreadPool, WeakPersistent,
};

type Log = Rc<RefCell<Vec<u32>>>;

struct Finalized(u32, Log);

impl Drop for Finalized {
    fn drop(&mut self) {
        self.1.borrow_mut().push(self.0);
    }
}

#[derive(GcObject)]
struct Branch {
    kids: Member<HeapVector<Branch>>,
    id: Cell<u32>,
    finalized: LocalExternal<Finalized>,
}

const BRANCHES: u32 = 300;
const SCHEDULES: usize = 20_000;

#[test]
fn marking_on_workers_keeps_what_one_thread_keeps() {
    static HELPED: AtomicUsize = AtomicUsize::new(0);
    static BRANCHES_SITE: Site = Site::new();
    shuttle::check_random(
        || {
            let log = Log::default();
            let mut heap = Heap::with_platform(crate::verified(), Arc::new(ThreadPool::new(2)));
            let (root, mut garbage) = heap.mutate(|cx| {
                let branch = |id: u32| {
                    cx.alloc_in(
                        &BRANCHES_SITE,
                        BranchInit {
                            kids: Some(HeapVector::new(cx, 8)),
                            id: Cell::new(id),
                            finalized: LocalExternal::new(Finalized(id, log.clone())),
                        },
                    )
                };
                let mut all = vec![branch(0)];
                let mut garbage = Vec::new();
                for id in 1..BRANCHES {
                    let child = branch(id);
                    // Every fifth branch is left out of the tree: garbage.
                    if id % 5 == 0 {
                        garbage.push(id);
                        continue;
                    }
                    let parent = all[(all.len() - 1) / 8];
                    parent.kids.get(cx).unwrap().push(cx, child);
                    all.push(child);
                }
                (Persistent::new(cx, all[0]), garbage)
            });
            heap.collect();
            let mut finalized = log.borrow().clone();
            finalized.sort_unstable();
            garbage.sort_unstable();
            assert_eq!(finalized, garbage);
            assert_eq!(
                heap.site(&BRANCHES_SITE).marked,
                BRANCHES as usize - garbage.len()
            );
            if heap.stats().last_gc.scanned_by_workers > 0 {
                HELPED.fetch_add(1, Ordering::Relaxed);
            }
            drop(root);
        },
        SCHEDULES,
    );
    assert!(
        HELPED.load(Ordering::Relaxed) > 0,
        "no schedule had a worker scan an object"
    );
}

/// About 8 KiB: a page holds 32, so that the holders' slot sets are on
/// 16 pages, each a piece of a scavenge's work: two segments, under
/// shuttle, for any thread to take, the joining thread's after the copies
/// it made itself. The last page has room left for promoted holders.
#[derive(GcObject)]
struct Holder {
    leaves: [Member<Leaf>; 2],
    pad: [Cell<u64>; 1000],
}

#[derive(GcObject)]
struct Leaf {
    id: Cell<u32>,
    finalized: LocalExternal<Finalized>,
}

#[derive(GcObject)]
struct Parent {
    leaves: [Member<Leaf>; 8],
}

const HOLDERS: usize = 490;
const LEAVES: u32 = 40;
const SCAVENGE_SCHEDULES: usize = 1000;
/// Young holders a root holds, promoted by the second scavenge onto the
/// old holders' last page (the only one of their class with room).
const CARRIERS: usize = 4;

/// The leaf holder `holder`'s edge `edge` points at: `holder` and
/// `holder + 31` (holders about a page apart share leaves), mod 30. The parent
/// points at leaves 0 to 7; leaves 30 and up are garbage.
fn leaf_of(holder: usize, edge: usize) -> u32 {
    ((holder + edge * 31) % 30) as u32
}

#[test]
fn a_scavenge_on_workers_keeps_what_one_thread_keeps() {
    static SLOTS_BY_WORKERS: AtomicUsize = AtomicUsize::new(0);
    static ABANDONED: AtomicUsize = AtomicUsize::new(0);
    static LEAVES_SITE: Site = Site::new();
    shuttle::check_random(
        || {
            let log = Log::default();
            let mut heap = Heap::with_platform(crate::verified(), Arc::new(ThreadPool::new(2)));
            // Old holders, on several pages.
            let holders = heap.mutate(|cx| {
                let list = HeapVector::new(cx, HOLDERS);
                for _ in 0..HOLDERS {
                    list.push(
                        cx,
                        cx.alloc(HolderInit {
                            leaves: [None, None],
                            pad: std::array::from_fn(|_| Cell::new(0)),
                        }),
                    );
                }
                Persistent::new(cx, list)
            });
            heap.collect();
            // Young leaves, each pointed to from holders on two pages and,
            // the first eight, from a young parent a root holds; and young
            // holders, carriers, a root holds.
            let (parent, carriers) = heap.mutate(|cx| {
                let leaves: Vec<_> = (0..LEAVES)
                    .map(|id| {
                        cx.alloc_in(
                            &LEAVES_SITE,
                            LeafInit {
                                id: Cell::new(id),
                                finalized: LocalExternal::new(Finalized(id, log.clone())),
                            },
                        )
                    })
                    .collect();
                let list = holders.get(cx);
                for index in 0..HOLDERS {
                    let holder = list.get(cx, index).unwrap();
                    for edge in 0..2 {
                        let leaf = leaves[leaf_of(index, edge) as usize];
                        holder.leaves[edge].set(cx, Some(leaf));
                    }
                }
                let parent = cx.alloc(ParentInit {
                    leaves: std::array::from_fn(|i| Some(leaves[i])),
                });
                let carriers = HeapVector::new(cx, CARRIERS);
                for _ in 0..CARRIERS {
                    carriers.push(
                        cx,
                        cx.alloc(HolderInit {
                            leaves: [None, None],
                            pad: std::array::from_fn(|_| Cell::new(0)),
                        }),
                    );
                }
                (Persistent::new(cx, parent), Persistent::new(cx, carriers))
            });
            heap.scavenge();
            let stats = heap.stats().last_scavenge;
            // Leaves 0 to 29 live, the parent, the carriers, their vector
            // and its array: leaves 30 to 39 are garbage.
            assert_eq!(stats.survived_objects, 30 + 1 + CARRIERS + 2);
            let mut finalized = log.borrow().clone();
            finalized.sort_unstable();
            assert_eq!(finalized, (30..LEAVES).collect::<Vec<_>>());
            heap.mutate(|cx| {
                let list = holders.get(cx);
                for index in 0..HOLDERS {
                    let holder = list.get(cx, index).unwrap();
                    for edge in 0..2 {
                        let leaf = holder.leaves[edge].get(cx).unwrap();
                        assert_eq!(leaf.id.get(), leaf_of(index, edge));
                    }
                }
                for (i, edge) in parent.get(cx).leaves.iter().enumerate() {
                    assert_eq!(edge.get(cx).unwrap().id.get(), i as u32);
                }
            });
            if stats.slot_pages_by_workers > 0 {
                SLOTS_BY_WORKERS.fetch_add(1, Ordering::Relaxed);
            }
            if stats.copies_abandoned > 0 {
                ABANDONED.fetch_add(1, Ordering::Relaxed);
            }
            // The carriers, aged, point at new leaves (young to young: not
            // recorded). The second scavenge promotes them onto the holders'
            // last page and puts their edges to the new leaves, copied
            // young, into its slot set: a set the scavenge updates as one of
            // its pages too, perhaps after (whose edges then point to
            // to-space already, and must stay recorded: the verifier checks).
            heap.mutate(|cx| {
                let carriers = carriers.get(cx);
                for index in 0..CARRIERS {
                    let leaf = cx.alloc_in(
                        &LEAVES_SITE,
                        LeafInit {
                            id: Cell::new(100 + index as u32),
                            finalized: LocalExternal::new(Finalized(
                                100 + index as u32,
                                log.clone(),
                            )),
                        },
                    );
                    carriers.get(cx, index).unwrap().leaves[0].set(cx, Some(leaf));
                }
            });
            heap.scavenge();
            assert_eq!(heap.stats().last_scavenge.survived_objects, CARRIERS);
            // The 30 leaves, aged, promoted by whichever threads: the
            // carriers' new leaves stay young.
            assert_eq!(heap.site(&LEAVES_SITE).entered_old, 30);
            heap.mutate(|cx| {
                let carriers = carriers.get(cx);
                for index in 0..CARRIERS {
                    let leaf = carriers.get(cx, index).unwrap().leaves[0].get(cx).unwrap();
                    assert_eq!(leaf.id.get(), 100 + index as u32);
                }
            });
            drop((holders, parent, carriers));
        },
        SCAVENGE_SCHEDULES,
    );
    assert!(
        SLOTS_BY_WORKERS.load(Ordering::Relaxed) > 0,
        "no schedule had a worker update a slot set's page"
    );
    assert!(
        ABANDONED.load(Ordering::Relaxed) > 0,
        "no schedule had two threads copy the same object"
    );
}

#[derive(GcObject)]
struct Link {
    next: Member<Link>,
    id: Cell<u32>,
    finalized: LocalExternal<Finalized>,
}

const LINKS: u32 = 24;
const MARKING_SCHEDULES: usize = 1000;

fn new_link<'gc>(cx: &elise::Mutation<'gc>, id: u32, log: &Log) -> elise::Gc<'gc, Link> {
    cx.alloc(LinkInit {
        next: None,
        id: Cell::new(id),
        finalized: LocalExternal::new(Finalized(id, log.clone())),
    })
}

/// The ids along the list from `from`.
fn ids(heap: &mut Heap, from: &Persistent<Link>) -> Vec<u32> {
    heap.mutate(|cx| {
        let mut ids = Vec::new();
        let mut next = Some(from.get(cx));
        while let Some(link) = next {
            ids.push(link.id.get());
            next = link.next.get(cx);
        }
        ids
    })
}

/// A list of `LINKS` from a root, old. A marking starts (garbage pushes
/// the old generation to half its limit), and while the workers mark, the
/// mutator, an operation at a time: puts the list's second half behind a
/// new link (black, never scanned) under a root of its own, cuts it off
/// the first half, drops the first half's last two links, and links in
/// their place a link only a weak root reached (garbage at the start).
/// Every schedule keeps what the roots reach (the barrier keeps the second
/// half, the weak read barrier the link read; the verifier checks after
/// each GC), finalizes only what they don't and clears a weak root to a
/// dropped link; some have the workers mark while the mutator runs.
#[test]
fn marking_alongside_the_mutator_keeps_what_one_thread_keeps() {
    static ALONGSIDE: AtomicUsize = AtomicUsize::new(0);
    shuttle::check_random(
        || {
            let log = Log::default();
            let mut heap = Heap::with_platform(
                HeapOptions {
                    min_allocation_bytes: 4 << 10,
                    // Markings finish at the limit too.
                    min_overshoot_bytes: 0,
                    nursery_bytes: 0,
                    ..crate::verified()
                },
                Arc::new(ThreadPool::new(2)),
            );
            let head = heap.mutate(|cx| {
                let head = new_link(cx, 0, &log);
                let mut last = head;
                for id in 1..LINKS {
                    let next = new_link(cx, id, &log);
                    last.next.set(cx, Some(next));
                    last = next;
                }
                Persistent::new(cx, head)
            });
            // A link nothing strong reaches, and a weak root to it.
            let lone = heap.mutate(|cx| WeakPersistent::new(cx, new_link(cx, 200, &log)));
            while !heap.is_marking() {
                heap.mutate(|cx| {
                    for _ in 0..8 {
                        new_link(cx, 1000, &log);
                    }
                });
            }
            let half = LINKS / 2;
            // The new link before the second half, rooted.
            let tail = heap.mutate(|cx| {
                let mut at = head.get(cx);
                for _ in 1..half {
                    at = at.next.get(cx).unwrap();
                }
                let new = new_link(cx, 100, &log);
                new.next.set(cx, at.next.get(cx));
                Persistent::new(cx, new)
            });
            // Cut off, the first half's last two dropped, the lone link
            // read through its weak root in their place.
            let dropped = heap.mutate(|cx| {
                let mut at = head.get(cx);
                for _ in 1..half - 2 {
                    at = at.next.get(cx).unwrap();
                }
                let dropped = WeakPersistent::new(cx, at.next.get(cx).unwrap());
                at.next
                    .set(cx, Some(lone.get(cx).expect("no full GC ran yet")));
                dropped
            });
            // Finishes once the workers are done (or at the limit).
            for _ in 0..200 {
                if !heap.is_marking() {
                    break;
                }
                heap.mutate(|_| {});
            }
            let record = heap.stats().last_gc;
            if record.concurrent && record.marked_in_pause < record.marked_objects {
                ALONGSIDE.fetch_add(1, Ordering::Relaxed);
            }
            heap.collect();
            assert_eq!(
                ids(&mut heap, &head),
                (0..half - 2)
                    .chain(std::iter::once(200))
                    .collect::<Vec<_>>()
            );
            heap.mutate(|cx| {
                assert_eq!(lone.get(cx).map(|link| link.id.get()), Some(200));
                assert!(
                    dropped.get(cx).is_none(),
                    "a weak root to a dropped link is cleared"
                );
            });
            assert_eq!(
                ids(&mut heap, &tail),
                std::iter::once(100).chain(half..LINKS).collect::<Vec<_>>()
            );
            // Finalized: the garbage and the two dropped, nothing else.
            let finalized: Vec<u32> = log
                .borrow()
                .iter()
                .copied()
                .filter(|&id| id < 1000)
                .collect();
            let mut finalized = finalized;
            finalized.sort_unstable();
            assert_eq!(finalized, vec![half - 2, half - 1]);
            drop((head, tail, lone, dropped));
        },
        MARKING_SCHEDULES,
    );
    assert!(
        ALONGSIDE.load(Ordering::Relaxed) > 0,
        "no schedule had the workers mark while the mutator ran"
    );
}

/// An object with no drop glue (its pages are the sweep's), of 8 KiB: 32
/// on a page, so that the sweep has pages to sweep.
#[derive(GcObject)]
struct Plain {
    next: Member<Plain>,
    id: Cell<u32>,
    words: [Cell<u64>; 1000],
}

fn plain<'gc>(
    cx: &elise::Mutation<'gc>,
    id: u32,
    next: Option<elise::Gc<'gc, Plain>>,
) -> elise::Gc<'gc, Plain> {
    cx.alloc(PlainInit {
        next,
        id: Cell::new(id),
        words: std::array::from_fn(|_| Cell::new(0)),
    })
}

/// Garbage whose finalizer is the sweep's worker's (an `External`'s):
/// counts its drops, from any thread. Of 8 KiB, so that its pages take the
/// job a while.
#[derive(GcObject)]
struct Sent {
    drops: External<SentDrops>,
    words: [Cell<u64>; 1000],
}

struct SentDrops(Arc<AtomicUsize>);

impl Drop for SentDrops {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

const SENT: usize = 40;

const SWEEP_SCHEDULES: usize = 1000;

/// A list of plain objects, old, and garbage of their class (and garbage
/// whose finalizer is the sweep's) until a marking starts and finishes.
/// While the workers sweep (and finalize), the mutator allocates more of
/// the class (on pages swept, or sweeping one itself) into a young list,
/// and scavenges it into the old generation (onto pages swept, not those
/// waiting). Every schedule keeps both lists whole (the verifier checks
/// each page as its sweep is applied, and the pages once the sweep is
/// done) and finalizes each of the garbage once; some have the workers
/// sweep, and finalize, while the mutator runs.
#[test]
fn sweeping_alongside_the_mutator_keeps_the_live() {
    static ALONGSIDE: AtomicUsize = AtomicUsize::new(0);
    static FINALIZED_ALONGSIDE: AtomicUsize = AtomicUsize::new(0);
    shuttle::check_random(
        || {
            let drops = Arc::new(AtomicUsize::new(0));
            let mut heap = Heap::with_platform(
                HeapOptions {
                    min_allocation_bytes: 1 << 20,
                    min_overshoot_bytes: 0,
                    nursery_bytes: 1,
                    max_nursery_bytes: 1,
                    ..crate::verified()
                },
                Arc::new(ThreadPool::new(2)),
            );
            // The list's links among garbage, on several pages.
            let old = heap.mutate(|cx| {
                cx.pretenured(|cx| {
                    let mut head = None;
                    for id in (0..LINKS).rev() {
                        head = Some(plain(cx, id, head));
                        for _ in 0..3 {
                            plain(cx, 1000, None);
                        }
                    }
                    Persistent::new(cx, head.unwrap())
                })
            });
            heap.collect();
            heap.mutate(|cx| {
                cx.pretenured(|cx| {
                    for _ in 0..SENT {
                        cx.alloc(SentInit {
                            drops: External::new(SentDrops(drops.clone())),
                            words: std::array::from_fn(|_| Cell::new(0)),
                        });
                    }
                });
            });
            while !heap.is_marking() {
                heap.mutate(|cx| {
                    cx.pretenured(|cx| {
                        for _ in 0..4 {
                            plain(cx, 1000, None);
                        }
                    });
                });
            }
            for _ in 0..200 {
                if !heap.is_marking() {
                    break;
                }
                heap.mutate(|_| {});
            }
            // While the sweep runs: an operation, and whether a worker's
            // run ended while it ran.
            let operate = |heap: &mut Heap, f: &mut dyn FnMut(&mut Heap)| {
                let sweeping = heap.is_sweeping();
                let before = heap.stats().background_sweep_ns;
                let dropped = drops.load(Ordering::Relaxed);
                f(heap);
                if sweeping && heap.stats().background_sweep_ns > before {
                    ALONGSIDE.fetch_add(1, Ordering::Relaxed);
                }
                if sweeping && drops.load(Ordering::Relaxed) > dropped {
                    FINALIZED_ALONGSIDE.fetch_add(1, Ordering::Relaxed);
                }
            };
            let mut young = None;
            operate(&mut heap, &mut |heap| {
                young = Some(heap.mutate(|cx| Persistent::new(cx, plain(cx, 0, None))));
            });
            let young = young.unwrap();
            for id in 1..LINKS {
                if !heap.is_sweeping() {
                    break;
                }
                operate(&mut heap, &mut |heap| {
                    heap.mutate(|cx| {
                        cx.pretenured(|cx| plain(cx, 1000, None));
                        let head = young.get(cx);
                        let new = plain(cx, id, head.next.get(cx));
                        head.next.set(cx, Some(new));
                    });
                });
                heap.scavenge();
            }
            heap.collect();
            let ids = |heap: &mut Heap, list: &Persistent<Plain>| {
                heap.mutate(|cx| {
                    let mut ids = Vec::new();
                    let mut at = Some(list.get(cx));
                    while let Some(link) = at {
                        ids.push(link.id.get());
                        at = link.next.get(cx);
                    }
                    ids
                })
            };
            assert_eq!(ids(&mut heap, &old), (0..LINKS).collect::<Vec<_>>());
            let young_ids = ids(&mut heap, &young);
            assert_eq!(young_ids[0], 0);
            let mut rest = young_ids[1..].to_vec();
            rest.sort_unstable();
            assert_eq!(rest, (1..young_ids.len() as u32).collect::<Vec<_>>());
            assert_eq!(drops.load(Ordering::Relaxed), SENT, "each finalized once");
        },
        SWEEP_SCHEDULES,
    );
    assert!(
        ALONGSIDE.load(Ordering::Relaxed) > 0,
        "no schedule had the workers sweep while the mutator ran"
    );
    assert!(
        FINALIZED_ALONGSIDE.load(Ordering::Relaxed) > 0,
        "no schedule had the workers finalize while the mutator ran"
    );
}
