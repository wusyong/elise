//! A heap an engine's code alone uses (`raw::EngineHeap`): it moves between
//! threads and is read from several at once; it allocates and sizes
//! objects through bits, writes their edges with the barriers, and keeps
//! them by the roots its GCs are given. Every heap runs the verifier after
//! each GC.

#![allow(unsafe_code, reason = "the engine's side of an engine heap")]

use std::sync::Arc;

use elise::raw::{AllocError, ArrayLayout, CAGE_BYTES, EngineHeap, Layout, TypeRange};
use elise::{HeapOptions, InlinePlatform};

/// Two edges, then a `u64`.
const PAIR: Layout<'static> = Layout {
    size: 16,
    edges: &[0, 4],
    array: None,
};

/// A length, then edges.
const EDGES: Layout<'static> = Layout {
    size: 8,
    edges: &[],
    array: Some(ArrayLayout {
        length_at: 0,
        element_bytes: 4,
        element_edges: true,
    }),
};

/// A length, then `u64`s.
const WORDS: Layout<'static> = Layout {
    size: 8,
    edges: &[],
    array: Some(ArrayLayout {
        length_at: 0,
        element_bytes: 8,
        element_edges: false,
    }),
};

/// The test's own range: `PAIR`, `EDGES`, `WORDS`.
fn types() -> (TypeRange, [u32; 3]) {
    let range = TypeRange::reserve(3).expect("free indices");
    let index = |id, layout| range.register(id, layout, Box::new(())).unwrap();
    let indices = [index(0, &PAIR), index(1, &EDGES), index(2, &WORDS)];
    (range, indices)
}

fn heap(options: HeapOptions) -> EngineHeap {
    EngineHeap::new(options, Arc::new(InlinePlatform))
}

fn give(slots: &mut [u32]) -> impl FnMut(&mut dyn FnMut(&mut u32)) + '_ {
    move |visit| {
        for slot in slots.iter_mut() {
            visit(slot);
        }
    }
}

/// A pair, `left` and `right` its edges' bits, `value` its `u64`.
fn pair(heap: &mut EngineHeap, ty: u32, left: u32, right: u32, value: u64) -> u32 {
    let object = heap.alloc(ty).expect("no GC due");
    // SAFETY: the pair just allocated, its edges and its `u64`; `left` and
    // `right` the caller's, live.
    unsafe {
        heap.write_bits(object, 0, left);
        heap.write_bits(object, 4, right);
        heap.cage()
            .add(object as usize + 8)
            .cast::<u64>()
            .write(value);
    }
    object
}

/// The `u64` of the pair with `object` bits.
fn value(heap: &EngineHeap, object: u32) -> u64 {
    // SAFETY: a live pair's `u64`.
    unsafe { heap.cage().add(object as usize + 8).cast::<u64>().read() }
}

/// The bits of the edge at `at` of the pair with `object` bits.
fn edge(heap: &EngineHeap, object: u32, at: u32) -> u32 {
    // SAFETY: a live pair's edge.
    unsafe { heap.read_bits(object, at) }
}

fn send_sync<T: Send + Sync>() {}

#[test]
fn an_engine_heap_is_send_and_sync() {
    send_sync::<EngineHeap>();
}

#[test]
fn objects_move_with_their_heap_to_another_thread() {
    let (_range, [pair_ty, ..]) = types();
    let mut heap = heap(crate::verified());
    let right = pair(&mut heap, pair_ty, 0, 0, 2);
    let left = pair(&mut heap, pair_ty, right, 0, 1);
    let (heap, root) = std::thread::spawn(move || {
        let mut roots = [left];
        // SAFETY: the root is a live pair's bits, given twice alike.
        unsafe { heap.collect_with_roots(give(&mut roots)) };
        (heap, roots[0])
    })
    .join()
    .unwrap();
    assert_eq!(value(&heap, root), 1);
    assert_eq!(value(&heap, edge(&heap, root, 0)), 2);
}

