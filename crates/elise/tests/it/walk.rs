//! The walk of every object for a heap snapshot (`Heap::walk`): it finds
//! the old generation's objects (size classes' pages, around the ranges
//! allocation hasn't used yet, and large objects) and the nursery's, each
//! with its type, size and edges by its fields' names; after a full GC,
//! none of the garbage; while a marking runs, what it hands out stays
//! valid. Every heap runs the verifier.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;

use elise::{
    EdgeKind, EphemeronPair, GcObject, Heap, HeapOptions, HeapVector, InlinePlatform, JsEdge,
    Member, Persistent, ThreadPool, WeakMember,
};

use crate::concurrent::{finish, start_marking, stepping_heap};

#[derive(GcObject)]
struct Node {
    next: Member<Node>,
    weak: WeakMember<Node>,
    value: Cell<u32>,
}

#[derive(GcObject)]
struct Holder {
    children: Member<HeapVector<Node>>,
    pair: EphemeronPair<Node, Node>,
    wrapper: JsEdge,
}

/// Larger than a slot (32 KiB): pages of its own.
#[derive(GcObject)]
struct Large {
    next: Member<Node>,
    bytes: [u8; 40 << 10],
}

fn node<'gc>(
    cx: &elise::Mutation<'gc>,
    value: u32,
    next: Option<elise::Gc<'gc, Node>>,
) -> elise::Gc<'gc, Node> {
    cx.alloc(NodeInit {
        next,
        weak: None,
        value: Cell::new(value),
    })
}

fn heap(nursery_bytes: usize) -> Heap {
    Heap::with_platform(
        HeapOptions {
            nursery_bytes,
            ..crate::verified()
        },
        Arc::new(InlinePlatform),
    )
}

/// The walk's objects by type name: how many of each.
fn counts(heap: &mut Heap) -> HashMap<&'static str, usize> {
    heap.walk(|_, walk| {
        let mut counts = HashMap::new();
        for object in walk.objects() {
            *counts.entry(object.type_name()).or_insert(0) += 1;
        }
        counts
    })
}

/// Old objects (from a full GC's survivors and allocated since) and young
/// ones are all there, each once, garbage too until a full GC frees it.
#[test]
fn the_walk_has_the_old_and_the_young() {
    let mut heap = heap(1 << 20);
    let root = heap.mutate(|cx| Persistent::new(cx, node(cx, 0, Some(node(cx, 1, None)))));
    heap.collect();
    // Garbage, and an object only a root holds, both young.
    let young = heap.mutate(|cx| {
        node(cx, 2, None);
        Persistent::new(cx, node(cx, 3, None))
    });
    assert_eq!(heap.stats().young_bytes, 2 * 8 * 3, "two young nodes");
    heap.mutate(|cx| {
        cx.pretenured(|cx| {
            node(cx, 4, None);
        })
    });
    let values = |heap: &mut Heap| {
        heap.walk(|_, walk| {
            let mut values: Vec<u32> = walk
                .objects()
                .filter_map(|object| object.get().downcast::<Node>())
                .map(|node| node.value.get())
                .collect();
            values.sort_unstable();
            values
        })
    };
    assert_eq!(values(&mut heap), [0, 1, 2, 3, 4]);
    heap.collect();
    assert_eq!(values(&mut heap), [0, 1, 3]);
    drop((root, young));
    heap.collect();
    assert!(values(&mut heap).is_empty());
}

