//! The heap's own work: size classes and large objects, free slots found
//! and reused, pages pooled and discarded, what it counts, and when a
//! safepoint collects.

use std::cell::Cell;
use std::rc::Rc;

use elise::{
    Gc, GcObject, Heap, HeapOptions, HeapStats, HeapVector, LocalExternal, Member, Persistent,
};

#[derive(GcObject)]
struct Small {
    value: Cell<u32>,
}

/// 16 000 bytes: a size class of the spaced-out ones.
#[derive(GcObject)]
struct Big {
    next: Member<Big>,
    words: [Cell<u64>; 2000],
}

/// About 1 KiB: a page holds a couple of hundred.
#[derive(GcObject)]
struct Medium {
    id: Cell<u32>,
    words: [Cell<u64>; 120],
}

/// 40 000 bytes: a large object, with a finalizer.
#[derive(GcObject)]
struct Large {
    next: Member<Large>,
    words: [Cell<u64>; 5000],
    drops: LocalExternal<Drops>,
}

/// Counts its drops.
struct Drops(Rc<Cell<u32>>);

impl Drop for Drops {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

const PAGE_BYTES: usize = 256 << 10;

/// An object's address.
fn address<T>(object: Gc<'_, T>) -> usize {
    let text = format!("{object:?}");
    usize::from_str_radix(text.trim_start_matches("Gc(0x").trim_end_matches(')'), 16).unwrap()
}

fn medium<'gc>(cx: &elise::Mutation<'gc>, id: u32) -> Gc<'gc, Medium> {
    cx.alloc(MediumInit {
        id: Cell::new(id),
        words: std::array::from_fn(|_| Cell::new(0)),
    })
}

fn large<'gc>(
    cx: &elise::Mutation<'gc>,
    next: Option<Gc<'gc, Large>>,
    fill: u64,
    drops: &Rc<Cell<u32>>,
) -> Gc<'gc, Large> {
    cx.alloc(LargeInit {
        next,
        words: std::array::from_fn(|i| Cell::new(fill + i as u64)),
        drops: LocalExternal::new(Drops(drops.clone())),
    })
}

/// The old generation alone (no nursery): the tests of its pages, slots
/// and limit.
fn old_heap() -> Heap {
    Heap::new(HeapOptions {
        nursery_bytes: 0,
        ..crate::verified()
    })
}

/// The old generation alone, with a GC after every KiB (a marking the
/// workers run alongside finished past half a KiB more at most).
fn small_heap() -> Heap {
    Heap::new(HeapOptions {
        min_allocation_bytes: 1 << 10,
        min_overshoot_bytes: 0,
        nursery_bytes: 0,
        ..crate::verified()
    })
}

/// A dead object's slot is the next one its class hands out; a live one's
/// isn't.
#[test]
fn a_freed_slot_is_reused() {
    let mut heap = old_heap();
    let (kept, dead) = heap.mutate(|cx| {
        let kept = cx.alloc(SmallInit {
            value: Cell::new(1),
        });
        let dead = cx.alloc(SmallInit {
            value: Cell::new(2),
        });
        (Persistent::new(cx, kept), format!("{dead:?}"))
    });
    heap.collect();
    heap.mutate(|cx| {
        let new = cx.alloc(SmallInit {
            value: Cell::new(3),
        });
        assert_eq!(format!("{new:?}"), dead);
        assert_eq!(kept.get(cx).value.get(), 1);
        let next = cx.alloc(SmallInit {
            value: Cell::new(4),
        });
        assert_ne!(format!("{next:?}"), format!("{:?}", kept.get(cx)));
    });
}

/// Slots in use are counted by their class's bytes, header included;
/// pages, a whole one per class in use.
#[test]
fn stats_count_slots_and_pages() {
    let mut heap = old_heap();
    assert_eq!(heap.stats(), HeapStats::default());
    let kept = heap.mutate(|cx| {
        cx.alloc(SmallInit {
            value: Cell::new(0),
        });
        let big = cx.alloc(BigInit {
            next: None,
            words: std::array::from_fn(|_| Cell::new(0)),
        });
        Persistent::new(cx, big)
    });
    let big_slot = heap.stats().used_bytes - 16;
    assert!((16_008..=16_008 + 16_008 / 8 + 16).contains(&big_slot));
    assert_eq!(heap.stats().committed_bytes, 2 * PAGE_BYTES);
    heap.collect();
    let stats = heap.stats();
    assert_eq!(
        (stats.gcs, stats.used_bytes, stats.committed_bytes),
        (1, big_slot, 2 * PAGE_BYTES)
    );
    drop(kept);
}

