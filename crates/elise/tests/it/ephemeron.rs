//! Ephemerons: a value lives while its key does. A full GC keeps a live
//! key's value, clears a dead key's pair (a value pointing back to its key
//! doesn't keep it), and marks along a chain of pairs whatever order it
//! finds them in; a scavenge keeps a value as an edge would, and clears a
//! pair whose young key died (an old holder's too); a concurrent marking
//! does the same, its value reads marking the key. The heaps but the
//! marking ones run on `InlinePlatform`; every heap runs the verifier,
//! which checks each pair after a GC.

use std::cell::Cell;
use std::sync::Arc;

use elise::{
    EphemeronPair, Gc, GcObject, Heap, HeapOptions, InlinePlatform, Member, Mutation, Persistent,
    WeakPersistent,
};

use crate::concurrent::{finish, start_marking, stepping_heap};

#[derive(GcObject)]
struct Node {
    next: Member<Node>,
    value: Cell<u32>,
}

/// Two ephemerons in a row.
#[derive(GcObject)]
struct Table {
    pairs: [EphemeronPair<Node, Node>; 2],
}

fn node<'gc>(cx: &Mutation<'gc>, value: u32, next: Option<Gc<'gc, Node>>) -> Gc<'gc, Node> {
    cx.alloc(NodeInit {
        next,
        value: Cell::new(value),
    })
}