/// Each object's type, size (its header's 8 bytes and its fields') and
/// edges: a `Member`'s strong, a `WeakMember`'s weak, an ephemeron's key
/// weak and its value alive while the key is, an array's unnamed, all by
/// their fields' names; JS edges by theirs; the roots' objects.
#[test]
fn an_object_has_its_type_size_and_named_edges() {
    let mut heap = heap(0);
    let _root = heap.mutate(|cx| {
        let a = node(cx, 1, None);
        let b = node(cx, 2, Some(a));
        b.weak.set(cx, Some(a));
        let children = HeapVector::new(cx, 2);
        children.push(cx, a);
        children.push(cx, b);
        let holder = cx.alloc(HolderInit {
            children: Some(children),
            pair: (Some(a), Some(b)),
            wrapper: (),
        });
        Persistent::new(cx, holder)
    });
    heap.walk(|_, walk| {
        let by_value = |value: u32| {
            walk.objects()
                .find(|object| {
                    object
                        .get()
                        .downcast::<Node>()
                        .is_some_and(|node| node.value.get() == value)
                })
                .expect("the node")
        };
        let (a, b) = (by_value(1), by_value(2));
        assert_eq!(a.type_name(), "Node");
        assert_eq!(a.size(), 8 + size_of::<Node>().next_multiple_of(8));
        assert_eq!(a.edges().count(), 0);
        let edges: Vec<_> = b
            .edges()
            .map(|edge| {
                (
                    edge.name.unwrap().to_str().unwrap(),
                    edge.kind,
                    edge.to.address(),
                )
            })
            .collect();
        assert_eq!(edges.len(), 2);
        assert!(matches!(edges[0], ("next", EdgeKind::Strong, to) if to == a.address()));
        assert!(matches!(edges[1], ("weak", EdgeKind::Weak, to) if to == a.address()));

        let holder = walk
            .objects()
            .find(|object| object.type_name() == "Holder")
            .expect("the holder");
        assert_eq!(holder.type_index(), elise::type_index::<Holder>());
        let edges: Vec<_> = holder.edges().collect();
        assert_eq!(
            edges.len(),
            3,
            "{edges:?}",
            edges = edges.iter().map(|edge| edge.to).collect::<Vec<_>>()
        );
        assert_eq!(edges[0].name, Some(c"children"));
        assert!(matches!(edges[0].kind, EdgeKind::Strong));
        assert_eq!(edges[0].to.type_name(), "HeapVector");
        assert_eq!(edges[1].name, Some(c"pair"));
        assert!(matches!(edges[1].kind, EdgeKind::Weak));
        assert_eq!(edges[1].to.address(), a.address());
        assert_eq!(edges[2].name, Some(c"pair"));
        assert!(
            matches!(edges[2].kind, EdgeKind::Ephemeron { key } if key.address() == a.address())
        );
        assert_eq!(edges[2].to.address(), b.address());
        let js: Vec<_> = holder.js_edges().map(|(name, _)| name).collect();
        assert_eq!(js, [c"wrapper"]);

        let vector = edges[0].to;
        let store: Vec<_> = vector.edges().collect();
        assert_eq!(store.len(), 1);
        assert_eq!(store[0].name, Some(c"store"));
        let array: Vec<_> = store[0].to.edges().collect();
        assert_eq!(array.len(), 2);
        assert!(
            array
                .iter()
                .all(|edge| edge.name.is_none() && matches!(edge.kind, EdgeKind::Strong))
        );
        assert_eq!(array[0].to.address(), a.address());
        assert_eq!(array[1].to.address(), b.address());
        assert_eq!(
            store[0].to.size(),
            (8 + 4 + 4 * 2_usize).next_multiple_of(8)
        );

        let roots: Vec<_> = walk.roots().map(|root| root.address()).collect();
        assert_eq!(roots, [holder.address()]);
    });
}

/// A large object, after the size classes' pages; a size class's page
/// partly bumped through: what its cursor hasn't reached isn't an object.
#[test]
fn large_objects_and_a_half_used_page() {
    let mut heap = heap(0);
    let _root = heap.mutate(|cx| {
        let large = cx.alloc(LargeInit {
            next: Some(node(cx, 1, None)),
            bytes: [0; 40 << 10],
        });
        Persistent::new(cx, large)
    });
    let counts = counts(&mut heap);
    assert_eq!(counts.get("Large"), Some(&1));
    assert_eq!(counts.get("Node"), Some(&1));
    assert_eq!(counts.len(), 2, "{counts:?}");
    heap.walk(|_, walk| {
        let large = walk
            .objects()
            .find(|object| object.type_name() == "Large")
            .unwrap();
        assert_eq!(large.size(), 8 + size_of::<Large>().next_multiple_of(8));
        assert_eq!(large.edges().next().unwrap().to.type_name(), "Node");
    });
}

