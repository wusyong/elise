//! The id table: an id finds its object while it lives, then nothing,
//! and is never given again; a full GC clears the dead and frees a chunk
//! left empty; a scavenge moves the young and clears the dead, through its
//! list of young ids; a lookup checks the object's type (any for
//! `Erased`); an object found while a concurrent marking runs is marked.
//! The heaps but the marking one run on `InlinePlatform`; every heap runs
//! the verifier, which checks the table after each GC.

use std::cell::Cell;
use std::sync::Arc;

use elise::{
    Erased, Gc, GcObject, Heap, HeapOptions, Id, InlinePlatform, Member, Mutation, Persistent,
};

use crate::concurrent::{finish, start_marking, stepping_heap};

#[derive(GcObject)]
struct Node {
    next: Member<Node>,
    value: Cell<u32>,
}

#[derive(GcObject)]
struct Other {
    value: Cell<u32>,
}

fn node<'gc>(cx: &Mutation<'gc>, value: u32, next: Option<Gc<'gc, Node>>) -> Gc<'gc, Node> {
    cx.alloc(NodeInit {
        next,
        value: Cell::new(value),
    })
}

/// A heap with a nursery of `pages` pages, marking only in its pauses.
fn heap(pages: usize) -> Heap {
    Heap::with_platform(
        HeapOptions {
            nursery_bytes: pages * (256 << 10),
            ..crate::verified()
        },
        Arc::new(InlinePlatform),
    )
}

fn value(cx: &Mutation<'_>, id: Id) -> Option<u32> {
    cx.by_id::<Node>(id).map(|node| node.value.get())
}

/// A full GC clears a dead object's id and keeps a live one's; ids are
/// never given again, so a dead id finds nothing for good.
#[test]
fn a_full_gc_clears_dead_ids_and_ids_are_never_given_again() {
    let mut heap = heap(0);
    let (alive, dead, root) = heap.mutate(|cx| {
        let alive = node(cx, 1, None);
        (
            cx.new_id(alive),
            cx.new_id(node(cx, 2, None)),
            Persistent::new(cx, alive),
        )
    });
    heap.collect();
    heap.mutate(|cx| {
        assert_eq!(value(cx, alive), Some(1));
        assert_eq!(value(cx, dead), None);
        let next = cx.new_id(node(cx, 3, None));
        assert!(
            next.get() > alive.get().max(dead.get()),
            "a new id, never an old one"
        );
        assert_eq!(value(cx, Id::from_raw(0)), None);
        assert_eq!(value(cx, Id::from_raw(next.get() + 100)), None);
    });
    drop(root);
}

/// A chunk whose objects all died is freed; one with a live object stays.
#[test]
fn a_full_gc_frees_a_chunk_left_empty() {
    let mut heap = heap(0);
    // Two chunks' worth: the first all garbage, one of the second kept.
    let root = heap.mutate(|cx| {
        let mut kept = None;
        for i in 0..2048 {
            let node = node(cx, i, None);
            cx.new_id(node);
            if i == 1500 {
                kept = Some(Persistent::new(cx, node));
            }
        }
        kept.unwrap()
    });
    assert_eq!(heap.stats().id_chunks, 3, "ids 1 to 2048 span three chunks");
    heap.collect();
    assert_eq!(heap.stats().id_chunks, 1);
    drop(root);
    heap.collect();
    assert_eq!(heap.stats().id_chunks, 0);
}

/// A scavenge moves a young object's id with it and clears a dead one's;
/// once promoted, it is the full GC's.
#[test]
fn a_scavenge_moves_and_clears_young_ids() {
    let mut heap = heap(4);
    let (alive, dead, root) = heap.mutate(|cx| {
        let alive = node(cx, 1, None);
        (
            cx.new_id(alive),
            cx.new_id(node(cx, 2, None)),
            Persistent::new(cx, alive),
        )
    });
    for _ in 0..2 {
        heap.scavenge();
        heap.mutate(|cx| {
            assert!(Gc::ptr_eq(cx.by_id::<Node>(alive).unwrap(), root.get(cx)));
            assert_eq!(value(cx, dead), None);
        });
    }
    drop(root);
    heap.scavenge();
    heap.mutate(|cx| assert_eq!(value(cx, alive), Some(1), "old now"));
    heap.collect();
    heap.mutate(|cx| assert_eq!(value(cx, alive), None));
}

/// A lookup of another type finds nothing; `Erased` finds any object.
#[test]
fn a_lookup_checks_the_type() {
    let mut heap = heap(0);
    heap.mutate(|cx| {
        let other = cx.alloc(OtherInit {
            value: Cell::new(5),
        });
        let id = cx.new_id(other);
        assert!(cx.by_id::<Node>(id).is_none());
        assert_eq!(
            cx.by_id::<Other>(id).map(|other| other.value.get()),
            Some(5)
        );
        assert!(cx.by_id::<Erased>(id).is_some());
    });
}

/// While marking runs, an object nothing strong reaches is found by its id
/// and linked from a new (black) node: the lookup marks it.
#[test]
fn a_lookup_while_marking_keeps_its_object() {
    let (mut heap, steps) = stepping_heap(0);
    let (id, _anchor) = heap.mutate(|cx| {
        cx.pretenured(|cx| {
            (
                cx.new_id(node(cx, 7, None)),
                Persistent::new(cx, node(cx, 0, None)),
            )
        })
    });
    start_marking(&mut heap);
    let linked = heap.mutate(|cx| {
        let found = cx.by_id::<Node>(id);
        Persistent::new(cx, node(cx, 1, found))
    });
    finish(&mut heap, &steps);
    heap.mutate(|cx| {
        assert_eq!(
            linked.get(cx).next.get(cx).map(|node| node.value.get()),
            Some(7)
        );
        assert_eq!(value(cx, id), Some(7));
    });
}