fn new_table<'gc>(cx: &Mutation<'gc>) -> Gc<'gc, Table> {
    cx.alloc(TableInit {
        pairs: [(None, None), (None, None)],
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

/// Pair `at`'s key's and value's values.
fn pair(cx: &Mutation<'_>, table: Gc<'_, Table>, at: usize) -> (Option<u32>, Option<u32>) {
    let pair = &table.pairs[at];
    (
        pair.key(cx).map(|key| key.value.get()),
        pair.value(cx).map(|value| value.value.get()),
    )
}

/// A live key keeps its value; once the key dies, a full GC clears the
/// pair, and the value dies with it, though it points back to the key.
#[test]
fn a_full_gc_keeps_a_live_keys_value_and_clears_a_dead_keys_pair() {
    let mut heap = heap(0);
    let (table, key, value) = heap.mutate(|cx| {
        let table = new_table(cx);
        let key = node(cx, 1, None);
        // The value points back to its key.
        let value = node(cx, 2, Some(key));
        table.pairs[0].set(cx, Some(key), Some(value));
        (
            Persistent::new(cx, table),
            Persistent::new(cx, key),
            WeakPersistent::new(cx, value),
        )
    });
    heap.collect();
    heap.mutate(|cx| {
        assert_eq!(pair(cx, table.get(cx), 0), (Some(1), Some(2)));
        assert!(value.get(cx).is_some());
    });
    drop(key);
    heap.collect();
    heap.mutate(|cx| {
        assert_eq!(pair(cx, table.get(cx), 0), (None, None));
        assert!(value.get(cx).is_none(), "the value died with its key");
    });
}

/// A chain through two pairs, the second's key the first's value, found
/// in the other order: the second is discovered first (its key not marked
/// yet), and marked from once the first marks its key.
#[test]
fn marking_follows_a_chain_of_pairs_in_any_order() {
    let mut heap = heap(0);
    let (table, root) = heap.mutate(|cx| {
        let table = new_table(cx);
        let root = node(cx, 1, None);
        let middle = node(cx, 2, None);
        table.pairs[1].set(cx, Some(root), Some(middle));
        table.pairs[0].set(cx, Some(middle), Some(node(cx, 3, None)));
        (Persistent::new(cx, table), Persistent::new(cx, root))
    });
    heap.collect();
    heap.mutate(|cx| {
        assert_eq!(pair(cx, table.get(cx), 1), (Some(1), Some(2)));
        assert_eq!(pair(cx, table.get(cx), 0), (Some(2), Some(3)));
    });
    drop(root);
    heap.collect();
    heap.mutate(|cx| {
        assert_eq!(pair(cx, table.get(cx), 1), (None, None));
        assert_eq!(pair(cx, table.get(cx), 0), (None, None));
    });
}

/// A scavenge moves a live young key and its value, and clears a pair
/// whose young key died (its value, kept as an edge keeps, dies the next
/// one).
#[test]
fn a_scavenge_moves_live_pairs_and_clears_dead_keys() {
    let mut heap = heap(4);
    let (table, key, dead_value) = heap.mutate(|cx| {
        let table = new_table(cx);
        let key = node(cx, 1, None);
        table.pairs[0].set(cx, Some(key), Some(node(cx, 2, None)));
        let dead_value = node(cx, 4, None);
        table.pairs[1].set(cx, Some(node(cx, 3, None)), Some(dead_value));
        (
            Persistent::new(cx, table),
            Persistent::new(cx, key),
            WeakPersistent::new(cx, dead_value),
        )
    });
    heap.scavenge();
    heap.mutate(|cx| {
        let table = table.get(cx);
        assert_eq!(pair(cx, table, 0), (Some(1), Some(2)));
        assert!(Gc::ptr_eq(table.pairs[0].key(cx).unwrap(), key.get(cx)));
        assert_eq!(pair(cx, table, 1), (None, None));
        assert!(
            dead_value.get(cx).is_some(),
            "kept one scavenge as an edge's"
        );
    });
    heap.scavenge();
    heap.mutate(|cx| assert!(dead_value.get(cx).is_none()));
}

/// An old holder's pair with a young key: in the ephemeron slot sets, so a
/// scavenge clears it once the key dies, and moves it while it lives.
#[test]
fn a_scavenge_clears_an_old_holders_pair_with_a_dead_young_key() {
    let mut heap = heap(4);
    let table = heap.mutate(|cx| cx.pretenured(|cx| Persistent::new(cx, new_table(cx))));
    let key = heap.mutate(|cx| {
        let key = node(cx, 1, None);
        table.get(cx).pairs[0].set(cx, Some(key), Some(node(cx, 2, None)));
        Persistent::new(cx, key)
    });
    heap.scavenge();
    heap.mutate(|cx| assert_eq!(pair(cx, table.get(cx), 0), (Some(1), Some(2))));
    drop(key);
    heap.scavenge();
    heap.mutate(|cx| assert_eq!(pair(cx, table.get(cx), 0), (None, None)));
}

/// A concurrent marking: a live key's value kept (the key reached after
/// the pair was discovered), a dead key's pair cleared; a value read while
/// marking runs marks its key, so the pair lives on.
#[test]
fn a_concurrent_marking_keeps_live_pairs_and_clears_dead_ones() {
    let (mut heap, steps) = stepping_heap(0);
    let (table, chain, read) = heap.mutate(|cx| {
        cx.pretenured(|cx| {
            let table = new_table(cx);
            // The key, two links down a chain from a root: the marker
            // scans the table first.
            let key = node(cx, 1, None);
            let chain = node(cx, 10, Some(node(cx, 11, Some(key))));
            table.pairs[0].set(cx, Some(key), Some(node(cx, 2, None)));
            table.pairs[1].set(cx, Some(node(cx, 3, None)), Some(node(cx, 4, None)));
            let read = new_table(cx);
            read.pairs[0].set(cx, Some(node(cx, 5, None)), Some(node(cx, 6, None)));
            (
                Persistent::new(cx, table),
                Persistent::new(cx, chain),
                Persistent::new(cx, read),
            )
        })
    });
    start_marking(&mut heap);
    // Read while marking runs, and linked from a new (black) node: its key
    // is marked, so the pair lives on.
    let kept = heap.mutate(|cx| {
        let key = read.get(cx).pairs[0].key(cx);
        Persistent::new(cx, node(cx, 7, key))
    });
    finish(&mut heap, &steps);
    heap.mutate(|cx| {
        assert_eq!(pair(cx, table.get(cx), 0), (Some(1), Some(2)));
        assert_eq!(pair(cx, table.get(cx), 1), (None, None));
        assert_eq!(pair(cx, read.get(cx), 0), (Some(5), Some(6)));
    });
    drop((chain, kept));
}

/// While marking runs, the mutator reads only a pair's value (its key
/// nothing else reaches) and links it from a new (black) node: the read
/// marks the key, so the value is marked through the pair.
#[test]
fn a_value_read_while_marking_is_kept() {
    let (mut heap, steps) = stepping_heap(0);
    let holder = heap.mutate(|cx| {
        cx.pretenured(|cx| {
            let holder = new_table(cx);
            holder.pairs[0].set(cx, Some(node(cx, 1, None)), Some(node(cx, 2, None)));
            Persistent::new(cx, holder)
        })
    });
    start_marking(&mut heap);
    let linked = heap.mutate(|cx| {
        let value = holder.get(cx).pairs[0].value(cx);
        Persistent::new(cx, node(cx, 3, value))
    });
    finish(&mut heap, &steps);
    heap.mutate(|cx| {
        assert_eq!(
            linked.get(cx).next.get(cx).map(|value| value.value.get()),
            Some(2)
        );
    });
}

/// While marking runs, before the marker reaches the holder, the mutator
/// moves a pair's value (nothing else reaches it) to a new (black) node
/// and empties the pair: the barrier keeps the value overwritten.
#[test]
fn a_value_overwritten_while_marking_is_kept() {
    let (mut heap, steps) = stepping_heap(0);
    let (holder, _key) = heap.mutate(|cx| {
        cx.pretenured(|cx| {
            let holder = new_table(cx);
            let key = node(cx, 1, None);
            holder.pairs[0].set(cx, Some(key), Some(node(cx, 2, None)));
            (Persistent::new(cx, holder), Persistent::new(cx, key))
        })
    });
    start_marking(&mut heap);
    let linked = heap.mutate(|cx| {
        let pair = &holder.get(cx).pairs[0];
        let value = pair.value(cx);
        pair.set(cx, pair.key(cx), None);
        Persistent::new(cx, node(cx, 3, value))
    });
    finish(&mut heap, &steps);
    heap.mutate(|cx| {
        assert_eq!(
            linked.get(cx).next.get(cx).map(|value| value.value.get()),
            Some(2)
        );
        assert_eq!(pair(cx, holder.get(cx), 0), (Some(1), None));
    });
}
