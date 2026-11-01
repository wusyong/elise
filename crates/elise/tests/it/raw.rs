//! Types registered as the process runs (`raw`): their layouts checked;
//! their objects allocated zeroed, kept through a scavenge and a full GC by
//! their edges (a struct's, an array's elements), moved whole, with both
//! barriers on their edges, pointing to Rust objects and back; a retired
//! type freed, its index taken again, only once every heap finished a full
//! GC after. Every heap runs the verifier after each GC.

#![allow(unsafe_code, reason = "the embedder's side of runtime types")]

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use elise::raw::{self, ArrayLayout, Layout, RegisterError, TypeRange};
use elise::{Erased, Gc, GcObject, Heap, Member, Mutation, Persistent};

use crate::concurrent::{finish, start_marking, stepping_heap};

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

/// A range of the test's own, with `layouts` registered at ids 0 up.
fn types(layouts: &[Layout<'_>]) -> (TypeRange, Vec<u32>) {
    let range = TypeRange::reserve(16).expect("free indices");
    let indices = layouts
        .iter()
        .enumerate()
        .map(|(id, layout)| range.register(id as u32, layout, Box::new(())).unwrap())
        .collect();
    (range, indices)
}

fn pair<'gc>(
    cx: &Mutation<'gc>,
    ty: u32,
    left: Option<Gc<'gc, Erased>>,
    right: Option<Gc<'gc, Erased>>,
    value: u64,
) -> Gc<'gc, Erased> {
    let object = raw::alloc(cx, ty);
    // SAFETY: the pair's edges and its `u64`, by its layout.
    unsafe {
        raw::write_edge(cx, object, 0, left);
        raw::write_edge(cx, object, 4, right);
        raw::data(object).as_ptr().add(8).cast::<u64>().write(value);
    }
    object
}

fn value(object: Gc<'_, Erased>) -> u64 {
    // SAFETY: a pair's `u64`.
    unsafe { raw::data(object).as_ptr().add(8).cast::<u64>().read() }
}

fn left<'gc>(cx: &Mutation<'gc>, object: Gc<'gc, Erased>) -> Option<Gc<'gc, Erased>> {
    // SAFETY: a pair's first edge.
    unsafe { raw::read_edge(cx, object, 0) }
}

/// The edge of element `i` of an array of `EDGES`.
fn element_at(i: u32) -> u32 {
    8 + 4 * i
}

/// A list of `len` pairs, each valued its place from the head, linked by
/// their left edges, their right edges to a garbage pair each.
fn list(heap: &mut Heap, ty: u32, len: u64) -> Persistent<Erased> {
    heap.mutate(|cx| {
        let mut head = None;
        for at in (0..len).rev() {
            let garbage = pair(cx, ty, None, None, u64::MAX);
            head = Some(pair(cx, ty, head, Some(garbage), at));
            pair(cx, ty, None, None, u64::MAX);
        }
        Persistent::new(cx, head.unwrap())
    })
}

fn check_list(heap: &mut Heap, list: &Persistent<Erased>, len: u64) {
    heap.mutate(|cx| {
        let mut node = Some(list.get(cx));
        for at in 0..len {
            let here = node.unwrap();
            assert_eq!(value(here), at);
            // SAFETY: a pair's second edge.
            let right = unsafe { raw::read_edge(cx, here, 4) }.unwrap();
            assert_eq!(value(right), u64::MAX);
            node = left(cx, here);
        }
        assert!(node.is_none());
    });
}

#[test]
fn a_struct_types_objects_live_through_scavenges_and_full_gcs() {
    let (_range, ty) = types(&[PAIR]);
    let mut heap = Heap::new(crate::verified());
    let list = list(&mut heap, ty[0], 100);
    heap.scavenge();
    check_list(&mut heap, &list, 100);
    heap.scavenge();
    check_list(&mut heap, &list, 100);
    heap.collect();
    check_list(&mut heap, &list, 100);
    assert!(heap.stats().last_gc.marked_objects >= 200);
}

