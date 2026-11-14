//! Marking with a JS engine (`HeapOptions::driven`), with a fake engine of
//! the tests': it starts, advances and finishes the marking, hands its
//! wrappers' objects over (from a thread of its own, as V8's markers), and
//! collects the JS edges the heap's marker gives it, which it wrote. A type
//! with a JS edge is always old; a marking keeps what the engine handed
//! over and frees the rest, but what is made while it runs; an object
//! read back from the engine while marking is marked; scavenges keep what
//! only the engine holds; the heap never starts a GC of the old generation
//! by itself. Every heap runs the verifier.

// The engine's side is unsafe: it hands over and reads back raw addresses,
// and writes its references into the heap's objects.
#![allow(unsafe_code, reason = "the fake engine's side of the interface")]

use std::cell::Cell;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use elise::{
    GcObject, Heap, HeapOptions, InlinePlatform, JsEdge, JsEdgeLocal, JsEdgeSink, Member,
    Persistent, ThreadPool,
};

use crate::concurrent::{Stepping, stepping_heap_with};

#[derive(GcObject)]
struct Node {
    next: Member<Node>,
    wrapper: JsEdge,
    value: Cell<u32>,
}

/// An object with no JS edge, which may be young.
#[derive(GcObject)]
struct Plain {
    next: Member<Node>,
}

fn node<'gc>(
    cx: &elise::Mutation<'gc>,
    value: u32,
    next: Option<elise::Gc<'gc, Node>>,
) -> elise::Gc<'gc, Node> {
    cx.alloc(NodeInit {
        next,
        wrapper: (),
        value: Cell::new(value),
    })
}

/// The engine's reference in `node`'s JS edge: what the fake engine wrote.
fn write_edge(node: &Node, value: u64) {
    // SAFETY: the edge's 8 aligned bytes, the engine's to write, which the
    // marker never reads.
    unsafe { AtomicU64::from_ptr(node.wrapper.as_ptr()) }.store(value, Ordering::Relaxed);
}

/// The fake engine's side of the JS edges: the values it found in the
/// edges the heap's marker gave it.
#[derive(Clone, Default)]
struct Engine(Arc<Mutex<Vec<u64>>>);

impl Engine {
    fn marked(&self) -> Vec<u64> {
        let mut marked = self.0.lock().unwrap().clone();
        marked.sort_unstable();
        marked
    }
}

impl JsEdgeSink for Engine {
    fn local(&self) -> Box<dyn JsEdgeLocal> {
        Box::new(EngineLocal(self.0.clone(), Vec::new()))
    }
}

/// A marking thread's JS edges, published when it ends.
struct EngineLocal(Arc<Mutex<Vec<u64>>>, Vec<u64>);

impl JsEdgeLocal for EngineLocal {
    unsafe fn mark(&mut self, edge: NonNull<JsEdge>) {
        // SAFETY: as the heap promises: a live object's edge.
        let value =
            unsafe { AtomicU64::from_ptr((*edge.as_ptr()).as_ptr()) }.load(Ordering::Relaxed);
        if value != 0 {
            self.1.push(value);
        }
    }
}

impl Drop for EngineLocal {
    fn drop(&mut self) {
        self.0.lock().unwrap().append(&mut self.1);
    }
}

/// An engine's step's time (V8's task's).
const STEP: Duration = Duration::from_millis(1);

/// A driven heap on a `Stepping` platform, with a nursery of
/// `nursery_bytes`.
fn driven(nursery_bytes: usize) -> (Heap, Arc<Stepping>) {
    stepping_heap_with(HeapOptions {
        driven: true,
        nursery_bytes,
        ..crate::verified()
    })
}

/// The marking's end, as V8's pause ends it: the heap advanced until both
/// sides are done, then finished.
fn finish(heap: &mut Heap) {
    while !heap.advance_marking(true, Duration::MAX) {}
    heap.finish_marking();
}

/// An engine's wrapper's hold on an object: its address, the pointer
/// itself (its provenance kept, as Miri's strict provenance wants).
#[derive(Clone, Copy)]
struct Held(NonNull<u8>);

// SAFETY: the engine's threads only pass the address back to the heap
// (`hand`, `from_engine`, whose callers promise it is live), as V8's do;
// none reads through it.
unsafe impl Send for Held {}

impl Held {
    fn of<'gc>(cx: &elise::Mutation<'gc>, node: elise::Gc<'gc, Node>) -> Self {
        Self(cx.engine_address(node))
    }

    fn get<'gc>(self, cx: &elise::Mutation<'gc>) -> elise::Gc<'gc, Node> {
        // SAFETY: the address `engine_address` gave of a `Node` of this
        // heap, which the test keeps alive (handed over, or no marking ran).
        unsafe { cx.from_engine(self.0) }
    }
}