#[test]
fn several_threads_read_an_engine_heap_at_once() {
    let (_range, [pair_ty, ..]) = types();
    let mut heap = heap(crate::verified());
    let right = pair(&mut heap, pair_ty, 0, 0, 2);
    let left = pair(&mut heap, pair_ty, right, 0, 1);
    let heap = &heap;
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(move || {
                assert_eq!(edge(heap, left, 0), right);
                assert_eq!(value(heap, right), 2);
                // SAFETY: a live object's bits.
                assert_eq!(unsafe { heap.size(left) }, 16);
            });
        }
    });
}

#[test]
fn an_objects_size_is_its_layouts() {
    let (_range, [pair_ty, edges_ty, words_ty]) = types();
    let mut heap = heap(crate::verified());
    let pair = heap.alloc(pair_ty).unwrap();
    let edges = heap.alloc_array(edges_ty, 3).unwrap();
    let words = heap.alloc_array(words_ty, 5).unwrap();
    // SAFETY: live objects' bits; an array's length, its first word.
    unsafe {
        assert_eq!(heap.size(pair), 16);
        assert_eq!(heap.size(edges), 8 + 3 * 4);
        assert_eq!(heap.size(words), 8 + 5 * 8);
        assert_eq!(heap.cage().add(words as usize).cast::<u32>().read(), 5);
    }
    assert!(heap.used_bytes() >= 16 + 20 + 48);
    assert!((words as usize) < CAGE_BYTES);
}

#[test]
fn a_scavenge_and_a_full_gc_move_and_keep_what_the_roots_hold() {
    let (_range, [pair_ty, edges_ty, ..]) = types();
    let mut heap = heap(crate::verified());
    let right = pair(&mut heap, pair_ty, 0, 0, 2);
    let left = pair(&mut heap, pair_ty, right, 0, 1);
    for _ in 0..100 {
        heap.alloc_array(edges_ty, 16).unwrap();
    }
    let used = heap.used_bytes();
    let mut roots = [left];
    // SAFETY: the root is a live pair's bits, given twice alike.
    unsafe { heap.scavenge_with_roots(give(&mut roots)) };
    assert_ne!(roots[0], left, "the scavenge moved it");
    assert!(heap.used_bytes() < used, "the garbage is gone");
    assert_eq!(value(&heap, edge(&heap, roots[0], 0)), 2);
    let right = pair(&mut heap, pair_ty, 0, 0, 3);
    // SAFETY: the pair is live, the root holds it; so is `right`.
    unsafe { heap.write_bits(roots[0], 4, right) };
    // SAFETY: as above.
    unsafe { heap.collect_with_roots(give(&mut roots)) };
    assert_eq!(value(&heap, edge(&heap, roots[0], 4)), 3);
}

#[test]
fn a_scavenge_leaves_old_garbage_to_the_full_gc() {
    let (_range, [pair_ty, ..]) = types();
    let mut heap = heap(crate::verified());
    let mut roots = [pair(&mut heap, pair_ty, 0, 0, 1)];
    // SAFETY: the root is a live pair's bits, twice: now it's old.
    unsafe {
        heap.scavenge_with_roots(give(&mut roots));
        heap.scavenge_with_roots(give(&mut roots));
    }
    let used = heap.used_bytes();
    // SAFETY: no roots; the pair is old garbage.
    unsafe { heap.scavenge_with_roots(give(&mut [])) };
    assert_eq!(heap.used_bytes(), used, "a scavenge leaves the old alone");
    // SAFETY: as above.
    unsafe { heap.collect_with_roots(give(&mut [])) };
    assert!(heap.used_bytes() < used, "the full GC frees it");
}

#[test]
fn an_id_lives_while_its_object_does() {
    let (_range, [pair_ty, ..]) = types();
    let mut heap = heap(crate::verified());
    let young = pair(&mut heap, pair_ty, 0, 0, 1);
    let mut roots = [pair(&mut heap, pair_ty, 0, 0, 2)];
    // SAFETY: live objects' bits.
    let (young, old) = unsafe { (heap.new_id(young), heap.new_id(roots[0])) };
    assert!(heap.id_lives(young) && heap.id_lives(old));
    // SAFETY: the root is a live pair's bits, twice: now it's old.
    unsafe {
        heap.scavenge_with_roots(give(&mut roots));
        heap.scavenge_with_roots(give(&mut roots));
    }
    assert!(!heap.id_lives(young), "the scavenge dropped it");
    assert!(heap.id_lives(old), "moved, and promoted");
    // SAFETY: no roots.
    unsafe { heap.scavenge_with_roots(give(&mut [])) };
    assert!(heap.id_lives(old), "a scavenge leaves the old alone");
    // SAFETY: no roots.
    unsafe { heap.collect_with_roots(give(&mut [])) };
    assert!(!heap.id_lives(old), "the full GC dropped it");
}

