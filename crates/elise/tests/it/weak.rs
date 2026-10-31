//! Weak edges and weak roots: a full GC clears those whose old object
//! died and keeps the rest; a scavenge clears those whose young object
//! nothing strong kept (an old holder's too, through the weak slot sets)
//! and points the rest to the copies; while a concurrent marking runs, an
//! object read through a weak edge is marked (the mutator holds it now),
//! and an object allocated black keeps its weak edges to the end. The
//! heaps but the marking ones run on `InlinePlatform` (no marking
//! alongside), and every heap runs the verifier, which checks each weak
//! edge after a GC.

use std::cell::Cell;
use std::sync::Arc;

use elise::{
    Gc, GcObject, Heap, HeapOptions, InlinePlatform, Member, Mutation, Persistent, WeakMember,
    WeakPersistent,
};

use crate::concurrent::{finish, start_marking, stepping_heap};

#[derive(GcObject)]
struct Node {
    next: Member<Node>,
    value: Cell<u32>,
}

/// What holds weak edges: one, and a row of them.
#[derive(GcObject)]
struct Holder {
    weak: WeakMember<Node>,
    row: [WeakMember<Node>; 2],
    strong: Member<Node>,
}

fn node<'gc>(cx: &Mutation<'gc>, value: u32) -> Gc<'gc, Node> {
    cx.alloc(NodeInit {
        next: None,
        value: Cell::new(value),
    })
}

