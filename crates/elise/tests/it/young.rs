//! The young generation: what a scavenge copies, ages and promotes; the
//! write barrier's slot sets keeping the young objects old ones point to,
//! whichever way the edge was written; the nursery's finalizers; a full
//! nursery sending allocations to the old generation; a full GC emptying
//! the nursery. Every heap runs the verifier after each GC.

use std::cell::Cell;
use std::rc::Rc;

use elise::{
    Gc, GcObject, Heap, HeapOptions, HeapVector, LocalExternal, Member, Mutation, Persistent,
};

/// A node with a finalizer: every one is on the nursery's list of them.
#[derive(GcObject)]
struct Node {
    next: Member<Node>,
    value: Cell<u32>,
    drops: LocalExternal<Drops>,
}

/// 40 000 bytes: a large object, so an old one from its allocation, with
/// a single edge and a row of them.
#[derive(GcObject)]
struct Big {
    child: Member<Node>,
    row: [Member<Node>; 2],
    words: [Cell<u64>; 5000],
}

/// Counts its drops.
struct Drops(Rc<Cell<u32>>);

impl Drop for Drops {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

fn node<'gc>(
    cx: &Mutation<'gc>,
    value: u32,
    next: Option<Gc<'gc, Node>>,
    drops: &Rc<Cell<u32>>,
) -> Gc<'gc, Node> {
    cx.alloc(NodeInit {
        next,
        value: Cell::new(value),
        drops: LocalExternal::new(Drops(drops.clone())),
    })
}

/// An object's address, from its `Debug`.
fn address<T>(object: Gc<'_, T>) -> usize {
    let text = format!("{object:?}");
    usize::from_str_radix(text.trim_start_matches("Gc(0x").trim_end_matches(')'), 16).unwrap()
}

/// The values along `next` from `node`.
fn values<'gc>(cx: &Mutation<'gc>, node: Gc<'gc, Node>) -> Vec<u32> {
    let mut values = vec![node.value.get()];
    let mut next = node.next.get(cx);
    while let Some(node) = next {
        values.push(node.value.get());
        next = node.next.get(cx);
    }
    values
}

/// A heap with a nursery of `pages` pages.
fn heap(pages: usize) -> Heap {
    Heap::new(HeapOptions {
        nursery_bytes: pages * (256 << 10),
        ..crate::verified()
    })
}

/// A rooted node, promoted by a full GC: an old object.
fn old_node(heap: &mut Heap, drops: &Rc<Cell<u32>>) -> Persistent<Node> {
    let root = heap.mutate(|cx| Persistent::new(cx, node(cx, 0, None, drops)));
    heap.collect();
    root
}

#[test]
fn a_scavenge_copies_the_live_and_leaves_the_dead() {
    let drops = Rc::default();
    let mut heap = heap(4);
    let (root, middle) = heap.mutate(|cx| {
        let middle = node(cx, 2, Some(node(cx, 3, None, &drops)), &drops);
        let list = node(cx, 1, Some(middle), &drops);
        for value in 0..100 {
            node(cx, value, None, &drops);
        }
        (Persistent::new(cx, list), Persistent::new(cx, middle))
    });
    let before = heap.mutate(|cx| address(root.get(cx)));
    let used = heap.stats().young_bytes;
    heap.scavenge();
    let stats = heap.stats();
    assert_eq!(stats.scavenges, 1);
    assert_eq!(stats.gcs, 0);
    assert_eq!(stats.last_scavenge.survived_objects, 3);
    assert_eq!(stats.last_scavenge.promoted_objects, 0);
    // On one thread, its buffer's unused end goes back: the nursery holds
    // the copies alone.
    assert_eq!(stats.young_bytes, stats.last_scavenge.survived_bytes);
    assert_eq!(
        stats.last_scavenge.survived_bytes * 103,
        used * 3,
        "the copies are as large"
    );
    // The dead are finalized, never copied.
    assert_eq!(stats.last_scavenge.finalized, 100);
    assert_eq!(drops.get(), 100);
    heap.mutate(|cx| {
        let list = root.get(cx);
        assert_ne!(address(list), before, "the survivors moved");
        assert_eq!(values(cx, list), [1, 2, 3]);
        // Copied once: the edge and the root reach the same copy.
        assert!(Gc::ptr_eq(list.next.get(cx).unwrap(), middle.get(cx)));
    });
}