#[test]
fn asking_whether_an_id_lives_doesnt_keep_its_object() {
    let (_range, [pair_ty, ..]) = types();
    let (platform, steps) = crate::concurrent::stepping_platform();
    // Every safepoint GCs: by turns a scavenge, and a marking's start or
    // end, the marking running between them.
    let mut heap = EngineHeap::new(
        HeapOptions {
            stress: true,
            ..crate::verified()
        },
        platform,
    );
    // SAFETY: no roots; under stress the allocation after it is the one
    // that succeeds.
    unsafe { heap.safepoint_with_roots(give(&mut [])) };
    let mut roots = [pair(&mut heap, pair_ty, 0, 0, 1)];
    // SAFETY: the root is a live pair's bits, twice: now it's old.
    unsafe {
        heap.scavenge_with_roots(give(&mut roots));
        heap.scavenge_with_roots(give(&mut roots));
    }
    // SAFETY: as above.
    let id = unsafe { heap.new_id(roots[0]) };
    // Let go: asked about at every step, it dies at the end of a marking.
    let mut safepoints = 0;
    while heap.id_lives(id) {
        assert!(safepoints < 8, "a marking finished");
        // SAFETY: no roots.
        unsafe { heap.safepoint_with_roots(give(&mut [])) };
        steps.step();
        safepoints += 1;
    }
}

#[test]
fn an_allocation_fails_as_a_gc_is_due_until_the_safepoint_runs() {
    let (_range, [pair_ty, edges_ty, ..]) = types();
    let mut heap = heap(HeapOptions {
        stress: true,
        ..crate::verified()
    });
    assert!(heap.should_collect());
    assert_eq!(heap.alloc(pair_ty), Err(AllocError::GcDue));
    assert_eq!(heap.alloc_array(edges_ty, 1), Err(AllocError::GcDue));
    // SAFETY: no roots.
    unsafe { heap.safepoint_with_roots(give(&mut [])) };
    assert!(heap.alloc(pair_ty).is_ok());
}

#[test]
fn frames_are_entered_and_left() {
    let mut heap = heap(crate::verified());
    assert!(!heap.in_frames());
    heap.enter_frames();
    heap.enter_frames();
    heap.leave_frames();
    assert!(heap.in_frames());
    heap.leave_frames();
    assert!(!heap.in_frames());
}

#[test]
fn the_write_barrier_records_an_old_objects_edge_to_a_young_one() {
    let (_range, [pair_ty, ..]) = types();
    let mut heap = heap(crate::verified());
    let old = pair(&mut heap, pair_ty, 0, 0, 1);
    let mut roots = [old];
    // SAFETY: the root is a live pair's bits.
    unsafe { heap.scavenge_with_roots(give(&mut roots)) };
    // SAFETY: as above, twice: now it's old.
    unsafe { heap.scavenge_with_roots(give(&mut roots)) };
    let young = pair(&mut heap, pair_ty, 0, 0, 2);
    // SAFETY: both live.
    unsafe { heap.write_bits(roots[0], 0, young) };
    // SAFETY: as above.
    unsafe { heap.scavenge_with_roots(give(&mut roots)) };
    assert_eq!(value(&heap, edge(&heap, roots[0], 0)), 2);
}

#[test]
fn an_array_too_large_isnt_allocated() {
    let (_range, [_, _, words_ty]) = types();
    let mut heap = heap(crate::verified());
    assert_eq!(
        heap.alloc_array(words_ty, u32::MAX),
        Err(AllocError::TooLarge)
    );
}