/// A big object's fields survive GCs whole, its edge too.
#[test]
fn big_objects_survive_whole() {
    let mut heap = Heap::new(HeapOptions {
        stress: true,
        ..crate::verified()
    });
    let head = heap.mutate(|cx| {
        let mut head = None;
        for i in 0..20u64 {
            let big = cx.alloc(BigInit {
                next: head,
                words: std::array::from_fn(|j| Cell::new(i * 10_000 + j as u64)),
            });
            head = Some(big);
        }
        Persistent::new(cx, head.unwrap())
    });
    heap.collect();
    heap.mutate(|cx| {
        let mut big = Some(head.get(cx));
        for i in (0..20u64).rev() {
            let object = big.unwrap();
            for (j, word) in object.words.iter().enumerate() {
                assert_eq!(word.get(), i * 10_000 + j as u64);
            }
            big = object.next.get(cx);
        }
        assert!(big.is_none());
    });
}

/// A vector grown past a page's small classes keeps every edge.
#[test]
fn a_long_vector_keeps_its_edges() {
    let mut heap = small_heap();
    let list = heap.mutate(|cx| Persistent::new(cx, HeapVector::<Small>::new(cx, 0)));
    for value in 0..5000 {
        heap.mutate(|cx| {
            list.get(cx).push(
                cx,
                cx.alloc(SmallInit {
                    value: Cell::new(value),
                }),
            );
        });
    }
    assert!(heap.stats().gcs > 0);
    heap.collect();
    heap.mutate(|cx| {
        let list = list.get(cx);
        assert_eq!(list.len(), 5000);
        for i in 0..5000 {
            assert_eq!(list.get(cx, i).unwrap().value.get(), i as u32);
        }
    });
}

/// A cleared vector is empty, keeps its store (refilled, it allocates
/// nothing), and keeps nothing alive: what it held is collected.
#[test]
fn a_cleared_vector_keeps_its_store_and_nothing_alive() {
    let mut heap = small_heap();
    let list = heap.mutate(|cx| Persistent::new(cx, HeapVector::<Small>::new(cx, 8)));
    heap.mutate(|cx| {
        let list = list.get(cx);
        for value in 0..8 {
            list.push(
                cx,
                cx.alloc(SmallInit {
                    value: Cell::new(value),
                }),
            );
        }
    });
    heap.collect();
    let full = heap.stats().used_bytes;
    heap.mutate(|cx| {
        let list = list.get(cx);
        list.clear(cx);
        assert!(list.is_empty() && list.get(cx, 0).is_none());
    });
    heap.collect();
    let cleared = heap.stats().used_bytes;
    assert!(cleared < full, "{cleared} after {full}");
    heap.mutate(|cx| {
        let list = list.get(cx);
        for value in 0..8 {
            list.push(
                cx,
                cx.alloc(SmallInit {
                    value: Cell::new(value + 10),
                }),
            );
        }
    });
    heap.collect();
    // The 8 new objects, and no new store.
    assert_eq!(heap.stats().used_bytes, full);
    heap.mutate(|cx| assert_eq!(list.get(cx).get(cx, 7).unwrap().value.get(), 17));
}