#[test]
fn a_survivor_is_promoted_by_its_second_scavenge() {
    let drops = Rc::default();
    let mut heap = heap(4);
    let root = heap.mutate(|cx| Persistent::new(cx, node(cx, 7, None, &drops)));
    heap.scavenge();
    let stats = heap.stats();
    assert_eq!(
        (
            stats.last_scavenge.survived_objects,
            stats.last_scavenge.promoted_objects
        ),
        (1, 0)
    );
    assert!(stats.young_bytes > 0);
    heap.scavenge();
    let stats = heap.stats();
    assert_eq!(
        (
            stats.last_scavenge.survived_objects,
            stats.last_scavenge.promoted_objects
        ),
        (0, 1)
    );
    assert_eq!(stats.young_bytes, 0);
    assert!(stats.used_bytes > 0, "the old generation holds it");
    // Old now: a scavenge leaves it where it is.
    let old = heap.mutate(|cx| address(root.get(cx)));
    heap.scavenge();
    heap.mutate(|cx| {
        assert_eq!(address(root.get(cx)), old);
        assert_eq!(root.get(cx).value.get(), 7);
    });
    assert_eq!(drops.get(), 0);
}

/// `Member::set` on an old object: the young object is recorded, kept, and
/// let go once the edge is cleared.
#[test]
fn an_old_objects_edge_keeps_a_young_one() {
    let drops = Rc::default();
    let mut heap = heap(4);
    let root = old_node(&mut heap, &drops);
    heap.mutate(|cx| {
        let child = node(cx, 8, Some(node(cx, 9, None, &drops)), &drops);
        root.get(cx).next.set(cx, Some(child));
    });
    heap.scavenge();
    assert_eq!(heap.stats().last_scavenge.survived_objects, 2);
    heap.mutate(|cx| assert_eq!(values(cx, root.get(cx)), [0, 8, 9]));
    // Promoted by the next: an old edge to an old object, not recorded.
    heap.scavenge();
    assert_eq!(heap.stats().last_scavenge.promoted_objects, 2);
    heap.mutate(|cx| {
        root.get(cx).next.set(cx, Some(node(cx, 10, None, &drops)));
    });
    heap.scavenge();
    heap.mutate(|cx| {
        assert_eq!(values(cx, root.get(cx)), [0, 10]);
        root.get(cx).next.set(cx, None);
    });
    // The young one dies with the edge; the old ones wait for a full GC.
    heap.scavenge();
    assert_eq!(heap.stats().last_scavenge.survived_objects, 0);
    assert_eq!(drops.get(), 1);
    heap.collect();
    assert_eq!(drops.get(), 3);
}

/// An old `HeapVector`'s array, written by push, set and remove.
#[test]
fn an_old_vectors_edges_keep_young_objects() {
    let drops = Rc::default();
    let mut heap = heap(4);
    let list = heap.mutate(|cx| Persistent::new(cx, HeapVector::<Node>::new(cx, 2)));
    heap.collect();
    heap.mutate(|cx| {
        let list = list.get(cx);
        // Past its capacity: a new array, young, takes the edges.
        for value in 0..5 {
            list.push(cx, node(cx, value, None, &drops));
        }
    });
    // The array is promoted with its elements by the second scavenge.
    heap.scavenge();
    heap.scavenge();
    heap.mutate(|cx| {
        let list = list.get(cx);
        list.set(cx, 1, node(cx, 11, None, &drops));
        list.push(cx, node(cx, 15, None, &drops));
        list.remove(cx, 0);
    });
    heap.scavenge();
    heap.mutate(|cx| {
        let list = list.get(cx);
        let values: Vec<u32> = (0..list.len())
            .map(|i| list.get(cx, i).unwrap().value.get())
            .collect();
        assert_eq!(values, [11, 2, 3, 4, 15]);
    });
    assert_eq!(
        drops.get(),
        0,
        "the replaced and removed are old: a full GC's"
    );
    heap.collect();
    assert_eq!(drops.get(), 2);
}