#[test]
fn an_arrays_edges_keep_its_elements() {
    let (_range, ty) = types(&[PAIR, EDGES]);
    let mut heap = Heap::new(crate::verified());
    // A small array, in the nursery, and one larger than a slot: old.
    let arrays = heap.mutate(|cx| {
        [10, 100_000].map(|len| {
            let array = raw::alloc_array(cx, ty[1], len).unwrap();
            for i in (0..len).step_by(997) {
                let element = pair(cx, ty[0], None, None, u64::from(i));
                // SAFETY: an element below the array's length.
                unsafe { raw::write_edge(cx, array, element_at(i), Some(element)) };
            }
            Persistent::new(cx, array)
        })
    });
    let check = |heap: &mut Heap| {
        heap.mutate(|cx| {
            for (array, len) in arrays.iter().zip([10, 100_000]) {
                let array = array.get(cx);
                // SAFETY: the array's length.
                assert_eq!(unsafe { raw::data(array).cast::<u32>().read() }, len);
                for i in 0..len {
                    // SAFETY: an element below the length.
                    let element = unsafe { raw::read_edge(cx, array, element_at(i)) };
                    if i % 997 == 0 {
                        assert_eq!(value(element.unwrap()), u64::from(i));
                    } else {
                        assert!(element.is_none());
                    }
                }
            }
        });
    };
    heap.scavenge();
    check(&mut heap);
    heap.collect();
    check(&mut heap);
}

/// A scavenge copies an array of data whole: its length's elements.
#[test]
fn an_array_of_data_moves_whole() {
    let (_range, ty) = types(&[WORDS]);
    let mut heap = Heap::new(crate::verified());
    let array = heap.mutate(|cx| {
        let array = raw::alloc_array(cx, ty[0], 50).unwrap();
        for i in 0..50 {
            // SAFETY: an element below the length.
            unsafe {
                raw::data(array)
                    .as_ptr()
                    .add(8 + 8 * i)
                    .cast::<u64>()
                    .write(i as u64)
            };
        }
        Persistent::new(cx, array)
    });
    heap.scavenge();
    // Its header, its length's word and its elements, copied.
    assert_eq!(heap.stats().last_scavenge.survived_bytes, 8 + 8 + 400);
    heap.scavenge();
    assert_eq!(heap.stats().last_scavenge.promoted_objects, 1);
    heap.collect();
    heap.mutate(|cx| {
        let data = raw::data(array.get(cx));
        for i in 0..50 {
            // SAFETY: as above.
            let element = unsafe { data.as_ptr().add(8 + 8 * i).cast::<u64>().read() };
            assert_eq!(element, i as u64);
        }
    });
}

/// Room a dead object of ones bits took is zeroed for the next.
#[test]
fn objects_are_allocated_zeroed() {
    let (_range, ty) = types(&[WORDS, PAIR]);
    let mut heap = Heap::new(crate::verified());
    for _ in 0..3 {
        heap.mutate(|cx| {
            for _ in 0..200 {
                let array = raw::alloc_array(cx, ty[0], 8).unwrap();
                // SAFETY: the array's elements.
                unsafe { raw::data(array).as_ptr().add(8).write_bytes(0xff, 64) };
                let pair = raw::alloc(cx, ty[1]);
                // SAFETY: the pair's bytes; its edges get ones bits, which
                // the GC never reads: it is dead before the next safepoint.
                unsafe { raw::data(pair).as_ptr().write_bytes(0xff, 16) };
            }
        });
        heap.scavenge();
        heap.mutate(|cx| {
            cx.pretenured(|cx| {
                for _ in 0..200 {
                    let pair = raw::alloc(cx, ty[1]);
                    // SAFETY: as above.
                    unsafe { raw::data(pair).as_ptr().write_bytes(0xff, 16) };
                }
            });
        });
        heap.collect();
    }
    heap.mutate(|cx| {
        for _ in 0..200 {
            let array = raw::alloc_array(cx, ty[0], 8).unwrap();
            // SAFETY: the array's bytes.
            let bytes = unsafe { std::slice::from_raw_parts(raw::data(array).as_ptr(), 72) };
            assert_eq!(bytes[..4], 8u32.to_ne_bytes());
            assert!(bytes[4..].iter().all(|&byte| byte == 0));
        }
        cx.pretenured(|cx| {
            for _ in 0..200 {
                let pair = raw::alloc(cx, ty[1]);
                assert!(left(cx, pair).is_none());
                assert_eq!(value(pair), 0);
            }
        });
    });
}

#[derive(GcObject)]
struct Holder {
    raw: Member<Erased>,
    value: Cell<u32>,
}