/// A safepoint collects once as much was allocated since the last GC as
/// it kept, or the minimum: not before (a heap with no workers, which
/// can't mark alongside).
#[test]
fn a_safepoint_collects_after_enough_allocation() {
    let mut heap = Heap::with_platform(
        HeapOptions {
            min_allocation_bytes: 1 << 10,
            nursery_bytes: 0,
            ..crate::verified()
        },
        std::sync::Arc::new(elise::InlinePlatform),
    );
    // 63 slots of 16 bytes: just under 1 KiB.
    heap.mutate(|cx| {
        for _ in 0..63 {
            cx.alloc(SmallInit {
                value: Cell::new(0),
            });
        }
    });
    assert!(!heap.should_collect());
    heap.mutate(|cx| {
        cx.alloc(SmallInit {
            value: Cell::new(0),
        });
    });
    assert!(heap.should_collect());
    heap.mutate(|_| {});
    assert_eq!(heap.stats().gcs, 1);
    assert!(!heap.should_collect());
    // Keep about 5 KiB: the next GC waits for as much, over the minimum.
    let kept = heap.mutate(|cx| {
        let list = HeapVector::<Small>::new(cx, 0);
        for _ in 0..255 {
            list.push(
                cx,
                cx.alloc(SmallInit {
                    value: Cell::new(0),
                }),
            );
        }
        Persistent::new(cx, list)
    });
    heap.collect();
    let used = heap.stats().used_bytes;
    assert!(used > 4 << 10);
    // Just under it, in slots of 16 bytes.
    let under = used.div_ceil(16) - 1;
    heap.mutate(|cx| {
        for _ in 0..under {
            cx.alloc(SmallInit {
                value: Cell::new(0),
            });
        }
    });
    assert!(!heap.should_collect());
    heap.mutate(|cx| {
        cx.alloc(SmallInit {
            value: Cell::new(0),
        });
    });
    assert!(heap.should_collect());
    drop(kept);
}

/// By default the old generation takes 2 MiB after a full GC that kept
/// little before the next.
#[test]
fn the_least_before_a_full_gc_is_2_mib() {
    let mut heap = Heap::with_platform(
        HeapOptions {
            nursery_bytes: 0,
            ..crate::verified()
        },
        std::sync::Arc::new(elise::InlinePlatform),
    );
    while heap.stats().used_bytes < 2 << 20 {
        assert!(!heap.should_collect(), "at {}", heap.stats().used_bytes);
        heap.mutate(|cx| {
            medium(cx, 0);
        });
    }
    assert!(heap.should_collect());
    assert_eq!(heap.stats().gcs, 0);
}

/// Large objects keep their fields and edges through GCs, and are
/// finalized once when they die, or when the heap goes.
#[test]
fn large_objects_live_and_die() {
    let drops = Rc::new(Cell::new(0));
    let mut heap = Heap::new(HeapOptions {
        stress: true,
        ..crate::verified()
    });
    let kept = heap.mutate(|cx| {
        let first = large(cx, None, 100, &drops);
        let second = large(cx, Some(first), 200, &drops);
        large(cx, None, 300, &drops);
        Persistent::new(cx, second)
    });
    heap.collect();
    assert_eq!(drops.get(), 1);
    heap.mutate(|cx| {
        let second = kept.get(cx);
        let first = second.next.get(cx).unwrap();
        for (i, word) in first.words.iter().enumerate() {
            assert_eq!(word.get(), 100 + i as u64);
        }
        assert_eq!(second.words[4999].get(), 200 + 4999);
        assert!(first.next.get(cx).is_none());
        // Dropping the first: finalized at the next GC.
        second.next.set(cx, None);
    });
    heap.collect();
    assert_eq!(drops.get(), 2);
    drop(kept);
    drop(heap);
    assert_eq!(drops.get(), 3);
}

/// A large object commits its bytes rounded up to the OS's pages, not
/// whole pages.
#[test]
fn a_large_object_commits_what_it_uses() {
    let drops = Rc::new(Cell::new(0));
    let mut heap = Heap::new(crate::verified());
    let kept = heap.mutate(|cx| Persistent::new(cx, large(cx, None, 0, &drops)));
    let bytes = std::mem::size_of::<Large>() + 8;
    let committed = heap.stats().committed_bytes;
    // At most a 64 KiB OS page over.
    assert!(
        committed >= bytes && committed < bytes + (64 << 10),
        "{committed}"
    );
    assert!(committed < PAGE_BYTES);
    assert_eq!(heap.stats().used_bytes, committed);
    drop(kept);
}

/// A vector past 32 KiB of edges grows into a large object.
#[test]
fn a_vector_grows_past_a_slot() {
    let mut heap = small_heap();
    let list = heap.mutate(|cx| Persistent::new(cx, HeapVector::<Small>::new(cx, 0)));
    let count = if cfg!(miri) { 9000 } else { 40_000 };
    heap.mutate(|cx| {
        let list = list.get(cx);
        for value in 0..count {
            list.push(
                cx,
                cx.alloc(SmallInit {
                    value: Cell::new(value),
                }),
            );
        }
    });
    heap.collect();
    heap.mutate(|cx| {
        let list = list.get(cx);
        assert_eq!(list.len(), count as usize);
        for i in [0, 1, count as usize / 2, count as usize - 1] {
            assert_eq!(list.get(cx, i).unwrap().value.get(), i as u32);
        }
    });
}