/// Objects allocated old with edges to young ones: a large object, and one
/// allocated past a full nursery.
#[test]
fn an_old_objects_edges_written_at_its_allocation_keep_young_ones() {
    let drops = Rc::default();
    let mut heap = heap(1);
    let (big, late) = heap.mutate(|cx| {
        let first = node(cx, 1, None, &drops);
        let big = cx.alloc(BigInit {
            child: Some(first),
            row: [None, Some(node(cx, 4, None, &drops))],
            words: std::array::from_fn(|i| Cell::new(i as u64)),
        });
        let second = node(cx, 2, None, &drops);
        // Past the nursery's one page: these go to the old generation.
        for value in 0..20_000 {
            node(cx, value, None, &drops);
        }
        let late = node(cx, 3, Some(second), &drops);
        (Persistent::new(cx, big), Persistent::new(cx, late))
    });
    assert!(heap.should_collect(), "the nursery is full");
    let old = heap.mutate(|cx| address(late.get(cx)));
    assert_eq!(heap.stats().scavenges, 1, "the safepoint scavenged");
    heap.mutate(|cx| {
        assert_eq!(address(late.get(cx)), old, "allocated old");
        assert_eq!(values(cx, late.get(cx)), [3, 2]);
        assert_eq!(big.get(cx).child.get(cx).unwrap().value.get(), 1);
        assert_eq!(big.get(cx).row[1].get(cx).unwrap().value.get(), 4);
    });
    assert!(heap.stats().last_scavenge.survived_objects >= 2);
}

/// An operation with no safepoint does no GC's work, its nursery full or
/// not: what it reads (and allocates) is where it was; the next `mutate`
/// scavenges.
#[test]
fn an_operation_without_a_safepoint_leaves_the_gc_to_the_next() {
    let drops = Rc::default();
    let mut heap = heap(1);
    let kept = heap.mutate(|cx| {
        let kept = Persistent::new(cx, node(cx, 7, None, &drops));
        for value in 0..20_000 {
            node(cx, value, None, &drops);
        }
        kept
    });
    assert!(heap.should_collect(), "the nursery is full");
    let before = heap.read(|cx| {
        node(cx, 8, None, &drops);
        address(kept.get(cx))
    });
    assert_eq!(heap.stats().scavenges, 0, "no safepoint, no scavenge");
    assert!(heap.should_collect());
    let after = heap.mutate(|cx| {
        assert_eq!(kept.get(cx).value.get(), 7);
        address(kept.get(cx))
    });
    assert_eq!(heap.stats().scavenges, 1, "the next operation's safepoint");
    assert_ne!(before, after, "copied by the scavenge");
}

/// Operations without a safepoint share the heap: one may run in another
/// (each allocating), with no GC between, the outer's objects where they
/// were.
#[test]
fn operations_without_a_safepoint_may_overlap() {
    let drops = Rc::default();
    let heap = heap(1);
    let (outer, inner) = heap.read(|cx| {
        let outer = node(cx, 1, None, &drops);
        let inner = heap.read(|cx| address(node(cx, 2, None, &drops)));
        assert_eq!(outer.value.get(), 1);
        (address(outer), inner)
    });
    assert_ne!(outer, inner);
    assert_eq!(heap.stats().scavenges, 0);
}

/// 256 bytes and 16 with their headers: what fills a nursery's page to
/// the byte.
#[derive(GcObject)]
struct Pad {
    words: [Cell<u64>; 31],
}

#[derive(GcObject)]
struct Word {
    word: Cell<u64>,
}

/// A vector whose store (16 bytes) takes the nursery's last bytes is
/// allocated old itself: its edge to the young store is recorded.
#[test]
fn a_vector_allocated_old_keeps_its_young_store() {
    let drops = Rc::default();
    let mut heap = heap(1);
    let list = heap.mutate(|cx| {
        // 1023 pads and 15 words: a page but 16 bytes.
        for _ in 0..1023 {
            cx.alloc(PadInit {
                words: std::array::from_fn(|_| Cell::new(0)),
            });
        }
        for _ in 0..15 {
            cx.alloc(WordInit { word: Cell::new(0) });
        }
        Persistent::new(cx, HeapVector::<Node>::new(cx, 0))
    });
    assert!(heap.should_collect(), "the vector went past the nursery");
    heap.scavenge();
    heap.mutate(|cx| {
        let list = list.get(cx);
        list.push(cx, node(cx, 1, None, &drops));
        assert_eq!(list.get(cx, 0).unwrap().value.get(), 1);
    });
}