/// Hands `held` over from a thread of the engine's.
fn hand(marker: &elise::WrapperMarker, held: Vec<Held>) {
    let marker = marker.clone();
    std::thread::spawn(move || {
        let mut local = marker.local();
        for Held(address) in held {
            // SAFETY: a live `Node` of the heap marking, its wrapper's.
            unsafe { local.hand(address) };
        }
    })
    .join()
    .unwrap();
}

/// In a driven heap, an object with a JS edge is old from the start; one
/// with none is young. In a heap no engine drives, both are young.
#[test]
fn an_object_with_a_js_edge_is_old_in_a_driven_heap() {
    for driven in [false, true] {
        let mut heap = Heap::with_platform(
            HeapOptions {
                driven,
                nursery_bytes: 1 << 20,
                ..crate::verified()
            },
            Arc::new(InlinePlatform),
        );
        heap.mutate(|cx| {
            node(cx, 1, None);
        });
        assert_eq!(heap.stats().young_bytes == 0, driven, "driven: {driven}");
        heap.mutate(|cx| {
            cx.alloc(PlainInit { next: None });
        });
        assert!(heap.stats().young_bytes > 0, "a plain object is young");
    }
}

/// The engine hands over one of two objects: it, and what it points to,
/// survive; the other is freed.
#[test]
fn a_marking_keeps_what_the_engine_hands_over_and_frees_the_rest() {
    let (mut heap, _steps) = driven(0);
    let (kept, kept_id, dropped_id) = heap.mutate(|cx| {
        let a = node(cx, 1, Some(node(cx, 2, None)));
        let b = node(cx, 3, None);
        (Held::of(cx, a), cx.new_id(a), cx.new_id(b))
    });
    let marker = heap.start_marking(Arc::new(Engine::default()));
    hand(&marker, vec![kept]);
    finish(&mut heap);
    heap.mutate(|cx| {
        let a = kept.get(cx);
        assert_eq!(a.next.get(cx).unwrap().value.get(), 2);
        assert!(cx.by_id::<Node>(kept_id).is_some());
        assert!(cx.by_id::<Node>(dropped_id).is_none(), "its wrapper died");
    });
}

/// A marking the engine drives ends with the heap's live bytes as what it
/// uses, which the engine sets its next limit from (as V8 does cppgc's),
/// not what it used with the dead the sweep has yet to free: what the
/// marker scanned, and what entered the old generation while it ran
/// (black, never scanned): made there, and promoted by a scavenge. The
/// sweep finds the same.
#[test]
fn a_driven_marking_ends_with_the_live_bytes() {
    let (mut heap, steps) = driven(64 << 10);
    let kept = heap.mutate(|cx| {
        // Dead: nothing hands them over.
        for value in 0..2_000 {
            node(cx, value, None);
        }
        Held::of(cx, node(cx, 1, Some(node(cx, 2, None))))
    });
    let dead = heap.used_bytes();
    let marker = heap.start_marking(Arc::new(Engine::default()));
    hand(&marker, vec![kept]);
    let (made, young) = heap.mutate(|cx| {
        (
            Held::of(cx, node(cx, 3, None)),
            Persistent::new(cx, cx.alloc(PlainInit { next: None })),
        )
    });
    let scavenges = heap.stats().scavenges;
    // Under Miri, in smaller operations, up to the second scavenge (the
    // first copies the rooted object, the second promotes it): the
    // allocations take it hours.
    for _ in 0..if cfg!(miri) { 400 } else { 20 } {
        heap.mutate(|cx| {
            for _ in 0..if cfg!(miri) { 1000 } else { 10_000 } {
                cx.alloc(PlainInit { next: None });
            }
        });
        if cfg!(miri) && heap.stats().scavenges >= scavenges + 2 {
            break;
        }
    }
    assert!(
        heap.stats().scavenges > scavenges,
        "a scavenge promoted the rooted object"
    );
    while !heap.advance_marking(true, Duration::MAX) {}
    // What it used: the dead nodes too (what is allocated while marking
    // is black, so it lives through this marking).
    let used = heap.used_bytes();
    let live = heap.finish_marking();
    assert!(
        live.saturating_add(dead / 2) < used,
        "{live} of {used}: the dead counted"
    );
    // The old generation's: the nursery's bytes, counted too, change as it
    // scavenges.
    let old = live - heap.stats().young_bytes;
    steps.step();
    heap.mutate(|cx| assert_eq!(made.get(cx).value.get(), 3));
    assert_eq!(
        heap.used_bytes() - heap.stats().young_bytes,
        old,
        "the sweep found other live bytes"
    );
    drop(young);
}