/// The slots the dead left are handed out again, lowest first, between
/// the live ones.
#[test]
fn free_ranges_between_live_objects_are_reused() {
    let mut heap = old_heap();
    let (kept, dead) = heap.mutate(|cx| {
        let list = HeapVector::<Medium>::new(cx, 0);
        let mut dead = Vec::new();
        for id in 0..12 {
            let object = medium(cx, id);
            // Kept: 0, 1, 2, 6, 7, 8; dead: 3, 4, 5, 9, 10, 11.
            if id % 6 < 3 {
                list.push(cx, object);
            } else {
                dead.push(address(object));
            }
        }
        (Persistent::new(cx, list), dead)
    });
    heap.collect();
    heap.mutate(|cx| {
        let new: Vec<usize> = (0..6).map(|id| address(medium(cx, 100 + id))).collect();
        assert_eq!(new, dead);
        let list = kept.get(cx);
        let ids: Vec<u32> = (0..list.len())
            .map(|i| list.get(cx, i).unwrap().id.get())
            .collect();
        assert_eq!(ids, [0, 1, 2, 6, 7, 8]);
    });
}

/// After a GC, a class allocates on its page with the fewest free slots
/// first.
#[test]
fn the_fullest_page_is_used_first() {
    let mut heap = old_heap();
    // Three pages of `Medium`s (the list's array is of another class).
    let kept = heap.mutate(|cx| {
        let list = HeapVector::<Medium>::new(cx, 2000);
        let mut pages = Vec::new();
        let mut id = 0;
        while pages.len() < 3 {
            let object = medium(cx, id);
            list.push(cx, object);
            let page = address(object) / PAGE_BYTES;
            if !pages.contains(&page) {
                pages.push(page);
            }
            id += 1;
            // A page holds a few hundred: a broken heap may never fill one.
            assert!(id < 2000, "three pages filled");
        }
        Persistent::new(cx, list)
    });
    // The first page loses 10 and the second half; the third holds one.
    let (first, second) = heap.mutate(|cx| {
        let list = kept.get(cx);
        let page_of = |i: usize| address(list.get(cx, i).unwrap()) / PAGE_BYTES;
        let (first, second) = (page_of(0), page_of(list.len() - 1));
        let second = (0..list.len())
            .map(page_of)
            .find(|&page| page != first && page != second)
            .unwrap();
        let mut removed = 0;
        let mut i = 1;
        while i < list.len() {
            let page = page_of(i);
            let even = list.get(cx, i).unwrap().id.get().is_multiple_of(2);
            if (page == first && removed < 10) || (page == second && even) {
                removed += usize::from(page == first);
                list.remove(cx, i);
            } else {
                i += 1;
            }
        }
        (first, second)
    });
    heap.collect();
    heap.mutate(|cx| {
        for id in 0..10 {
            assert_eq!(
                address(medium(cx, 1000 + id)) / PAGE_BYTES,
                first,
                "the fullest page first"
            );
        }
        assert_eq!(address(medium(cx, 2000)) / PAGE_BYTES, second);
    });
}

/// A page emptied by a GC goes to the pool: its memory stays for a GC,
/// another class takes it, and it is discarded if none does (in the GC's
/// pause, on a platform with no workers).
#[test]
fn empty_pages_are_pooled_then_discarded() {
    let mut heap = Heap::with_platform(
        crate::verified(),
        std::sync::Arc::new(elise::InlinePlatform),
    );
    heap.mutate(|cx| {
        for id in 0..10 {
            medium(cx, id);
        }
    });
    assert_eq!(heap.stats().committed_bytes, PAGE_BYTES);
    heap.collect();
    assert_eq!(
        heap.stats().committed_bytes,
        PAGE_BYTES,
        "pooled, not discarded yet"
    );
    // Another class takes the pooled page: nothing new is committed.
    let kept = heap.mutate(|cx| {
        Persistent::new(
            cx,
            cx.alloc(SmallInit {
                value: Cell::new(1),
            }),
        )
    });
    assert_eq!(heap.stats().committed_bytes, PAGE_BYTES);
    heap.collect();
    heap.collect();
    assert_eq!(heap.stats().committed_bytes, PAGE_BYTES, "in use");
    drop(kept);
    heap.collect();
    assert_eq!(heap.stats().committed_bytes, PAGE_BYTES, "pooled for a GC");
    heap.collect();
    assert_eq!(heap.stats().committed_bytes, 0, "discarded");
    // A discarded page is committed again when taken.
    heap.mutate(|cx| {
        let object = medium(cx, 7);
        assert_eq!(object.id.get(), 7);
        assert!(object.words.iter().all(|word| word.get() == 0));
    });
    assert_eq!(heap.stats().committed_bytes, PAGE_BYTES);
}