/// A Rust object's `Member<Erased>` keeps a raw one, whose edge keeps
/// another Rust object, read back as one by `downcast`.
#[test]
fn rust_and_raw_objects_point_to_each_other() {
    let (_range, ty) = types(&[PAIR]);
    let mut heap = Heap::new(crate::verified());
    let root = heap.mutate(|cx| {
        let inner = cx.alloc(HolderInit {
            raw: None,
            value: Cell::new(2),
        });
        let raw = pair(cx, ty[0], Some(inner.erase()), None, 1);
        Persistent::new(
            cx,
            cx.alloc(HolderInit {
                raw: Some(raw),
                value: Cell::new(0),
            }),
        )
    });
    heap.scavenge();
    heap.collect();
    heap.mutate(|cx| {
        let raw = root.get(cx).raw.get(cx).unwrap();
        assert!(raw.downcast::<Holder>().is_none());
        assert_eq!(raw::index_of(raw), ty[0]);
        assert_eq!(value(raw), 1);
        let inner = left(cx, raw).unwrap().downcast::<Holder>().unwrap();
        assert_eq!(inner.value.get(), 2);
    });
}

/// An old raw object's edge to a young one is recorded: a scavenge keeps
/// the young one (the verifier checks the slot set too).
#[test]
fn the_write_barrier_records_an_old_raw_objects_edge_to_a_young_one() {
    let (_range, ty) = types(&[PAIR]);
    let mut heap = Heap::new(crate::verified());
    let old = heap.mutate(|cx| {
        let old = cx.pretenured(|cx| pair(cx, ty[0], None, None, 1));
        let young = pair(cx, ty[0], None, None, 2);
        // SAFETY: the pair's first edge.
        unsafe { raw::write_edge(cx, old, 0, Some(young)) };
        Persistent::new(cx, old)
    });
    heap.scavenge();
    heap.scavenge();
    heap.mutate(|cx| assert_eq!(value(left(cx, old.get(cx)).unwrap()), 2));
}

/// The snapshot reaches `x` only through `a`'s edge; while marking, the
/// mutator moves `x` to a new object (black, never scanned) and cuts
/// `a`'s edge: the barrier marks `x`.
#[test]
fn the_marking_barrier_keeps_what_a_raw_edge_held() {
    let (_range, ty) = types(&[PAIR]);
    let (mut heap, steps) = stepping_heap(0);
    let a = heap.mutate(|cx| {
        cx.pretenured(|cx| {
            let x = pair(cx, ty[0], None, None, 7);
            Persistent::new(cx, pair(cx, ty[0], Some(x), None, 1))
        })
    });
    start_marking(&mut heap);
    let b = heap.mutate(|cx| {
        let a = a.get(cx);
        let b = pair(cx, ty[0], left(cx, a), None, 2);
        // SAFETY: the pair's first edge.
        unsafe { raw::write_edge(cx, a, 0, None) };
        Persistent::new(cx, b)
    });
    finish(&mut heap, &steps);
    heap.mutate(|_| {});
    heap.mutate(|cx| assert_eq!(value(left(cx, b.get(cx)).unwrap()), 7));
}