/// An object made while the marking runs survives it, though the engine
/// never hands it over (its wrapper is new: black, never scanned); the next
/// marking frees it if not handed over.
#[test]
fn an_object_made_while_marking_survives_it() {
    let (mut heap, _steps) = driven(0);
    let _marker = heap.start_marking(Arc::new(Engine::default()));
    let (held, id) = heap.mutate(|cx| {
        let n = node(cx, 4, None);
        (Held::of(cx, n), cx.new_id(n))
    });
    finish(&mut heap);
    heap.mutate(|cx| assert_eq!(held.get(cx).value.get(), 4));
    let _marker = heap.start_marking(Arc::new(Engine::default()));
    finish(&mut heap);
    heap.mutate(|cx| assert!(cx.by_id::<Node>(id).is_none()));
}

/// A handover after the marking ended (a late one of the engine's, from a
/// thread still holding the marker) touches nothing: not even the mark bit
/// of an object the marking freed, on a page still in use.
#[test]
fn a_handover_after_the_marking_touches_nothing() {
    let (mut heap, steps) = driven(0);
    let (kept, dead) = heap.mutate(|cx| {
        (
            Persistent::new(cx, node(cx, 1, None)),
            Held::of(cx, node(cx, 2, None)),
        )
    });
    let marker = heap.start_marking(Arc::new(Engine::default()));
    finish(&mut heap);
    steps.step();
    heap.mutate(|_| {});
    assert!(!heap.is_sweeping());
    hand(&marker, vec![dead]);
    assert!(marker.is_drained(), "nothing pushed");
    heap.mutate(|cx| assert_eq!(kept.get(cx).value.get(), 1));
}

/// The heap dropped while the engine marks (its isolate torn down
/// mid-marking): a handover afterwards touches nothing of the heap's.
#[test]
fn a_handover_after_the_heap_is_dropped_touches_nothing() {
    let (mut heap, _steps) = driven(0);
    let held = heap.mutate(|cx| Held::of(cx, node(cx, 6, None)));
    let marker = heap.start_marking(Arc::new(Engine::default()));
    drop(heap);
    hand(&marker, vec![held]);
    assert!(marker.is_drained(), "nothing pushed");
}

/// While marking runs, script reads an object back from its wrapper
/// (which the engine hasn't handed over yet), links it from an object the
/// marker scanned already, and drops the wrapper: the read marked it.
#[test]
fn an_object_read_back_from_the_engine_while_marking_is_marked() {
    let (mut heap, steps) = driven(0);
    let (holder, held) = heap.mutate(|cx| {
        (
            Persistent::new(cx, node(cx, 0, None)),
            Held::of(cx, node(cx, 9, None)),
        )
    });
    let _marker = heap.start_marking(Arc::new(Engine::default()));
    // The marker scans the holder: black.
    steps.step();
    heap.mutate(|cx| {
        let n = held.get(cx);
        holder.get(cx).next.set(cx, Some(n));
    });
    finish(&mut heap);
    heap.mutate(|cx| assert_eq!(holder.get(cx).next.get(cx).unwrap().value.get(), 9));
}

/// An object only the engine's wrapper holds leads to another as it is
/// handed over; its edge to that one is then cleared. The other stays
/// marked, as the barrier (SATB) keeps what an edge held when the marking
/// reached its object: garbage that floats till the next marking. The
/// verifier takes it as such, though the roots never reached it (the
/// snapshot hasn't it) and the handed object no longer leads to it as the
/// marking ends.
#[test]
fn what_a_handed_object_led_to_floats_once_its_edge_is_cleared() {
    let (mut heap, steps) = driven(0);
    let (held, child_id) = heap.mutate(|cx| {
        let child = node(cx, 2, None);
        let parent = node(cx, 1, Some(child));
        (Held::of(cx, parent), cx.new_id(child))
    });
    let marker = heap.start_marking(Arc::new(Engine::default()));
    hand(&marker, vec![held]);
    // The marker goes through the handed object, to its child.
    steps.step();
    heap.mutate(|cx| held.get(cx).next.set(cx, None));
    // The verifier runs as the marking ends.
    finish(&mut heap);
    heap.mutate(|cx| {
        assert!(held.get(cx).next.get(cx).is_none());
        assert!(cx.by_id::<Node>(child_id).is_some(), "it floats");
    });
    // The next marking frees it.
    let marker = heap.start_marking(Arc::new(Engine::default()));
    hand(&marker, vec![held]);
    finish(&mut heap);
    heap.mutate(|cx| assert!(cx.by_id::<Node>(child_id).is_none()));
}