/// Where the platform has workers, a page a whole GC in the pool goes to
/// one, which gives its memory back: until the safepoint after it did, the
/// page still holds memory and the heap takes a new one instead; then it
/// is discarded, and taken (committed anew) before a new one.
#[test]
fn a_worker_discards_the_pooled_pages() {
    let (mut heap, steps) = crate::concurrent::stepping_heap(0);
    let first = heap.mutate(|cx| {
        let first = address(medium(cx, 0)) / PAGE_BYTES;
        for id in 1..10 {
            medium(cx, id);
        }
        first
    });
    assert_eq!(heap.stats().committed_bytes, PAGE_BYTES);
    heap.collect();
    assert!(!steps.posted(), "pooled for a GC");
    heap.collect();
    assert!(steps.posted(), "handed to the worker");
    assert_eq!(
        heap.stats().committed_bytes,
        PAGE_BYTES,
        "not discarded yet"
    );
    let kept = heap.mutate(|cx| {
        let object = medium(cx, 1);
        assert_ne!(address(object) / PAGE_BYTES, first, "the worker's page");
        Persistent::new(cx, object)
    });
    assert_eq!(heap.stats().committed_bytes, 2 * PAGE_BYTES);
    steps.step();
    assert!(!steps.posted());
    assert_eq!(
        heap.stats().committed_bytes,
        2 * PAGE_BYTES,
        "until a safepoint"
    );
    heap.mutate(|_| {});
    assert_eq!(heap.stats().committed_bytes, PAGE_BYTES, "discarded");
    heap.mutate(|cx| {
        let object = cx.alloc(SmallInit {
            value: Cell::new(3),
        });
        assert_eq!(address(object) / PAGE_BYTES, first, "taken again");
        assert_eq!(object.value.get(), 3);
    });
    assert_eq!(heap.stats().committed_bytes, 2 * PAGE_BYTES);
    drop(kept);
}

/// The worker's pages come back at a collect's and a scavenge's safepoint
/// too; a heap that goes cancels its job (which maps the heap's cage).
#[test]
fn the_discarded_pages_come_back_at_any_safepoint() {
    let (mut heap, steps) = crate::concurrent::stepping_heap(0);
    // A page of garbage, pooled, then handed to the worker.
    let hand = |heap: &mut Heap| {
        heap.mutate(|cx| {
            for id in 0..10 {
                medium(cx, id);
            }
        });
        heap.collect();
        heap.collect();
        assert!(steps.posted());
        assert_eq!(heap.stats().committed_bytes, PAGE_BYTES);
    };
    hand(&mut heap);
    steps.step();
    heap.collect();
    assert_eq!(heap.stats().committed_bytes, 0, "at a collect");
    hand(&mut heap);
    steps.step();
    heap.scavenge();
    assert_eq!(heap.stats().committed_bytes, 0, "at a scavenge");
    hand(&mut heap);
    drop(heap);
    assert!(!steps.posted(), "cancelled");
}

/// A dead large object's pages go to the pool too: small objects take
/// them, and a new large object takes a run of free pages.
#[test]
fn a_large_objects_pages_are_reused() {
    let drops = Rc::new(Cell::new(0));
    let mut heap = Heap::new(crate::verified());
    let address_of_large = heap.mutate(|cx| address(large(cx, None, 0, &drops)));
    heap.collect();
    assert_eq!(drops.get(), 1);
    heap.mutate(|cx| {
        let small = cx.alloc(SmallInit {
            value: Cell::new(3),
        });
        assert_eq!(address(small) / PAGE_BYTES, address_of_large / PAGE_BYTES);
    });
    // A larger one than fits the free pages before: new pages after.
    let bigger = heap.mutate(|cx| {
        let list = HeapVector::<Small>::new(cx, 0);
        for value in 0..if cfg!(miri) { 9000 } else { 100_000 } {
            list.push(
                cx,
                cx.alloc(SmallInit {
                    value: Cell::new(value),
                }),
            );
        }
        Persistent::new(cx, list)
    });
    heap.collect();
    heap.mutate(|cx| assert_eq!(bigger.get(cx).get(cx, 8999).unwrap().value.get(), 8999));
}