fn new_holder<'gc>(cx: &Mutation<'gc>, weak: Option<Gc<'gc, Node>>) -> Gc<'gc, Holder> {
    cx.alloc(HolderInit {
        weak,
        row: [None, None],
        strong: None,
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

fn value(node: Option<Gc<'_, Node>>) -> Option<u32> {
    node.map(|node| node.value.get())
}

/// A full GC clears the weak edges (one alone, and in a row) to the old
/// objects nothing strong keeps, and keeps those to the rest.
#[test]
fn a_full_gc_clears_weak_edges_to_dead_old_objects() {
    let mut heap = heap(0);
    let (holder, alive) = heap.mutate(|cx| {
        let alive = node(cx, 1);
        let holder = new_holder(cx, Some(node(cx, 2)));
        holder.row[0].set(cx, Some(alive));
        holder.row[1].set(cx, Some(node(cx, 3)));
        (Persistent::new(cx, holder), Persistent::new(cx, alive))
    });
    heap.collect();
    heap.mutate(|cx| {
        let holder = holder.get(cx);
        assert_eq!(value(holder.weak.get(cx)), None);
        assert_eq!(value(holder.row[0].get(cx)), Some(1));
        assert_eq!(value(holder.row[1].get(cx)), None);
    });
    drop(alive);
    heap.collect();
    heap.mutate(|cx| assert_eq!(value(holder.get(cx).row[0].get(cx)), None));
}

/// A scavenge clears a young holder's weak edge to a young object nothing
/// strong keeps, and points one to a survivor to its copy.
#[test]
fn a_scavenge_clears_and_moves_a_young_holders_weak_edges() {
    let mut heap = heap(4);
    let (holder, alive) = heap.mutate(|cx| {
        let alive = node(cx, 1);
        let holder = new_holder(cx, Some(node(cx, 2)));
        holder.row[0].set(cx, Some(alive));
        (Persistent::new(cx, holder), Persistent::new(cx, alive))
    });
    heap.scavenge();
    heap.mutate(|cx| {
        let holder = holder.get(cx);
        assert_eq!(value(holder.weak.get(cx)), None);
        let moved = holder.row[0].get(cx).unwrap();
        assert_eq!(moved.value.get(), 1);
        assert!(Gc::ptr_eq(moved, alive.get(cx)), "moved with its object");
    });
}

/// An old holder's weak edges to young objects: in the weak slot sets, so a
/// scavenge clears the dead one and moves the other, and again once it is
/// promoted.
#[test]
fn a_scavenge_clears_and_moves_an_old_holders_weak_edges() {
    let mut heap = heap(4);
    let holder = heap.mutate(|cx| cx.pretenured(|cx| Persistent::new(cx, new_holder(cx, None))));
    let alive = heap.mutate(|cx| {
        let alive = node(cx, 1);
        let holder = holder.get(cx);
        holder.weak.set(cx, Some(node(cx, 2)));
        holder.row[1].set(cx, Some(alive));
        Persistent::new(cx, alive)
    });
    // Copied, then promoted.
    for _ in 0..2 {
        heap.scavenge();
        heap.mutate(|cx| {
            let holder = holder.get(cx);
            assert_eq!(value(holder.weak.get(cx)), None);
            assert!(Gc::ptr_eq(holder.row[1].get(cx).unwrap(), alive.get(cx)));
        });
    }
    // Old now: a full GC's to judge.
    drop(alive);
    heap.scavenge();
    heap.mutate(|cx| assert_eq!(value(holder.get(cx).row[1].get(cx)), Some(1)));
    heap.collect();
    heap.mutate(|cx| assert_eq!(value(holder.get(cx).row[1].get(cx)), None));
}

/// A holder promoted with its weak edge to a young object (the scavenge
/// puts the edge in a weak slot set), and one written after (the barrier
/// does): the next scavenges still clear them once their objects die.
#[test]
fn a_promoted_holders_weak_edges_are_remembered() {
    let mut heap = heap(4);
    let holder = heap.mutate(|cx| Persistent::new(cx, new_holder(cx, None)));
    heap.scavenge();
    // Aged; its weak edge to a new object, which stays young as it is
    // promoted.
    let target = heap.mutate(|cx| {
        let target = node(cx, 1);
        holder.get(cx).weak.set(cx, Some(target));
        Persistent::new(cx, target)
    });
    heap.scavenge();
    heap.mutate(|cx| assert_eq!(value(holder.get(cx).weak.get(cx)), Some(1)));
    drop(target);
    heap.scavenge();
    heap.mutate(|cx| assert_eq!(value(holder.get(cx).weak.get(cx)), None));
    // Old now: written through the barrier.
    let target = heap.mutate(|cx| {
        let target = node(cx, 2);
        holder.get(cx).row[0].set(cx, Some(target));
        Persistent::new(cx, target)
    });
    heap.scavenge();
    heap.mutate(|cx| assert_eq!(value(holder.get(cx).row[0].get(cx)), Some(2)));
    drop(target);
    heap.scavenge();
    heap.mutate(|cx| assert_eq!(value(holder.get(cx).row[0].get(cx)), None));
}

/// A weak root: moved with its young object, cleared by a scavenge once
/// it dies young, or by a full GC once it dies old.
#[test]
fn weak_roots_are_moved_and_cleared() {
    let mut heap = heap(4);
    let (young_dead, alive, root) = heap.mutate(|cx| {
        let alive = node(cx, 1);
        (
            WeakPersistent::new(cx, node(cx, 2)),
            WeakPersistent::new(cx, alive),
            Persistent::new(cx, alive),
        )
    });
    heap.scavenge();
    heap.mutate(|cx| {
        assert_eq!(value(young_dead.get(cx)), None);
        assert!(Gc::ptr_eq(alive.get(cx).unwrap(), root.get(cx)));
    });
    heap.collect();
    heap.mutate(|cx| assert_eq!(value(alive.get(cx)), Some(1)));
    drop(root);
    heap.collect();
    heap.mutate(|cx| assert_eq!(value(alive.get(cx)), None));
}

/// While marking runs, the mutator reads the holder's weak edge to an
/// object nothing strong reaches, and links it from a new (black, never
/// scanned) object: the read marks it. Else the marker, scanning the holder
/// later, would find it unmarked and the end clear the edge and free it.
#[test]
fn a_weak_read_while_marking_keeps_its_object() {
    let (mut heap, steps) = stepping_heap(0);
    let holder = heap
        .mutate(|cx| cx.pretenured(|cx| Persistent::new(cx, new_holder(cx, Some(node(cx, 7))))));
    start_marking(&mut heap);
    let linked = heap.mutate(|cx| {
        let target = holder.get(cx).weak.get(cx);
        let linked = node(cx, 1);
        linked.next.set(cx, target);
        Persistent::new(cx, linked)
    });
    finish(&mut heap, &steps);
    heap.mutate(|cx| {
        assert_eq!(value(linked.get(cx).next.get(cx)), Some(7));
        assert_eq!(value(holder.get(cx).weak.get(cx)), Some(7));
    });
}

/// The same through a weak root: its read marks its object.
#[test]
fn a_weak_root_read_while_marking_keeps_its_object() {
    let (mut heap, steps) = stepping_heap(0);
    // A root, so that the marking has work until its job runs: with none,
    // it is done at once, and the next safepoint finishes it.
    let (weak, _anchor) = heap.mutate(|cx| {
        cx.pretenured(|cx| {
            (
                WeakPersistent::new(cx, node(cx, 7)),
                Persistent::new(cx, node(cx, 0)),
            )
        })
    });
    start_marking(&mut heap);
    let linked = heap.mutate(|cx| {
        let linked = node(cx, 1);
        linked.next.set(cx, weak.get(cx));
        Persistent::new(cx, linked)
    });
    finish(&mut heap, &steps);
    heap.mutate(|cx| {
        assert_eq!(value(linked.get(cx).next.get(cx)), Some(7));
        assert_eq!(value(weak.get(cx)), Some(7));
    });
}

/// A concurrent marking clears the weak edges to the snapshot's dead, and
/// a black object's weak edge to an object that dies while marking runs
/// is cleared by the next one (it floats this time).
#[test]
fn a_concurrent_marking_clears_weak_edges_and_black_ones_next() {
    let (mut heap, steps) = stepping_heap(0);
    let holder = heap
        .mutate(|cx| cx.pretenured(|cx| Persistent::new(cx, new_holder(cx, Some(node(cx, 7))))));
    start_marking(&mut heap);
    let black = heap.mutate(|cx| Persistent::new(cx, new_holder(cx, Some(node(cx, 8)))));
    finish(&mut heap, &steps);
    heap.mutate(|cx| {
        assert_eq!(value(holder.get(cx).weak.get(cx)), None);
        assert_eq!(value(black.get(cx).weak.get(cx)), Some(8), "floating");
    });
    start_marking(&mut heap);
    finish(&mut heap, &steps);
    heap.mutate(|cx| assert_eq!(value(black.get(cx).weak.get(cx)), None));
}