/// Scavenges keep what only the engine holds (old), and the young objects
/// it points to.
#[test]
fn scavenges_keep_what_only_the_engine_holds() {
    let (mut heap, _steps) = driven(1 << 20);
    let held = heap.mutate(|cx| {
        let n = node(cx, 5, None);
        let plain = cx.alloc(PlainInit {
            next: Some(node(cx, 6, None)),
        });
        let _ = plain;
        Held::of(cx, n)
    });
    let young = heap.mutate(|cx| {
        let n = held.get(cx);
        let young = cx.alloc(PlainInit { next: None });
        let _ = young;
        n.next.set(cx, Some(node(cx, 7, None)));
        Persistent::new(cx, cx.alloc(PlainInit { next: Some(n) }))
    });
    for _ in 0..3 {
        heap.scavenge();
        heap.mutate(|cx| {
            let n = held.get(cx);
            assert_eq!(n.value.get(), 5);
            assert_eq!(n.next.get(cx).unwrap().value.get(), 7);
        });
    }
    drop(young);
}

/// The heap's marker gives the engine the JS edges of what it marks, from
/// the background and in the pause; not a dead object's.
#[test]
fn the_marker_gives_the_engine_the_js_edges_of_what_it_marks() {
    let (mut heap, steps) = driven(0);
    let roots = heap.mutate(|cx| {
        let edged = |value: u64| {
            let n = node(cx, 0, None);
            write_edge(&n, value);
            n
        };
        let _dead = edged(30);
        let a = edged(10);
        let b = edged(20);
        (Persistent::new(cx, a), Persistent::new(cx, b))
    });
    let engine = Engine::default();
    let _marker = heap.start_marking(Arc::new(engine.clone()));
    steps.step();
    assert_eq!(engine.marked(), [10, 20]);
    finish(&mut heap);
    assert_eq!(engine.marked(), [10, 20]);
    drop(roots);
}

/// The heap never starts a GC of the old generation by itself, however
/// much it allocates there: the engine does.
#[test]
fn a_driven_heap_starts_no_marking_by_itself() {
    let (mut heap, _steps) = stepping_heap_with(HeapOptions {
        driven: true,
        nursery_bytes: 0,
        min_allocation_bytes: 16 << 10,
        ..crate::verified()
    });
    for _ in 0..100 {
        heap.mutate(|cx| {
            for i in 0..100 {
                node(cx, i, None);
            }
        });
        assert!(!heap.is_marking());
        assert!(!heap.should_collect(), "past its limit, but the engine's");
    }
    assert_eq!(heap.stats().gcs, 0);
    assert!(heap.used_bytes() > 16 << 10);
}

/// What it tells the engine it uses counts the nursery's objects too.
#[test]
fn the_engine_is_told_of_the_nurserys_bytes() {
    let (mut heap, _steps) = driven(1 << 20);
    let before = heap.used_bytes();
    let _young = heap.mutate(|cx| {
        let young = cx.alloc(PlainInit { next: None });
        for _ in 0..99 {
            cx.alloc(PlainInit { next: None });
        }
        Persistent::new(cx, young)
    });
    assert_eq!(heap.stats().scavenges, 0, "all of them young");
    assert!(heap.used_bytes() > before);
}

/// Its scavenges are its own: a full nursery is scavenged at the next
/// safepoint, as in a heap that isn't driven. Under Miri, a nursery of one
/// page, filled in smaller operations up to its first scavenge: filling a
/// megabyte's takes it hours.
#[test]
fn a_driven_heap_scavenges_its_full_nursery_itself() {
    let (mut heap, _steps) = driven(if cfg!(miri) { 1 } else { 1 << 20 });
    for _ in 0..if cfg!(miri) { 500 } else { 50 } {
        heap.mutate(|cx| {
            for _ in 0..if cfg!(miri) { 1000 } else { 10_000 } {
                cx.alloc(PlainInit { next: None });
            }
        });
        if cfg!(miri) && heap.stats().scavenges > 0 {
            break;
        }
    }
    assert!(heap.stats().scavenges > 0);
    assert_eq!(heap.stats().gcs, 0, "no full GC of its own");
}