/// A large object takes a run of free pages: a free page between pages in
/// use is too short for two, so it goes after them, and the objects on
/// them stay whole.
#[test]
fn a_large_object_skips_pages_in_use() {
    let mut heap = old_heap();
    // A page of `Small`s, one of garbage `Medium`s, one with a `Big`.
    let (small, big) = heap.mutate(|cx| {
        let small = cx.alloc(SmallInit {
            value: Cell::new(5),
        });
        for id in 0..10 {
            medium(cx, id);
        }
        let big = cx.alloc(BigInit {
            next: None,
            words: std::array::from_fn(|i| Cell::new(i as u64 + 1)),
        });
        assert_eq!(address(big) / PAGE_BYTES, address(small) / PAGE_BYTES + 2);
        (Persistent::new(cx, small), Persistent::new(cx, big))
    });
    heap.collect();
    // An array of 70 000 edges: two pages.
    let list = heap.mutate(|cx| Persistent::new(cx, HeapVector::<Small>::new(cx, 70_000)));
    heap.collect();
    heap.mutate(|cx| {
        assert_eq!(small.get(cx).value.get(), 5);
        for (i, word) in big.get(cx).words.iter().enumerate() {
            assert_eq!(word.get(), i as u64 + 1);
        }
        assert!(list.get(cx).is_empty());
    });
}

/// A platform that keeps the GC's trace events: marking runs inline.
#[derive(Default)]
struct Traced(std::sync::Mutex<Vec<String>>);

impl elise::Platform for Traced {
    fn run(&self, job: &dyn elise::Job) {
        elise::InlinePlatform.run(job);
    }

    fn trace_begin(&self, name: &'static str) {
        self.0.lock().unwrap().push(format!("begin {name}"));
    }

    fn trace_end(&self) {
        self.0.lock().unwrap().push("end".to_owned());
    }
}

/// A GC is a pause holding its marking (its weak edges' clearing inside),
/// sweeping and finalizing, each a trace event of the platform's; its
/// record counts what it kept and adds up its time.
#[test]
fn a_gc_reports_its_phases_and_what_it_kept() {
    let platform = std::sync::Arc::new(Traced::default());
    let mut heap = Heap::with_platform(crate::verified(), platform.clone());
    let kept = heap.mutate(|cx| {
        let list = HeapVector::<Small>::new(cx, 0);
        for value in 0..10 {
            list.push(
                cx,
                cx.alloc(SmallInit {
                    value: Cell::new(value),
                }),
            );
            cx.alloc(SmallInit {
                value: Cell::new(value),
            });
        }
        Persistent::new(cx, list)
    });
    heap.collect();
    assert_eq!(
        *platform.0.lock().unwrap(),
        [
            "begin Gc.Pause",
            "begin Gc.Scavenge",
            "end",
            "begin Gc.Mark",
            "begin Gc.Weak",
            "end",
            "end",
            "begin Gc.Sweep",
            "end",
            "begin Gc.Finalize",
            "end",
            "end"
        ]
    );
    let stats = heap.stats();
    let record = stats.last_gc;
    // The list, its array and its ten items.
    assert_eq!(record.marked_objects, 12);
    assert_eq!(record.marked_bytes, stats.used_bytes);
    assert!(record.pause_ns >= record.mark_ns + record.sweep_ns + record.finalize_ns);
    assert_eq!(stats.gc_ns, record.pause_ns);
    heap.collect();
    assert_eq!(
        heap.stats().gc_ns,
        record.pause_ns + heap.stats().last_gc.pause_ns
    );
    drop(kept);
}

/// Edges in an array, and edges to objects of any type.
#[derive(GcObject)]
struct Mixed {
    row: [Member<Small>; 3],
    any: Member<elise::Erased>,
    other: Member<elise::Erased>,
}