#[test]
fn a_wrong_layout_is_refused() {
    let range = TypeRange::reserve(1).unwrap();
    let refused = |size, edges: &[u32], array| match range.register(
        0,
        &Layout { size, edges, array },
        Box::new(()),
    ) {
        Err(RegisterError::Layout(why)) => why,
        other => panic!("{other:?}"),
    };
    let array = |length_at, element_bytes, element_edges| {
        Some(ArrayLayout {
            length_at,
            element_bytes,
            element_edges,
        })
    };
    // A quarter of the cage, header included: Miri's cage is 16 MiB.
    let max: u32 = if cfg!(miri) { 4 << 20 } else { 1 << 30 };
    assert_eq!(refused(max, &[], None), "larger than an object may be");
    assert_eq!(refused(16, &[2], None), "an edge isn't 4-aligned");
    assert_eq!(
        refused(16, &[16], None),
        "an edge is past the object's size"
    );
    assert_eq!(refused(16, &[13], None), "an edge isn't 4-aligned");
    assert_eq!(refused(16, &[4, 4], None), "the edges aren't ascending");
    assert_eq!(refused(16, &[8, 4], None), "the edges aren't ascending");
    let length = "the length isn't a 4-aligned u32 of the fixed part";
    assert_eq!(refused(8, &[], array(2, 4, false)), length);
    assert_eq!(refused(8, &[], array(8, 4, false)), length);
    assert_eq!(
        refused(8, &[4], array(4, 4, false)),
        "the length is an edge"
    );
    assert_eq!(
        refused(8, &[], array(0, 3, false)),
        "an element isn't 1, 2, 4, 8 or 16 bytes"
    );
    assert_eq!(
        refused(4, &[], array(0, 8, false)),
        "the elements aren't aligned"
    );
    assert_eq!(
        refused(12, &[], array(0, 16, false)),
        "the elements aren't aligned"
    );
    assert_eq!(
        refused(8, &[], array(0, 8, true)),
        "an edge element isn't 4 bytes"
    );
    // Their bounds, accepted.
    let ok = |size, edges: &[u32], array| {
        let range = TypeRange::reserve(1).unwrap();
        range
            .register(0, &Layout { size, edges, array }, Box::new(()))
            .unwrap();
    };
    ok(max - 8, &[], None);
    ok(16, &[0, 12], None);
    ok(8, &[0], array(4, 4, true));
    ok(16, &[], array(12, 16, false));
    ok(4, &[], array(0, 4, true));
}

#[test]
fn an_index_is_registered_once_in_its_range() {
    let range = TypeRange::reserve(2).unwrap();
    assert_eq!(range.len(), 2);
    assert_eq!(range.index(1), Some(range.base() + 1));
    assert_eq!(range.index(2), None);
    assert_eq!(range.register(1, &PAIR, Box::new(())), Ok(range.base() + 1));
    assert_eq!(
        range.register(1, &PAIR, Box::new(())),
        Err(RegisterError::Occupied)
    );
    assert_eq!(
        range.register(2, &PAIR, Box::new(())),
        Err(RegisterError::OutOfRange)
    );
    assert!(range.base() >= elise::MAX_TYPES as u32);
    assert!(range.base() + 2 <= raw::MAX_INDEX);
    assert!(TypeRange::reserve(raw::MAX_INDEX).is_none());
}

#[test]
fn an_array_too_large_isnt_allocated() {
    let (_range, ty) = types(&[WORDS]);
    let mut heap = Heap::new(crate::verified());
    heap.mutate(|cx| {
        assert!(raw::alloc_array(cx, ty[0], u32::MAX).is_none());
        assert!(raw::alloc_array(cx, ty[0], (1 << 27) - 1).is_none());
    });
}

#[test]
#[should_panic = "is a static one"]
fn a_static_type_isnt_allocated_raw() {
    let mut heap = Heap::new(crate::verified());
    heap.mutate(|cx| {
        raw::alloc(cx, elise::type_index::<Holder>());
    });
}

#[test]
#[should_panic = "is an array"]
fn an_array_type_isnt_allocated_as_a_struct() {
    let (_range, ty) = types(&[WORDS]);
    let mut heap = Heap::new(crate::verified());
    heap.mutate(|cx| {
        raw::alloc(cx, ty[0]);
    });
}

#[test]
#[should_panic = "isn't an array"]
fn a_struct_type_isnt_allocated_as_an_array() {
    let (_range, ty) = types(&[PAIR]);
    let mut heap = Heap::new(crate::verified());
    heap.mutate(|cx| {
        raw::alloc_array(cx, ty[0], 1);
    });
}

/// The heap alive as the type retires waits for it: the type stays
/// retired, which allocation refuses.
#[test]
#[should_panic = "is retired"]
fn a_retired_type_isnt_allocated() {
    let (range, ty) = types(&[PAIR]);
    let mut heap = Heap::new(crate::verified());
    // SAFETY: nothing allocated the type.
    unsafe { range.retire(0) };
    heap.mutate(|cx| {
        raw::alloc(cx, ty[0]);
    });
}

/// Counts its drops.
struct Drops(Arc<AtomicUsize>);