/// A safepoint while the engine marks publishes the barrier's objects for
/// the workers, who mark from them before the engine's pause; else the
/// pause would.
#[test]
fn a_safepoint_gives_the_workers_the_barriers_objects() {
    let (mut heap, steps) = driven(0);
    let root = heap.mutate(|cx| {
        let b = node(cx, 2, None);
        let a = node(cx, 1, Some(b));
        Persistent::new(cx, node(cx, 0, Some(a)))
    });
    let _marker = heap.start_marking(Arc::new(Engine::default()));
    // The marker hasn't reached `a`: cutting its edge records it.
    heap.mutate(|cx| root.get(cx).next.set(cx, None));
    heap.mutate(|_| {});
    steps.step();
    finish(&mut heap);
    let last = heap.stats().last_gc;
    assert_eq!(last.marked_objects, 3, "the root, and `a` and `b` floating");
    assert_eq!(last.marked_in_pause, 0, "the workers marked them all");
}

/// An engine's step wakes the workers for what the engine handed over.
#[test]
fn an_engines_step_wakes_the_workers() {
    let (mut heap, steps) = driven(0);
    let held = heap.mutate(|cx| Held::of(cx, node(cx, 1, None)));
    let marker = heap.start_marking(Arc::new(Engine::default()));
    hand(&marker, vec![held]);
    let before = steps.notified();
    heap.advance_marking(false, STEP);
    assert!(steps.notified() > before);
    finish(&mut heap);
}

/// The engine finishes a marking only once both sides are done.
#[test]
#[should_panic(expected = "the engine finishes a marking once it is done")]
fn the_engine_finishes_only_a_marking_that_is_done() {
    let (mut heap, _steps) = driven(0);
    let _root = heap.mutate(|cx| Persistent::new(cx, node(cx, 0, None)));
    let _marker = heap.start_marking(Arc::new(Engine::default()));
    heap.finish_marking();
}

/// Its full GCs are the engine's: the heap doesn't know what the engine's
/// wrappers hold.
#[test]
#[should_panic(expected = "the engine collects the heap it drives")]
fn the_engine_collects_a_driven_heap() {
    let (mut heap, _steps) = driven(0);
    heap.collect();
}

#[test]
#[should_panic(expected = "an engine holds a driven heap's objects")]
fn engine_addresses_are_a_driven_heaps() {
    let mut heap = Heap::new(crate::verified());
    heap.mutate(|cx| {
        cx.engine_address(node(cx, 1, None));
    });
}

#[test]
#[should_panic(expected = "an object an engine holds has a JS edge")]
fn an_object_an_engine_holds_has_a_js_edge() {
    let (mut heap, _steps) = driven(0);
    heap.mutate(|cx| {
        cx.engine_address(cx.alloc(PlainInit { next: None }));
    });
}

/// On a real pool: a forest of wrapped trees, the engine handing over every
/// other top from its thread while the workers mark, scavenges between;
/// the handed trees survive whole, the others are freed, and the engine is
/// given the JS edge of every node of the handed trees.
#[test]
fn a_marking_the_engine_drives_on_a_pool() {
    const TREES: u32 = 200;
    const DEPTH: u32 = 30;
    let mut heap = Heap::with_platform(
        HeapOptions {
            driven: true,
            nursery_bytes: 64 << 10,
            ..crate::verified()
        },
        Arc::new(ThreadPool::new(2)),
    );
    let tops: Vec<(Held, elise::Id)> = heap.mutate(|cx| {
        (0..TREES)
            .map(|tree| {
                let mut next = None;
                for depth in 0..DEPTH {
                    let n = node(cx, tree * DEPTH + depth, next);
                    write_edge(&n, u64::from(tree * DEPTH + depth + 1));
                    next = Some(n);
                }
                let top = next.unwrap();
                (Held::of(cx, top), cx.new_id(top))
            })
            .collect()
    });
    let engine = Engine::default();
    let marker = heap.start_marking(Arc::new(engine.clone()));
    let handed: Vec<Held> = tops.iter().step_by(2).map(|&(held, _)| held).collect();
    let engine_thread = {
        let marker = marker.clone();
        std::thread::spawn(move || {
            let mut local = marker.local();
            for (i, Held(address)) in handed.into_iter().enumerate() {
                // SAFETY: a live `Node` of the heap marking.
                unsafe { local.hand(address) };
                if i % 7 == 0 {
                    local.publish();
                }
            }
        })
    };
    for _ in 0..20 {
        heap.mutate(|cx| {
            for _ in 0..50 {
                cx.alloc(PlainInit { next: None });
            }
        });
        heap.scavenge();
        heap.advance_marking(false, STEP);
    }
    engine_thread.join().unwrap();
    finish(&mut heap);
    heap.mutate(|cx| {
        for (tree, &(held, id)) in tops.iter().enumerate() {
            if tree % 2 == 0 {
                let mut n = Some(held.get(cx));
                let mut count = 0;
                while let Some(node) = n {
                    count += 1;
                    n = node.next.get(cx);
                }
                assert_eq!(count, DEPTH, "tree {tree} whole");
            } else {
                assert!(cx.by_id::<Node>(id).is_none(), "tree {tree} freed");
            }
        }
    });
    let mut expected: Vec<u64> = (0..TREES)
        .step_by(2)
        .flat_map(|tree| (0..DEPTH).map(move |depth| u64::from(tree * DEPTH + depth + 1)))
        .collect();
    expected.sort_unstable();
    assert_eq!(engine.marked(), expected);
}