#[test]
fn the_nurserys_dead_are_finalized_once() {
    let drops = Rc::default();
    let mut heap = heap(4);
    let root = heap.mutate(|cx| {
        for value in 0..10 {
            node(cx, value, None, &drops);
        }
        Persistent::new(cx, node(cx, 10, None, &drops))
    });
    heap.scavenge();
    assert_eq!(drops.get(), 10);
    heap.scavenge();
    assert_eq!(drops.get(), 10);
    drop(root);
    // Promoted by the second scavenge, it is the old generation's to
    // finalize.
    heap.scavenge();
    assert_eq!(drops.get(), 10);
    heap.collect();
    heap.collect();
    assert_eq!(drops.get(), 11);
}

#[test]
fn a_full_gc_empties_the_nursery() {
    let drops = Rc::default();
    let mut heap = heap(4);
    let root = heap.mutate(|cx| {
        node(cx, 0, None, &drops);
        Persistent::new(cx, node(cx, 1, Some(node(cx, 2, None, &drops)), &drops))
    });
    heap.collect();
    let stats = heap.stats();
    assert_eq!(stats.young_bytes, 0);
    assert_eq!(stats.last_scavenge.promoted_objects, 2);
    assert_eq!(stats.last_gc.marked_objects, 2);
    assert_eq!(drops.get(), 1);
    heap.mutate(|cx| assert_eq!(values(cx, root.get(cx)), [1, 2]));
}

/// The heap goes with young objects in it: each finalizer runs once.
#[test]
fn the_nurserys_objects_are_finalized_with_the_heap() {
    let drops = Rc::default();
    let mut heap = heap(4);
    let root = heap.mutate(|cx| {
        node(cx, 0, None, &drops);
        Persistent::new(cx, node(cx, 1, None, &drops))
    });
    heap.scavenge();
    heap.mutate(|cx| {
        node(cx, 2, None, &drops);
    });
    // A root goes before its heap; no GC between, the object still there.
    drop(root);
    drop(heap);
    assert_eq!(drops.get(), 3);
}

/// A nursery of two pages growing to eight: it doubles each time the
/// scavenges' survivors add up to what it holds, and no further; a full GC
/// after scavenges of garbage halves it, down to two; a full GC's own
/// scavenge (every survivor promoted) counts for neither.
#[test]
fn the_nursery_grows_with_its_survivors_and_shrinks_after_garbage() {
    const PAGE: usize = 256 << 10;
    let drops = Rc::default();
    let mut heap = Heap::new(HeapOptions {
        nursery_bytes: 2 * PAGE,
        max_nursery_bytes: 8 * PAGE,
        ..crate::verified()
    });
    assert_eq!(heap.stats().nursery_bytes, 2 * PAGE);
    // A rooted list of 192 KiB, its nodes kept (copied, then promoted):
    // 384 KiB kept a round.
    let rooted_list = |heap: &mut Heap| {
        heap.mutate(|cx| {
            let mut list = None;
            for value in 0..8000 {
                list = Some(node(cx, value, list, &drops));
            }
            Persistent::new(cx, list.unwrap())
        })
    };
    let mut roots = Vec::new();
    let mut sizes = Vec::new();
    for _ in 0..16 {
        roots.push(rooted_list(&mut heap));
        heap.scavenge();
        heap.scavenge();
        sizes.push(heap.stats().nursery_bytes / PAGE);
    }
    assert_eq!(sizes.first(), Some(&2), "384 KiB kept: not yet");
    assert!(sizes.windows(2).all(|pair| pair[1] >= pair[0]), "{sizes:?}");
    assert!(sizes.contains(&4), "{sizes:?}");
    assert_eq!(sizes.last(), Some(&8), "{sizes:?}");
    assert!(
        sizes.iter().filter(|&&pages| pages == 8).count() > 3,
        "it stays at its largest: {sizes:?}"
    );
    // A full GC after scavenges that kept much: as it was. Its own
    // scavenge promotes a young list, which doesn't count.
    roots.push(rooted_list(&mut heap));
    heap.collect();
    assert_eq!(heap.stats().nursery_bytes, 8 * PAGE);
    // Garbage only, then a full GC, three times: halved, halved, and no
    // smaller than it started.
    for expected in [4, 2, 2] {
        for _ in 0..4 {
            heap.mutate(|cx| {
                for value in 0..4000 {
                    node(cx, value, None, &drops);
                }
            });
            heap.scavenge();
        }
        heap.collect();
        assert_eq!(heap.stats().nursery_bytes, expected * PAGE);
    }
    drop(roots);
}