/// The slots a GC freed are free again, those a cursor bumps through too:
/// their headers still name the dead objects' type, but they hold none.
#[test]
fn freed_slots_and_the_cursors_rest_hold_no_objects() {
    let mut heap = heap(0);
    let _root = heap.mutate(|cx| {
        let live = Persistent::new(cx, node(cx, 1, None));
        for _ in 0..10 {
            node(cx, 0, None);
        }
        live
    });
    heap.collect();
    let nodes = |heap: &mut Heap| counts(heap).get("Node").copied();
    assert_eq!(nodes(&mut heap), Some(1));
    // A new object takes the lowest free slot, a dead one's: the rest of
    // the range is the cursor's.
    let _new = heap.mutate(|cx| Persistent::new(cx, node(cx, 2, None)));
    assert_eq!(nodes(&mut heap), Some(2));
}

/// After a concurrent marking, whose sweep runs alongside the mutator
/// (posted, not run yet), the walk finishes the sweep first: the garbage
/// is gone, the live all there.
#[test]
fn the_walk_finishes_the_sweep() {
    let (mut heap, steps) = stepping_heap(0);
    let _root = heap.mutate(|cx| {
        cx.pretenured(|cx| {
            let mut list = None;
            for _ in 0..100 {
                list = Some(node(cx, 1, list));
                // Garbage.
                node(cx, 0, None);
            }
            Persistent::new(cx, list.unwrap())
        })
    });
    start_marking(&mut heap);
    finish(&mut heap, &steps);
    assert!(heap.is_sweeping());
    let values = heap.walk(|_, walk| {
        let mut values = [0usize; 2];
        for node in walk
            .objects()
            .filter_map(|object| object.get().downcast::<Node>())
        {
            values[node.value.get() as usize] += 1;
        }
        values
    });
    assert!(!heap.is_sweeping());
    assert_eq!(values, [0, 100]);
}

/// A pool's threads sweep alongside: the walk finishes theirs too.
#[test]
fn the_walk_waits_for_the_pools_sweep() {
    let mut heap = Heap::with_platform(
        HeapOptions {
            nursery_bytes: 0,
            ..crate::verified()
        },
        Arc::new(ThreadPool::new(2)),
    );
    let _root = heap.mutate(|cx| Persistent::new(cx, node(cx, 1, None)));
    for _ in 0..1000 {
        heap.mutate(|cx| {
            for _ in 0..100 {
                node(cx, 0, None);
            }
        });
    }
    let live = heap.walk(|_, walk| {
        walk.objects()
            .filter_map(|object| object.get().downcast::<Node>())
            .filter(|node| node.value.get() == 1)
            .count()
    });
    assert_eq!(live, 1);
    assert!(!heap.is_sweeping());
}

/// While a marking runs, an object garbage at its start that the walk
/// hands out stays alive once it is linked from a new object: new objects
/// are black and never scanned, so only the walk's marking of what it
/// hands out keeps it (SATB's rule for what the mutator gets from outside
/// the snapshot, as a weak read).
#[test]
fn what_the_walk_hands_out_while_marking_stays_valid() {
    let (mut heap, steps) = stepping_heap(0);
    heap.mutate(|cx| {
        cx.pretenured(|cx| {
            node(cx, 7, None);
        })
    });
    start_marking(&mut heap);
    let linked = heap.walk(|cx, walk| {
        let garbage = walk
            .objects()
            .filter_map(|object| object.get().downcast::<Node>())
            .find(|node| node.value.get() == 7)
            .expect("garbage, not swept yet");
        Persistent::new(cx, node(cx, 1, Some(garbage)))
    });
    finish(&mut heap, &steps);
    // Still an object once the sweep is done (a freed slot keeps its old
    // bytes, so reading it alone wouldn't tell).
    let kept = heap.walk(|_, walk| {
        walk.objects()
            .filter_map(|object| object.get().downcast::<Node>())
            .filter(|node| node.value.get() == 7)
            .count()
    });
    assert_eq!(kept, 1, "the object the walk handed out was swept");
    heap.mutate(|cx| assert_eq!(linked.get(cx).next.get(cx).unwrap().value.get(), 7));
}