/// With no worker to mark, the heap's thread marks in the engine's steps
/// (its schedule's share, or 64 KiB's worth when no progress was made since
/// the last): the marking is done before the engine's pause, which marks
/// nothing.
#[test]
fn the_heaps_thread_marks_in_the_engines_steps() {
    const NODES: u32 = 20_000;
    let mut heap = Heap::with_platform(
        HeapOptions {
            driven: true,
            nursery_bytes: 0,
            ..crate::verified()
        },
        Arc::new(InlinePlatform),
    );
    let _list = heap.mutate(|cx| {
        let mut next = None;
        for i in 0..NODES {
            next = Some(node(cx, i, next));
        }
        Persistent::new(cx, next.unwrap())
    });
    let _marker = heap.start_marking(Arc::new(Engine::default()));
    let mut steps = 0;
    while !heap.advance_marking(false, Duration::MAX) {
        steps += 1;
        assert!(steps < 1000, "marking makes progress");
    }
    // A step marks 1024 objects (64 KiB, an object 64 bytes before the
    // first GC) after one that marked none, while the marking is within
    // the schedule's 500 ms. Not under Miri: it is past them by then, and
    // a step catches up with the rest.
    if !cfg!(miri) {
        assert!(steps > 2 * (NODES as usize / 1024) - 4, "{steps} steps");
    }
    finish(&mut heap);
    let record = heap.stats().last_gc;
    assert_eq!(record.marked_objects, NODES as usize);
    assert_eq!(record.marked_in_steps, NODES as usize);
    assert_eq!(record.marked_in_pause, 0);
}

/// A step stops at its time: with no time, it marks a segment's worth at
/// most (it looks at the time once in a while), the rest in the pause.
#[test]
fn a_step_stops_at_its_time() {
    const NODES: u32 = 20_000;
    let mut heap = Heap::with_platform(
        HeapOptions {
            driven: true,
            nursery_bytes: 0,
            ..crate::verified()
        },
        Arc::new(InlinePlatform),
    );
    let _list = heap.mutate(|cx| {
        let mut next = None;
        for i in 0..NODES {
            next = Some(node(cx, i, next));
        }
        Persistent::new(cx, next.unwrap())
    });
    let _marker = heap.start_marking(Arc::new(Engine::default()));
    assert!(!heap.advance_marking(false, Duration::ZERO));
    finish(&mut heap);
    let record = heap.stats().last_gc;
    assert_eq!(record.marked_objects, NODES as usize);
    assert!(record.marked_in_steps <= 64, "{}", record.marked_in_steps);
    assert_eq!(
        record.marked_in_pause + record.marked_in_steps,
        NODES as usize
    );
}

/// A worker's objects count as it marks, not when its run ends: a step in
/// the middle of a long run sees them, finds the marking ahead of its
/// schedule, and marks nothing (counted at the end, the step would see no
/// progress and mark 64 KiB's worth).
#[test]
fn a_workers_objects_count_while_it_marks() {
    use std::sync::mpsc;

    /// Asked to yield the 20th time (1280 objects in): says it reached
    /// that, and waits for the step to be done.
    struct Paused {
        asked: Cell<usize>,
        reached: mpsc::Sender<()>,
        go: Mutex<mpsc::Receiver<()>>,
    }

    impl elise::JobDelegate for Paused {
        fn notify_concurrency_increase(&self) {}

        fn is_joining_thread(&self) -> bool {
            false
        }

        fn should_yield(&self) -> bool {
            self.asked.set(self.asked.get() + 1);
            if self.asked.get() == 20 {
                self.reached.send(()).unwrap();
                self.go.lock().unwrap().recv().unwrap();
                return true;
            }
            false
        }
    }

    const NODES: u32 = 5000;
    let (mut heap, steps) = driven(0);
    let _list = heap.mutate(|cx| {
        let mut next = None;
        for i in 0..NODES {
            next = Some(node(cx, i, next));
        }
        Persistent::new(cx, next.unwrap())
    });
    let _marker = heap.start_marking(Arc::new(Engine::default()));
    let job = steps.jobs().pop().expect("the marking's job");
    let (reached, reached_rx) = mpsc::channel();
    let (go, go_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        job.run(&Paused {
            asked: Cell::new(0),
            reached,
            go: Mutex::new(go_rx),
        });
    });
    reached_rx.recv().unwrap();
    // 1024 counted: ahead of a schedule of 5000 objects in 500 ms.
    heap.advance_marking(false, Duration::MAX);
    go.send(()).unwrap();
    worker.join().unwrap();
    finish(&mut heap);
    let record = heap.stats().last_gc;
    assert_eq!(record.marked_objects, NODES as usize);
    assert_eq!(record.marked_in_steps, 0);
}

