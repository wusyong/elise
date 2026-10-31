//! A small DOM on the heap: nodes with a parent, a first child and a next
//! sibling, attributes in a `HeapVector`, a style in an `External` and a
//! finalizer that logs its node's id. The workspace denies `unsafe` here,
//! as in kun's DOM: the derive's output passes it.
//!
//! What catches a bug: an edge or a root the GC doesn't mark (a test reads
//! a poisoned or reused slot), garbage kept or a live object finalized, a
//! finalizer run twice or never, a `HeapVector` that loses edges as it
//! grows or shrinks, a root entry leaked, and a `Member` or `Persistent`
//! of another heap read without a panic.

use std::cell::{Cell, RefCell};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;

use elise::{
    Gc, GcObject, Heap, HeapOptions, HeapVector, LocalExternal, Member, Mutation, Persistent,
};

#[derive(GcObject)]
struct Node {
    parent: Member<Node>,
    first_child: Member<Node>,
    next_sibling: Member<Node>,
    id: Cell<u32>,
    attributes: Member<HeapVector<Attr>>,
    style: LocalExternal<RefCell<Option<Rc<Style>>>>,
    finalized: LocalExternal<Finalized>,
}

#[derive(GcObject)]
struct Attr {
    value: Cell<u32>,
}

struct Style {
    color: u32,
}

/// Logs its node's id when the GC finalizes the node.
struct Finalized {
    id: u32,
    log: Rc<RefCell<Vec<u32>>>,
}

impl Drop for Finalized {
    fn drop(&mut self) {
        self.log.borrow_mut().push(self.id);
    }
}

type Log = Rc<RefCell<Vec<u32>>>;

fn node<'gc>(cx: &Mutation<'gc>, id: u32, log: &Log) -> Gc<'gc, Node> {
    cx.alloc(NodeInit {
        parent: None,
        first_child: None,
        next_sibling: None,
        id: Cell::new(id),
        attributes: Some(HeapVector::new(cx, 0)),
        style: LocalExternal::new(RefCell::new(None)),
        finalized: LocalExternal::new(Finalized {
            id,
            log: log.clone(),
        }),
    })
}

fn children<'gc>(cx: &Mutation<'gc>, parent: Gc<'gc, Node>) -> Vec<Gc<'gc, Node>> {
    let mut children = Vec::new();
    let mut child = parent.first_child.get(cx);
    while let Some(node) = child {
        children.push(node);
        // A broken heap may link a cycle: fail rather than grow forever.
        assert!(children.len() < 100_000, "a cycle of siblings");
        child = node.next_sibling.get(cx);
    }
    children
}

fn append<'gc>(cx: &Mutation<'gc>, parent: Gc<'gc, Node>, child: Gc<'gc, Node>) {
    child.parent.set(cx, Some(parent));
    match children(cx, parent).last() {
        Some(last) => last.next_sibling.set(cx, Some(child)),
        None => parent.first_child.set(cx, Some(child)),
    }
}

fn detach<'gc>(cx: &Mutation<'gc>, child: Gc<'gc, Node>) {
    let Some(parent) = child.parent.get(cx) else {
        return;
    };
    let next = child.next_sibling.get(cx);
    let siblings = children(cx, parent);
    let at = siblings
        .iter()
        .position(|&node| Gc::ptr_eq(node, child))
        .expect("a child is among its parent's");
    match at.checked_sub(1) {
        Some(previous) => siblings[previous].next_sibling.set(cx, next),
        None => parent.first_child.set(cx, next),
    }
    child.parent.set(cx, None);
    child.next_sibling.set(cx, None);
}