/// A nursery that shrank counts its survivors over: what the larger one
/// counted (384 KiB, more than the smaller holds) doesn't double it back at
/// the next scavenge.
#[test]
fn a_shrunk_nursery_counts_over() {
    const PAGE: usize = 256 << 10;
    let drops = Rc::default();
    let mut heap = Heap::new(HeapOptions {
        nursery_bytes: PAGE,
        max_nursery_bytes: 2 * PAGE,
        ..crate::verified()
    });
    // Seven rounds of 192 KiB kept: it doubles at the second (384 KiB),
    // starts over at its largest at the fifth (576 KiB), and has 384 KiB
    // counted after the seventh.
    let mut roots = Vec::new();
    for _ in 0..7 {
        roots.push(heap.mutate(|cx| {
            let mut list = None;
            for value in 0..4000 {
                list = Some(node(cx, value, list, &drops));
            }
            Persistent::new(cx, list.unwrap())
        }));
        heap.scavenge();
        heap.scavenge();
    }
    assert_eq!(heap.stats().nursery_bytes, 2 * PAGE);
    // A full GC after scavenges that kept much: as it was, its counts
    // since the last full GC start over (its survivors' don't).
    heap.collect();
    assert_eq!(heap.stats().nursery_bytes, 2 * PAGE);
    let garbage = |heap: &mut Heap| {
        heap.mutate(|cx| {
            for value in 0..4000 {
                node(cx, value, None, &drops);
            }
        });
        heap.scavenge();
    };
    garbage(&mut heap);
    heap.collect();
    assert_eq!(heap.stats().nursery_bytes, PAGE);
    garbage(&mut heap);
    assert_eq!(
        heap.stats().nursery_bytes,
        PAGE,
        "it kept nothing: no growing"
    );
    drop(roots);
}

/// `pretenured`: its allocations are old from the start (a scavenge leaves
/// them where they are), and their edges to young objects are recorded.
#[test]
fn pretenured_objects_are_old() {
    let drops = Rc::default();
    let mut heap = heap(4);
    let (root, address_before) = heap.mutate(|cx| {
        let young = node(cx, 2, None, &drops);
        let old = cx.pretenured(|cx| node(cx, 1, Some(young), &drops));
        // Back to the nursery after.
        let after = node(cx, 3, None, &drops);
        old.next.get(cx).unwrap().next.set(cx, Some(after));
        (Persistent::new(cx, old), address(old))
    });
    let young_before = heap.stats().young_bytes;
    heap.scavenge();
    let stats = heap.stats();
    // The young two copied, and only they were ever in the nursery.
    assert_eq!(stats.last_scavenge.survived_objects, 2);
    assert_eq!(stats.young_bytes, young_before);
    heap.mutate(|cx| {
        let old = root.get(cx);
        assert_eq!(address(old), address_before, "old from the start");
        assert_eq!(values(cx, old), [1, 2, 3]);
    });
}

/// A panic in `pretenured` ends it: the next allocations are young again.
#[test]
fn a_panic_ends_the_pretenuring() {
    let drops = Rc::default();
    let mut heap = heap(4);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        heap.mutate(|cx| cx.pretenured(|_| panic!("in the closure")));
    }));
    assert!(result.is_err());
    heap.mutate(|cx| {
        node(cx, 1, None, &drops);
    });
    assert!(heap.stats().young_bytes > 0);
}