/// An ephemeron the heap's thread scans before its key is marked: once
/// its step runs out of segments, the key marked since, the step marks the
/// value (a chain), as a worker's run does; the pause marks nothing.
#[test]
fn a_step_marks_the_values_of_ephemerons_whose_key_got_marked() {
    #[derive(GcObject)]
    struct Table {
        pair: elise::EphemeronPair<Node, Node>,
    }

    const CHAIN: u32 = 3000;
    let mut heap = Heap::with_platform(
        HeapOptions {
            driven: true,
            nursery_bytes: 0,
            ..crate::verified()
        },
        Arc::new(InlinePlatform),
    );
    fn chain<'gc>(cx: &elise::Mutation<'gc>, from: u32) -> elise::Gc<'gc, Node> {
        let mut next = None;
        for i in 0..CHAIN {
            next = Some(node(cx, from + i, next));
        }
        next.unwrap()
    }
    // The key at the end of a chain from one root; the table, the other
    // root, scanned first (the last root is taken first).
    let (_keys, _table) = heap.mutate(|cx| {
        let keys = chain(cx, 0);
        let mut key = keys;
        while let Some(next) = key.next.get(cx) {
            key = next;
        }
        let table = cx.alloc(TableInit { pair: (None, None) });
        table.pair.set(cx, Some(key), Some(chain(cx, CHAIN)));
        (Persistent::new(cx, keys), Persistent::new(cx, table))
    });
    let _marker = heap.start_marking(Arc::new(Engine::default()));
    let mut steps = 0;
    while !heap.advance_marking(false, Duration::MAX) {
        steps += 1;
        assert!(steps < 1000, "marking makes progress");
    }
    finish(&mut heap);
    let record = heap.stats().last_gc;
    assert_eq!(record.marked_objects, 2 * CHAIN as usize + 1);
    assert_eq!(record.marked_in_pause, 0, "{record:?}");
}

/// A step whose limit runs out with the last object it has to mark (the
/// first step's 1024 objects, cppgc's 64 KiB of 64-byte objects before the
/// heap's first GC; a multiple of the worklist's yield check): out of
/// segments all the same, it gives the values of the ephemerons whose key
/// got marked to the worklist, so the marking isn't done yet. The steps
/// mark them, not the pause.
#[test]
fn a_step_that_runs_out_as_it_drains_still_marks_the_ephemerons_values() {
    #[derive(GcObject)]
    struct Table {
        pair: elise::EphemeronPair<Node, Node>,
    }

    // With the table, the first step's 1024 objects.
    const KEYS: u32 = 1023;
    const VALUES: u32 = 100;
    let mut heap = Heap::with_platform(
        HeapOptions {
            driven: true,
            nursery_bytes: 0,
            ..crate::verified()
        },
        Arc::new(InlinePlatform),
    );
    fn chain<'gc>(cx: &elise::Mutation<'gc>, from: u32, len: u32) -> elise::Gc<'gc, Node> {
        let mut next = None;
        for i in 0..len {
            next = Some(node(cx, from + i, next));
        }
        next.unwrap()
    }
    // The key at the end of a chain from one root; the table, the other
    // root, scanned first (the last root is taken first).
    let (_keys, _table) = heap.mutate(|cx| {
        let keys = chain(cx, 0, KEYS);
        let mut key = keys;
        while let Some(next) = key.next.get(cx) {
            key = next;
        }
        let table = cx.alloc(TableInit { pair: (None, None) });
        table.pair.set(cx, Some(key), Some(chain(cx, KEYS, VALUES)));
        (Persistent::new(cx, keys), Persistent::new(cx, table))
    });
    let _marker = heap.start_marking(Arc::new(Engine::default()));
    assert!(
        !heap.advance_marking(false, Duration::MAX),
        "the values are yet to be marked"
    );
    let mut steps = 0;
    while !heap.advance_marking(false, Duration::MAX) {
        steps += 1;
        assert!(steps < 1000, "marking makes progress");
    }
    finish(&mut heap);
    let record = heap.stats().last_gc;
    assert_eq!(record.marked_objects, (KEYS + VALUES + 1) as usize);
    assert_eq!(record.marked_in_pause, 0, "{record:?}");
}