/// An object's array of edges and its edges to any type keep what they
/// point to, whatever its type; what they no longer point to goes.
#[test]
fn array_and_erased_edges_keep_their_objects() {
    let mut heap = old_heap();
    let (root, kept_big) = heap.mutate(|cx| {
        let small = |value| {
            cx.alloc(SmallInit {
                value: Cell::new(value),
            })
        };
        let big = cx.alloc(BigInit {
            next: None,
            words: std::array::from_fn(|i| Cell::new(i as u64)),
        });
        let mixed = cx.alloc(MixedInit {
            row: [Some(small(1)), None, Some(small(3))],
            any: Some(big.erase()),
            other: Some(small(4).erase()),
        });
        (Persistent::new(cx, mixed), format!("{big:?}"))
    });
    heap.collect();
    heap.mutate(|cx| {
        let mixed = root.get(cx);
        let values: Vec<Option<u32>> = mixed
            .row
            .iter()
            .map(|edge| edge.get(cx).map(|small| small.value.get()))
            .collect();
        assert_eq!(values, [Some(1), None, Some(3)]);
        assert_eq!(format!("{:?}", mixed.any.get(cx).unwrap()), kept_big);
        mixed.row[0].set(cx, None);
        mixed.other.set(cx, None);
    });
    heap.collect();
    // The big object, the root and one small one are left.
    assert_eq!(heap.stats().last_gc.marked_objects, 3);
}

/// An edge to any type reads back as the type its object is, and as no
/// other, before and after a GC.
#[test]
fn an_erased_edge_downcasts_to_its_objects_type_only() {
    let mut heap = old_heap();
    let root = heap.mutate(|cx| {
        let small = cx.alloc(SmallInit {
            value: Cell::new(7),
        });
        let big = cx.alloc(BigInit {
            next: None,
            words: std::array::from_fn(|i| Cell::new(i as u64)),
        });
        Persistent::new(
            cx,
            cx.alloc(MixedInit {
                row: [None, None, None],
                any: Some(small.erase()),
                other: Some(big.erase()),
            }),
        )
    });
    for _ in 0..2 {
        heap.mutate(|cx| {
            let mixed = root.get(cx);
            let any = mixed.any.get(cx).unwrap();
            assert_eq!(
                any.downcast::<Small>().map(|small| small.value.get()),
                Some(7)
            );
            assert!(any.downcast::<Big>().is_none());
            let other = mixed.other.get(cx).unwrap();
            assert!(other.downcast::<Big>().is_some());
            assert!(other.downcast::<Small>().is_none());
            assert!(other.downcast::<Mixed>().is_none());
        });
        heap.collect();
    }
}

/// A root goes before its heap: a heap dropped with one left says so (a
/// debug build's check), and leaves its tables to the roots, so that
/// their drops (and reads of their index) still work; the strong and the
/// weak roots alike.
#[test]
#[cfg_attr(
    miri,
    ignore = "leaks the heap's tables on purpose: run alone with -Zmiri-ignore-leaks (`test.py miri` does)"
)]
fn a_root_outliving_its_heap_is_caught() {
    let mut heap = Heap::new(HeapOptions::default());
    let (strong, weak) = heap.mutate(|cx| {
        let list = HeapVector::<Small>::new(cx, 0);
        (
            Persistent::new(cx, list),
            elise::WeakPersistent::new(cx, list),
        )
    });
    let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(heap)));
    if cfg!(debug_assertions) {
        let message = *dropped.unwrap_err().downcast::<String>().unwrap();
        assert_eq!(message, "2 roots outlive their heap");
    } else {
        assert!(dropped.is_ok());
    }
    let copy = strong.clone();
    assert_eq!(copy.index(), 1, "the table is still there");
    drop((strong, weak, copy));
}

/// A heap moves with whatever holds it, its roots' tables staying where
/// they are: a root made before the move reads and goes after it (under
/// Miri: no move of the heap invalidates what a root holds).
#[test]
fn a_heap_that_moves_keeps_its_roots() {
    let mut heap = Heap::new(HeapOptions::default());
    let root = heap.mutate(|cx| {
        let list = HeapVector::<Small>::new(cx, 3);
        list.push(
            cx,
            cx.alloc(SmallInit {
                value: Cell::new(7),
            }),
        );
        Persistent::new(cx, list)
    });
    let mut moved = vec![heap];
    let mut heap = moved.pop().unwrap();
    heap.collect();
    let copy = root.clone();
    assert_eq!(heap.mutate(|cx| copy.get(cx).len()), 1);
    drop((root, copy));
    assert_eq!(heap.roots(), 0);
}