impl Drop for Drops {
    fn drop(&mut self) {
        #[expect(clippy::disallowed_methods, reason = "a test's count")]
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

/// Runs `test` in a process of its own, the test binary run again for the
/// test named `name` alone: a retired type waits for every heap of the
/// process, which other tests' would hold.
fn alone(name: &str, test: impl FnOnce()) {
    if std::env::var_os("ELISE_TEST_ALONE").is_some() {
        test();
        return;
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([name, "--exact", "--test-threads=1"])
        .env("ELISE_TEST_ALONE", "1")
        .status()
        .unwrap();
    assert!(status.success(), "{name} alone: {status}");
}

/// A type retired as a concurrent marking runs: that GC may still read its
/// objects (its snapshot), so its entry, and the embedder's keepalive, is
/// freed only once the next full GC finished; until then its index is
/// taken. The process's one heap: another would hold it too.
#[test]
#[cfg_attr(miri, ignore = "runs the test binary again")]
fn a_type_retired_while_marking_waits_for_the_next_full_gc() {
    alone(
        "raw::a_type_retired_while_marking_waits_for_the_next_full_gc",
        || {
            let range = TypeRange::reserve(1).unwrap();
            let drops = Arc::new(AtomicUsize::new(0));
            let ty = range
                .register(0, &PAIR, Box::new(Drops(drops.clone())))
                .unwrap();
            let (mut heap, steps) = stepping_heap(0);
            let list = list(&mut heap, ty, 20);
            heap.collect();
            start_marking(&mut heap);
            drop(list);
            // SAFETY: nothing allocates the type any more, and no object of
            // it is reachable: the list's root is gone.
            unsafe { range.retire(0) };
            finish(&mut heap, &steps);
            assert!(heap.is_sweeping());
            steps.step();
            heap.mutate(|_| {});
            assert!(!heap.is_sweeping());
            assert_eq!(drops.load(Ordering::Relaxed), 0, "its marking began before");
            assert_eq!(
                range.register(0, &PAIR, Box::new(())),
                Err(RegisterError::Occupied)
            );
            heap.collect();
            assert_eq!(drops.load(Ordering::Relaxed), 1);
            let ty = range.register(0, &EDGES, Box::new(())).unwrap();
            heap.mutate(|cx| {
                let array = raw::alloc_array(cx, ty, 3).unwrap();
                assert_eq!(raw::index_of(array), ty);
            });
            heap.collect();
        },
    );
}

/// Each heap alive as a type retires is waited for: one that collected
/// till it is dropped, one that didn't till it collects.
#[test]
#[cfg_attr(miri, ignore = "runs the test binary again")]
fn a_retired_type_waits_for_every_heap() {
    alone("raw::a_retired_type_waits_for_every_heap", || {
        let range = TypeRange::reserve(1).unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let ty = range
            .register(0, &PAIR, Box::new(Drops(drops.clone())))
            .unwrap();
        let mut heap = Heap::new(crate::verified());
        let mut other = Heap::new(crate::verified());
        let list = list(&mut heap, ty, 20);
        drop(list);
        // SAFETY: as above.
        unsafe { range.retire(0) };
        heap.collect();
        assert_eq!(drops.load(Ordering::Relaxed), 0, "the other heap waits");
        drop(heap);
        assert_eq!(drops.load(Ordering::Relaxed), 0, "the other heap waits");
        other.collect();
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        let mut late = Heap::new(crate::verified());
        range
            .register(0, &PAIR, Box::new(Drops(drops.clone())))
            .unwrap();
        // SAFETY: nothing allocated it.
        unsafe { range.retire(0) };
        late.collect();
        drop(other);
        assert_eq!(drops.load(Ordering::Relaxed), 2, "both collected or gone");
    });
}

/// A released range's indices are free again, but those of its types not
/// retired; a retired one's once freed.
#[test]
#[cfg_attr(miri, ignore = "runs the test binary again")]
fn a_released_range_frees_its_indices() {
    alone("raw::a_released_range_frees_its_indices", || {
        let range = TypeRange::reserve(3).unwrap();
        let base = range.base();
        range.register(0, &PAIR, Box::new(())).unwrap();
        range.register(1, &PAIR, Box::new(())).unwrap();
        let mut heap = Heap::new(crate::verified());
        // SAFETY: nothing allocated the type.
        unsafe { range.retire(1) };
        drop(range);
        let next = TypeRange::reserve(1).unwrap();
        assert_eq!(next.base(), base + 2, "the empty one");
        heap.collect();
        let freed = TypeRange::reserve(1).unwrap();
        assert_eq!(freed.base(), base + 1, "the retired one, freed");
        assert_ne!(TypeRange::reserve(1).unwrap().base(), base, "registered");
    });
}