/// The engine's steps are `Gc.Step` in the trace, its pause's `Gc.Mark`:
/// inside the engine's own events, they tell the heap's share of them.
#[test]
fn the_engines_steps_and_pause_are_in_the_trace() {
    static TRACE: crate::Recorder = crate::Recorder::new(true);
    let mut heap = Heap::with_platform(
        HeapOptions {
            driven: true,
            nursery_bytes: 0,
            ..crate::verified()
        },
        crate::with_trace(Arc::new(InlinePlatform), &TRACE),
    );
    let _kept = heap.mutate(|cx| Persistent::new(cx, node(cx, 1, None)));
    let _marker = heap.start_marking(Arc::new(Engine::default()));
    TRACE.clear();
    heap.advance_marking(false, STEP);
    heap.advance_marking(true, Duration::MAX);
    assert_eq!(
        TRACE.names(),
        ["begin Gc.Step", "end", "begin Gc.Mark", "end"]
    );
    heap.finish_marking();
}

/// An object of another heap (another cage) handed over is none of this
/// marking's: ignored, the marking unchanged.
#[test]
fn another_heaps_object_handed_over_is_ignored() {
    let (mut heap, _steps) = driven(0);
    let (mut other, _other_steps) = driven(0);
    let foreign = other.mutate(|cx| Held::of(cx, node(cx, 1, None)));
    let id = heap.mutate(|cx| cx.new_id(node(cx, 2, None)));
    let marker = heap.start_marking(Arc::new(Engine::default()));
    hand(&marker, vec![foreign]);
    finish(&mut heap);
    heap.mutate(|cx| assert!(cx.by_id::<Node>(id).is_none(), "nothing kept"));
}

/// Reading back another heap's object in this heap's operation panics:
/// it isn't in this heap's cage.
#[test]
#[should_panic(expected = "an object of this heap")]
fn another_heaps_object_read_back_panics() {
    let (mut heap, _steps) = driven(0);
    let (mut other, _other_steps) = driven(0);
    let foreign = other.mutate(|cx| Held::of(cx, node(cx, 1, None)));
    heap.mutate(|cx| {
        foreign.get(cx);
    });
}

/// In a heap no engine drives, a JS edge is the embedder's word: 0 when the
/// object is made, what was set after, kept as a scavenge moves the
/// object, and as a full GC runs.
#[test]
fn an_undriven_heaps_js_edge_is_the_embedders_word() {
    let mut heap = Heap::new(HeapOptions {
        nursery_bytes: 256 << 10,
        ..crate::verified()
    });
    let root = heap.mutate(|cx| {
        let node = node(cx, 1, None);
        assert_eq!(node.wrapper.embedder_word(cx), 0);
        node.wrapper.set_embedder_word(cx, 0x1234_5678_9abc_def0);
        Persistent::new(cx, node)
    });
    let before = heap.mutate(|cx| std::ptr::from_ref::<Node>(&root.get(cx)).addr());
    heap.scavenge();
    heap.mutate(|cx| {
        let node = root.get(cx);
        assert_ne!(
            std::ptr::from_ref::<Node>(&node).addr(),
            before,
            "the scavenge moved it"
        );
        assert_eq!(node.wrapper.embedder_word(cx), 0x1234_5678_9abc_def0);
        node.wrapper.set_embedder_word(cx, 7);
    });
    heap.collect();
    heap.mutate(|cx| assert_eq!(root.get(cx).wrapper.embedder_word(cx), 7));
}

/// In a driven heap the edge's bytes are the engine's reference: neither
/// reading nor writing them as the embedder's word is allowed.
#[test]
fn a_driven_heaps_js_edge_isnt_the_embedders() {
    let (mut heap, _steps) = driven(0);
    for write in [false, true] {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            heap.mutate(|cx| {
                let node = node(cx, 1, None);
                if write {
                    node.wrapper.set_embedder_word(cx, 1);
                } else {
                    node.wrapper.embedder_word(cx);
                }
            });
        }));
        assert!(result.is_err(), "write: {write}");
    }
}