fn ids(nodes: &[Gc<'_, Node>]) -> Vec<u32> {
    nodes.iter().map(|node| node.id.get()).collect()
}

fn stress_heap() -> Heap {
    Heap::new(HeapOptions {
        stress: true,
        ..crate::verified()
    })
}

/// `alloc` writes each field from the init struct, a non-null edge too.
#[test]
fn alloc_writes_every_field_in_place() {
    let log = Log::default();
    let mut heap = stress_heap();
    heap.mutate(|cx| {
        let parent = node(cx, 1, &log);
        let child = cx.alloc(NodeInit {
            parent: Some(parent),
            first_child: None,
            next_sibling: None,
            id: Cell::new(2),
            attributes: None,
            style: LocalExternal::new(RefCell::new(Some(Rc::new(Style { color: 7 })))),
            finalized: LocalExternal::new(Finalized {
                id: 2,
                log: log.clone(),
            }),
        });
        assert!(Gc::ptr_eq(child.parent.get(cx).unwrap(), parent));
        assert!(child.first_child.is_null() && child.attributes.is_null());
        assert_eq!(child.id.get(), 2);
        assert_eq!(child.style.borrow().as_ref().unwrap().color, 7);
    });
}

/// A tree rooted by a `Persistent` survives a GC whole: promoted out of the
/// nursery, then where it was.
#[test]
fn a_rooted_tree_survives_a_gc_in_place() {
    let log = Log::default();
    let mut heap = Heap::new(crate::verified());
    let root = heap.mutate(|cx| {
        let root = node(cx, 1, &log);
        for id in 2..=4 {
            append(cx, root, node(cx, id, &log));
        }
        let first = root.first_child.get(cx).unwrap();
        append(cx, first, node(cx, 5, &log));
        Persistent::new(cx, root)
    });
    let young = heap.mutate(|cx| format!("{:?}", root.get(cx)));
    heap.collect();
    let address = heap.mutate(|cx| format!("{:?}", root.get(cx)));
    assert_ne!(address, young, "a full GC promotes the nursery's objects");
    heap.collect();
    heap.mutate(|cx| {
        let root = root.get(cx);
        assert_eq!(
            format!("{root:?}"),
            address,
            "the old generation doesn't move"
        );
        assert_eq!(ids(&children(cx, root)), [2, 3, 4]);
        let first = root.first_child.get(cx).unwrap();
        assert_eq!(ids(&children(cx, first)), [5]);
        for child in children(cx, root) {
            assert!(Gc::ptr_eq(child.parent.get(cx).unwrap(), root));
        }
    });
    assert!(log.borrow().is_empty());
}

/// A detached subtree is garbage: its nodes are finalized at the next GC,
/// once each; the rest at the heap's drop.
#[test]
fn garbage_is_finalized_once() {
    let log = Log::default();
    let mut heap = Heap::new(crate::verified());
    let root = heap.mutate(|cx| {
        let root = node(cx, 1, &log);
        let subtree = node(cx, 2, &log);
        append(cx, root, subtree);
        append(cx, subtree, node(cx, 3, &log));
        append(cx, root, node(cx, 4, &log));
        Persistent::new(cx, root)
    });
    heap.mutate(|cx| {
        let subtree = root.get(cx).first_child.get(cx).unwrap();
        detach(cx, subtree);
    });
    assert!(log.borrow().is_empty(), "no GC in an operation");
    heap.collect();
    let mut finalized = log.borrow().clone();
    finalized.sort();
    assert_eq!(finalized, [2, 3]);
    heap.collect();
    assert_eq!(log.borrow().len(), 2);
    drop(root);
    drop(heap);
    let mut finalized = log.borrow().clone();
    finalized.sort();
    assert_eq!(finalized, [1, 2, 3, 4]);
}

/// An `External`'s value lives as long as its object: the style's last
/// other reference goes when the node is finalized.
#[test]
fn external_values_drop_with_their_object() {
    let log = Log::default();
    let style = Rc::new(Style { color: 3 });
    let mut heap = stress_heap();
    let kept = heap.mutate(|cx| {
        let kept = node(cx, 1, &log);
        *kept.style.borrow_mut() = Some(style.clone());
        let dropped = node(cx, 2, &log);
        *dropped.style.borrow_mut() = Some(style.clone());
        Persistent::new(cx, kept)
    });
    heap.collect();
    assert_eq!(Rc::strong_count(&style), 2);
    heap.mutate(|cx| assert_eq!(kept.get(cx).style.borrow().as_ref().unwrap().color, 3));
    drop(kept);
    heap.collect();
    assert_eq!(Rc::strong_count(&style), 1);
}

/// A vector's edges survive its growth (4, 8, 16), removals and GCs.
#[test]
fn heap_vectors_grow_and_shrink() {
    let log = Log::default();
    let mut heap = stress_heap();
    let root = heap.mutate(|cx| Persistent::new(cx, node(cx, 1, &log)));
    for value in 0..10 {
        heap.mutate(|cx| {
            let attributes = root.get(cx).attributes.get(cx).unwrap();
            attributes.push(
                cx,
                cx.alloc(AttrInit {
                    value: Cell::new(value),
                }),
            );
        });
    }
    heap.mutate(|cx| {
        let attributes = root.get(cx).attributes.get(cx).unwrap();
        assert_eq!(attributes.remove(cx, 0).value.get(), 0);
        assert_eq!(attributes.remove(cx, 4).value.get(), 5);
        attributes.set(
            cx,
            0,
            cx.alloc(AttrInit {
                value: Cell::new(100),
            }),
        );
    });
    heap.mutate(|cx| {
        let attributes = root.get(cx).attributes.get(cx).unwrap();
        let values: Vec<u32> = (0..attributes.len())
            .map(|i| attributes.get(cx, i).unwrap().value.get())
            .collect();
        assert_eq!(values, [100, 2, 3, 4, 6, 7, 8, 9]);
        assert!(attributes.get(cx, 8).is_none());
    });
}

/// What a vector removes is garbage: no edge past its length keeps it.
#[test]
fn heap_vectors_let_go_of_what_they_remove() {
    let log = Log::default();
    let mut heap = stress_heap();
    let list = heap.mutate(|cx| {
        let list = HeapVector::<Node>::new(cx, 2);
        for id in 1..=3 {
            list.push(cx, node(cx, id, &log));
        }
        assert_eq!(list.remove(cx, 2).id.get(), 3);
        assert_eq!(list.remove(cx, 0).id.get(), 1);
        Persistent::new(cx, list)
    });
    heap.collect();
    let mut finalized = log.borrow().clone();
    finalized.sort();
    assert_eq!(finalized, [1, 3]);
    heap.mutate(|cx| assert_eq!(ids(&[list.get(cx).get(cx, 0).unwrap()]), [2]));
}

/// A `Persistent`'s clone is a root of its own; dropping either frees its
/// entry, which a new one reuses.
#[test]
fn persistents_take_and_free_entries() {
    let log = Log::default();
    let mut heap = stress_heap();
    let first = heap.mutate(|cx| Persistent::new(cx, node(cx, 1, &log)));
    let second = first.clone();
    assert_eq!(heap.roots(), 2);
    drop(first);
    heap.collect();
    assert!(log.borrow().is_empty(), "the clone keeps it");
    heap.mutate(|cx| assert_eq!(second.get(cx).id.get(), 1));
    let third = heap.mutate(|cx| Persistent::new(cx, node(cx, 2, &log)));
    assert_eq!(heap.roots(), 2);
    drop(second);
    heap.collect();
    assert_eq!(*log.borrow(), [1]);
    drop(third);
}

#[test]
#[should_panic = "a Persistent of another heap"]
fn a_persistent_of_another_heap_panics() {
    let log = Log::default();
    let mut one = stress_heap();
    let mut other = stress_heap();
    let root = one.mutate(|cx| Persistent::new(cx, node(cx, 1, &log)));
    other.mutate(|cx| {
        root.get(cx);
    });
}

#[test]
#[should_panic = "a Member of another heap"]
fn a_member_of_another_heap_panics() {
    let log = Log::default();
    let mut one = stress_heap();
    let mut other = stress_heap();
    one.mutate(|one_cx| {
        let node = node(one_cx, 1, &log);
        other.mutate(|other_cx| {
            node.parent.get(other_cx);
        });
    });
}

/// An operation that panics leaves the heap whole: what it allocated is
/// garbage, what it rooted stays.
#[test]
fn a_panicking_operation_leaves_the_heap_whole() {
    let log = Log::default();
    let mut heap = stress_heap();
    let root = heap.mutate(|cx| Persistent::new(cx, node(cx, 1, &log)));
    let result = catch_unwind(AssertUnwindSafe(|| {
        heap.mutate(|cx| {
            append(cx, root.get(cx), node(cx, 2, &log));
            node(cx, 3, &log);
            panic!("in the middle");
        })
    }));
    assert!(result.is_err());
    heap.collect();
    assert_eq!(*log.borrow(), [3]);
    heap.mutate(|cx| assert_eq!(ids(&children(cx, root.get(cx))), [2]));
}

/// Random changes to a tree, with a GC before every operation, checked
/// against a model of it after each: every node, edge, attribute and style
/// where the model says, every garbage node finalized once.
#[test]
fn a_random_tree_survives_a_gc_at_every_safepoint() {
    let rounds = if cfg!(miri) { 20 } else { 3000 };
    let log = Log::default();
    let mut heap = stress_heap();
    let style = Rc::new(Style { color: 9 });
    let root = heap.mutate(|cx| Persistent::new(cx, node(cx, 0, &log)));
    // The model: each live node's parent, children and attribute values.
    let mut parent: Vec<Option<u32>> = vec![None];
    let mut kids: Vec<Vec<u32>> = vec![Vec::new()];
    let mut attributes: Vec<Vec<u32>> = vec![Vec::new()];
    let mut styled: Vec<bool> = vec![false];
    let mut alive: Vec<bool> = vec![true];
    let mut random = 0x2545_f491_u32;
    let mut next = || {
        random ^= random << 13;
        random ^= random >> 17;
        random ^= random << 5;
        random
    };
    for _ in 0..rounds {
        let live: Vec<u32> = (0..alive.len() as u32)
            .filter(|&id| alive[id as usize])
            .collect();
        let pick = |n: u32| live[(n as usize) % live.len()];
        let (a, b, kind, value) = (pick(next()), pick(next()), next() % 6, next() % 1000);
        heap.mutate(|cx| {
            let find = |id: u32| find(cx, root.get(cx), id).expect("a live node is in the tree");
            match kind {
                // A new node appended to `a`.
                0 | 1 => {
                    let id = alive.len() as u32;
                    append(cx, find(a), node(cx, id, &log));
                    parent.push(Some(a));
                    kids.push(Vec::new());
                    kids[a as usize].push(id);
                    attributes.push(Vec::new());
                    styled.push(false);
                    alive.push(true);
                }
                // `a`'s subtree detached: garbage.
                2 if a != 0 => {
                    detach(cx, find(a));
                    let p = parent[a as usize].take().unwrap();
                    kids[p as usize].retain(|&kid| kid != a);
                    let mut gone = vec![a];
                    while let Some(id) = gone.pop() {
                        alive[id as usize] = false;
                        gone.extend(kids[id as usize].iter().copied());
                    }
                }
                // `a` moved under `b`, unless `b` is in `a`'s subtree.
                3 if a != 0 && !is_ancestor(&parent, a, b) => {
                    let moved = find(a);
                    detach(cx, moved);
                    append(cx, find(b), moved);
                    let p = parent[a as usize].replace(b).unwrap();
                    kids[p as usize].retain(|&kid| kid != a);
                    kids[b as usize].push(a);
                }
                // An attribute pushed, or the first removed.
                4 => {
                    let list = find(a).attributes.get(cx).unwrap();
                    let model = &mut attributes[a as usize];
                    if value % 3 == 0 && !model.is_empty() {
                        list.remove(cx, 0);
                        model.remove(0);
                    } else {
                        list.push(
                            cx,
                            cx.alloc(AttrInit {
                                value: Cell::new(value),
                            }),
                        );
                        model.push(value);
                    }
                }
                // A style set.
                5 => {
                    *find(a).style.borrow_mut() = Some(style.clone());
                    styled[a as usize] = true;
                }
                _ => {}
            }
        });
        heap.mutate(|cx| {
            check(cx, root.get(cx), None, &parent, &kids, &attributes, &styled);
        });
    }
    heap.collect();
    let mut finalized = log.borrow().clone();
    finalized.sort();
    let dead: Vec<u32> = (0..alive.len() as u32)
        .filter(|&id| !alive[id as usize])
        .collect();
    assert_eq!(finalized, dead);
    let styled_alive = (0..alive.len())
        .filter(|&id| alive[id] && styled[id])
        .count();
    assert_eq!(Rc::strong_count(&style), 1 + styled_alive);
    // A GC at every safepoint: scavenges and full GCs in turn.
    let stats = heap.stats();
    assert!((stats.gcs + stats.scavenges) as usize >= 2 * rounds);
    assert!(stats.scavenges > stats.gcs);
}

fn find<'gc>(cx: &Mutation<'gc>, node: Gc<'gc, Node>, id: u32) -> Option<Gc<'gc, Node>> {
    if node.id.get() == id {
        return Some(node);
    }
    children(cx, node)
        .into_iter()
        .find_map(|child| find(cx, child, id))
}

fn is_ancestor(parent: &[Option<u32>], ancestor: u32, mut node: u32) -> bool {
    loop {
        if node == ancestor {
            return true;
        }
        match parent[node as usize] {
            Some(p) => node = p,
            None => return false,
        }
    }
}

fn check<'gc>(
    cx: &Mutation<'gc>,
    node: Gc<'gc, Node>,
    expected_parent: Option<u32>,
    parent: &[Option<u32>],
    kids: &[Vec<u32>],
    attributes: &[Vec<u32>],
    styled: &[bool],
) {
    let id = node.id.get();
    assert_eq!(node.finalized.id, id);
    assert_eq!(parent[id as usize], expected_parent);
    assert_eq!(node.parent.get(cx).map(|p| p.id.get()), expected_parent);
    let list = node.attributes.get(cx).unwrap();
    let values: Vec<u32> = (0..list.len())
        .map(|i| list.get(cx, i).unwrap().value.get())
        .collect();
    assert_eq!(values, attributes[id as usize]);
    assert_eq!(node.style.borrow().is_some(), styled[id as usize]);
    let children = children(cx, node);
    assert_eq!(ids(&children), kids[id as usize]);
    for child in children {
        check(cx, child, Some(id), parent, kids, attributes, styled);
    }
}
