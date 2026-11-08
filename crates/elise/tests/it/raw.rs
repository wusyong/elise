//! Types registered as the process runs (`raw`): their layouts checked;
//! their objects allocated zeroed, kept through a scavenge and a full GC by
//! their edges (a struct's, an array's elements), moved whole, with both
//! barriers on their edges, pointing to Rust objects and back; a retired
//! type freed, its index taken again, only once every heap finished a full
//! GC after. Every heap runs the verifier after each GC.

#![allow(unsafe_code, reason = "the embedder's side of runtime types")]

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use elise::raw::{self, ArrayLayout, I31, Layout, Ref, RegisterError, TypeRange};
use elise::{Erased, Gc, GcObject, Heap, HeapOptions, Member, Mutation, Persistent, RootsSource};

use crate::concurrent::{finish, start_marking, stepping_heap, stepping_heap_with};

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
        raw::write_edge(cx, object, 0, left.map(Ref::from));
        raw::write_edge(cx, object, 4, right.map(Ref::from));
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
    object_in(unsafe { raw::read_edge(cx, object, 0) })
}

/// The object an edge holds, none if it holds none.
///
/// # Panics
///
/// If it holds an `I31`.
fn object_in(value: Option<Ref<'_>>) -> Option<Gc<'_, Erased>> {
    value.map(|value| match value {
        Ref::Object(object) => object,
        Ref::I31(value) => panic!("an I31, {value:?}, where an object was"),
    })
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
            let right = object_in(unsafe { raw::read_edge(cx, here, 4) }).unwrap();
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
                unsafe { raw::write_edge(cx, array, element_at(i), Some(element.into())) };
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
                    let element = object_in(unsafe { raw::read_edge(cx, array, element_at(i)) });
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
        unsafe { raw::write_edge(cx, old, 0, Some(young.into())) };
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

/// An `I31` whose bits fall inside `object` (its offset, plus one): where
/// a GC that took it for an object would read what isn't a header.
fn i31_inside<'gc>(cx: &Mutation<'gc>, object: Gc<'gc, Erased>) -> I31 {
    let offset = raw::bits(cx, Some(object.into()));
    let i31 = I31::wrapping_u32(offset >> 1);
    assert_eq!(raw::bits(cx, Some(i31.into())), offset + 1);
    i31
}

/// The `I31` an edge holds.
///
/// # Panics
///
/// If it holds none, or an object.
fn i31_in(value: Option<Ref<'_>>) -> I31 {
    match value {
        Some(Ref::I31(value)) => value,
        other => panic!("{other:?} where an I31 was"),
    }
}

/// As wasm's `i31ref` and wasmtime's `I31`: 31 bits, wrapped or checked,
/// read zero- or sign-extended.
#[test]
fn an_i31_converts_as_wasms() {
    let max = u32::MAX >> 1;
    assert_eq!(I31::wrapping_u32(u32::MAX).get_u32(), max);
    assert_eq!(I31::wrapping_u32(u32::MAX).get_i32(), -1);
    assert_eq!(I31::wrapping_i32(-1).get_u32(), max);
    assert_eq!(I31::wrapping_i32(i32::MIN).get_i32(), 0);
    assert_eq!(I31::new_u32(max).map(I31::get_u32), Some(max));
    assert_eq!(I31::new_u32(max + 1), None);
    assert_eq!(I31::new_i32(-(1 << 30)).map(I31::get_i32), Some(-(1 << 30)));
    assert_eq!(
        I31::new_i32((1 << 30) - 1).map(I31::get_i32),
        Some((1 << 30) - 1)
    );
    assert_eq!(I31::new_i32(1 << 30), None);
    assert_eq!(I31::new_i32(-(1 << 30) - 1), None);
}

/// An edge's bits: none's are 0, an object's its offset (4-aligned), an
/// `I31`'s its integer shifted left, the lowest bit set; bits written are
/// read back as what they stand for.
#[test]
fn an_edges_bits_are_what_it_holds() {
    let (_range, ty) = types(&[PAIR]);
    let mut heap = Heap::new(crate::verified());
    heap.mutate(|cx| {
        assert_eq!(raw::bits(cx, None), 0);
        assert_eq!(raw::bits(cx, Some(I31::wrapping_u32(5).into())), 11);
        assert_eq!(raw::bits(cx, Some(I31::wrapping_i32(-1).into())), u32::MAX);
        let a = pair(cx, ty[0], None, None, 1);
        let b = pair(cx, ty[0], Some(a), None, 2);
        // SAFETY: the pair's edges; an object's bits read in this
        // operation, and `I31`s'.
        unsafe {
            let bits = raw::read_bits(cx, b, 0);
            assert_eq!(bits, raw::bits(cx, Some(a.into())));
            assert_eq!(bits % 4, 0);
            raw::write_bits(cx, b, 4, bits);
            assert!(Gc::ptr_eq(object_in(raw::read_edge(cx, b, 4)).unwrap(), a));
            raw::write_bits(cx, b, 0, 11);
            assert_eq!(i31_in(raw::read_edge(cx, b, 0)).get_u32(), 5);
            raw::write_edge(cx, b, 0, Some(I31::wrapping_i32(-7).into()));
            assert_eq!(i31_in(raw::read_edge(cx, b, 0)).get_i32(), -7);
            raw::write_edge(cx, b, 0, None);
            assert_eq!(raw::read_bits(cx, b, 0), 0);
        }
    });
}

/// `I31`s in a struct's edge and an array's, beside objects, stay as they
/// are through scavenges and a full GC, each with bits inside a dead
/// object: young for the scavenges, old for the full GC, where taking it
/// for an object would read what isn't a header (or keep the dead).
#[test]
fn i31s_live_through_scavenges_and_full_gcs() {
    let (_range, ty) = types(&[PAIR, EDGES]);
    let mut heap = Heap::new(crate::verified());
    let (root, mut i31s) = heap.mutate(|cx| {
        let array = raw::alloc_array(cx, ty[1], 8).unwrap();
        for i in (0..8).step_by(2) {
            let element = pair(cx, ty[0], None, None, u64::from(i));
            // SAFETY: as above.
            unsafe { raw::write_edge(cx, array, element_at(i), Some(element.into())) };
        }
        let dead = pair(cx, ty[0], None, None, u64::MAX);
        let root = pair(cx, ty[0], None, None, 100);
        // SAFETY: the pair's edges.
        unsafe {
            raw::write_edge(cx, root, 0, Some(i31_inside(cx, dead).into()));
            raw::write_edge(cx, root, 4, Some(array.into()));
        }
        (Persistent::new(cx, root), fill(cx, ty[0], array, false))
    });
    // SAFETY: the pair's first edge.
    let first = heap.mutate(|cx| i31_in(unsafe { raw::read_edge(cx, root.get(cx), 0) }));
    let check = |heap: &mut Heap, i31s: &[I31]| {
        heap.mutate(|cx| {
            let root = root.get(cx);
            // SAFETY: the pair's edges, then the array's elements.
            unsafe {
                assert_eq!(i31_in(raw::read_edge(cx, root, 0)), first);
                let array = object_in(raw::read_edge(cx, root, 4)).unwrap();
                for i in 0..8 {
                    let element = raw::read_edge(cx, array, element_at(i));
                    if i % 2 == 0 {
                        assert_eq!(value(object_in(element).unwrap()), u64::from(i));
                    } else {
                        assert_eq!(i31_in(element), i31s[i as usize / 2]);
                    }
                }
            }
        });
    };
    heap.scavenge();
    check(&mut heap, &i31s);
    heap.scavenge();
    check(&mut heap, &i31s);
    i31s = heap.mutate(|cx| {
        let array = left_of_right(cx, root.get(cx));
        fill(cx, ty[0], array, true)
    });
    heap.collect();
    check(&mut heap, &i31s);
    // The root, the array and its four pairs: no dead one.
    assert_eq!(heap.stats().last_gc.marked_objects, 6);
}

/// An `I31` at each odd element of `array`, of `EDGES`, each inside a pair
/// of type `pair_ty` that dies (`old`, or young): the `I31`s.
fn fill<'gc>(cx: &Mutation<'gc>, pair_ty: u32, array: Gc<'gc, Erased>, old: bool) -> Vec<I31> {
    (1..8)
        .step_by(2)
        .map(|i| {
            let dead = if old {
                cx.pretenured(|cx| pair(cx, pair_ty, None, None, u64::MAX))
            } else {
                pair(cx, pair_ty, None, None, u64::MAX)
            };
            let i31 = i31_inside(cx, dead);
            // SAFETY: an element below the array's length.
            unsafe { raw::write_edge(cx, array, element_at(i), Some(i31.into())) };
            i31
        })
        .collect()
}

/// The array a root pair's second edge holds.
fn left_of_right<'gc>(cx: &Mutation<'gc>, root: Gc<'gc, Erased>) -> Gc<'gc, Erased> {
    // SAFETY: the pair's second edge.
    object_in(unsafe { raw::read_edge(cx, root, 4) }).unwrap()
}

/// An old object's edge, recorded holding a young object, then given an
/// `I31` with bits inside that object, which dies: the scavenge, going
/// through the slot set, leaves the `I31` and copies nothing.
#[test]
fn a_slot_set_naming_an_edge_that_since_holds_an_i31_skips_it() {
    let (_range, ty) = types(&[PAIR]);
    let mut heap = Heap::new(crate::verified());
    let (old, i31) = heap.mutate(|cx| {
        let old = cx.pretenured(|cx| pair(cx, ty[0], None, None, 1));
        let young = pair(cx, ty[0], None, None, 2);
        let i31 = i31_inside(cx, young);
        // SAFETY: the pair's first edge.
        unsafe {
            raw::write_edge(cx, old, 0, Some(young.into()));
            raw::write_edge(cx, old, 0, Some(i31.into()));
        }
        (Persistent::new(cx, old), i31)
    });
    heap.scavenge();
    assert_eq!(heap.stats().last_scavenge.survived_objects, 0);
    // SAFETY: as above.
    heap.mutate(|cx| assert_eq!(i31_in(unsafe { raw::read_edge(cx, old.get(cx), 0) }), i31));
}

/// While marking, an edge holding an `I31` with bits inside a dead old
/// object is cut: the barrier marks nothing, the dead object isn't kept.
#[test]
fn the_marking_barrier_takes_no_i31() {
    let (_range, ty) = types(&[PAIR]);
    let (mut heap, steps) = stepping_heap(0);
    let a = heap.mutate(|cx| {
        cx.pretenured(|cx| {
            let dead = pair(cx, ty[0], None, None, 7);
            let a = pair(cx, ty[0], None, None, 1);
            // SAFETY: the pair's first edge.
            unsafe { raw::write_edge(cx, a, 0, Some(i31_inside(cx, dead).into())) };
            Persistent::new(cx, a)
        })
    });
    start_marking(&mut heap);
    // SAFETY: as above.
    heap.mutate(|cx| unsafe { raw::write_edge(cx, a.get(cx), 0, None) });
    finish(&mut heap, &steps);
    heap.mutate(|_| {});
    assert_eq!(heap.stats().last_gc.marked_objects, 1);
    drop(a);
}

/// A heap snapshot's walk (`Heap::walk`) finds a runtime type's objects,
/// their sizes (an array's by its length) and their edges to objects, an
/// array's elements' too, unnamed; an `I31` or an empty edge is none.
#[test]
fn a_walk_finds_raw_objects_and_their_edges_to_objects() {
    let (_range, ty) = types(&[PAIR, EDGES]);
    let (pair_ty, edges_ty) = (ty[0], ty[1]);
    for nursery_bytes in [0, 1 << 20] {
        let mut heap = Heap::new(HeapOptions {
            nursery_bytes,
            ..crate::verified()
        });
        let root = heap.mutate(|cx| {
            let leaf = pair(cx, pair_ty, None, None, 2);
            let inner = pair(cx, pair_ty, Some(leaf), None, 1);
            let array = raw::alloc_array(cx, edges_ty, 3).unwrap();
            // SAFETY: the pair's right edge and the array's elements, by
            // their layouts.
            unsafe {
                raw::write_edge(cx, inner, 4, Some(Ref::I31(I31::wrapping_u32(7))));
                raw::write_edge(cx, array, element_at(0), Some(Ref::Object(inner)));
                raw::write_edge(
                    cx,
                    array,
                    element_at(1),
                    Some(Ref::I31(I31::wrapping_u32(9))),
                );
            }
            Persistent::new(cx, array)
        });
        heap.walk(|cx, walk| check_walk(cx, walk, root.get(cx), edges_ty, nursery_bytes));
    }
}

/// `a_walk_finds_raw_objects_and_their_edges_to_objects`'s checks: the
/// array, its pair and that one's leaf.
fn check_walk<'gc>(
    cx: &Mutation<'gc>,
    walk: &elise::Walk<'gc>,
    array: Gc<'gc, Erased>,
    edges_ty: u32,
    nursery_bytes: usize,
) {
    let inner = left_of_array(cx, array);
    let leaf = left(cx, inner).unwrap();
    let found = |object: Gc<'gc, Erased>| {
        walk.objects()
            .find(|found| Gc::ptr_eq(found.get(), object))
            .unwrap_or_else(|| panic!("{nursery_bytes}: not walked"))
    };
    // Its edges' objects, each strong and unnamed.
    let to = |object: Gc<'gc, Erased>| -> Vec<Gc<'gc, Erased>> {
        found(object)
            .edges()
            .map(|edge| {
                assert!(edge.name.is_none());
                assert!(matches!(edge.kind, elise::EdgeKind::Strong));
                edge.to.get()
            })
            .collect()
    };
    let only = |edges: Vec<Gc<'gc, Erased>>, object: Gc<'gc, Erased>| {
        edges.len() == 1 && Gc::ptr_eq(edges[0], object)
    };
    assert_eq!(found(array).type_index(), edges_ty);
    assert_eq!(found(array).type_name(), "raw");
    // Its header, its length's 8 bytes, its three edges, rounded up to 8.
    assert_eq!(found(array).size(), 8 + 8 + 16, "{nursery_bytes}");
    assert_eq!(found(inner).size(), 8 + 16, "{nursery_bytes}");
    assert!(only(to(array), inner), "{nursery_bytes}");
    assert!(only(to(inner), leaf), "{nursery_bytes}");
    assert!(to(leaf).is_empty());
}

/// The object element 0 of an array of `EDGES` holds.
fn left_of_array<'gc>(cx: &Mutation<'gc>, array: Gc<'gc, Erased>) -> Gc<'gc, Erased> {
    // SAFETY: the array's element 0, by its layout.
    object_in(unsafe { raw::read_edge(cx, array, element_at(0)) }).unwrap()
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

/// An engine's frames' roots, as slots of bits, for a GC given them.
fn give(slots: &mut [u32]) -> impl FnMut(&mut dyn FnMut(&mut u32)) + '_ {
    move |visit| {
        for slot in slots.iter_mut() {
            visit(slot);
        }
    }
}

/// A pair through bits (inside frames, where no GC is due here), `left`
/// and `right` its edges' bits.
fn pair_bits(heap: &mut Heap, ty: u32, left: u32, right: u32, value: u64) -> u32 {
    let object = raw::alloc_bits(heap, ty).expect("no GC due");
    // SAFETY: the pair just allocated, its edges; `left` and `right` the
    // caller's, live (the frames hold them).
    unsafe {
        raw::write_bits_in(heap, object, 0, left);
        raw::write_bits_in(heap, object, 4, right);
    }
    heap.mutate(|cx| {
        // SAFETY: as above; its `u64`.
        unsafe {
            raw::data(object_of(cx, object))
                .as_ptr()
                .add(8)
                .cast::<u64>()
                .write(value);
        }
    });
    object
}

/// The object a frame's `bits` hold, in an operation.
fn object_of<'gc>(cx: &Mutation<'gc>, bits: u32) -> Gc<'gc, Erased> {
    // SAFETY: the test's frames hold `bits`, of a live object (a GC given
    // them kept it), and no GC runs before `'gc` ends.
    object_in(unsafe { raw::from_bits(cx, bits) }).expect("an object's bits")
}

/// The `u64` of the pair a frame's `bits` hold.
fn value_of(heap: &mut Heap, bits: u32) -> u64 {
    heap.mutate(|cx| value(object_of(cx, bits)))
}

/// The bits of the edge at `at` of the pair a frame's `bits` hold.
fn edge_of(heap: &Heap, bits: u32, at: u32) -> u32 {
    // SAFETY: as `object_of`; the pair's edge.
    unsafe { raw::read_bits_in(heap, bits, at) }
}

fn i31_bits(heap: &mut Heap, value: u32) -> u32 {
    heap.mutate(|cx| raw::bits(cx, Some(I31::wrapping_u32(value).into())))
}

/// The length of a `WORDS` array of 4 KiB: what fills a nursery or the old
/// generation in a few hundred allocations (Miri takes half an hour for tens
/// of thousands).
const FILLER_WORDS: u32 = 511;

/// Inside an engine's frames, which hold roots the heap can't see, no
/// safepoint starts a GC: the nursery full, objects go to the old
/// generation, which grows past its limit; once the frames are left, the
/// next does.
#[test]
fn inside_frames_a_safepoint_starts_no_gc() {
    let (_range, ty) = types(&[WORDS]);
    let mut heap = Heap::new(HeapOptions {
        nursery_bytes: 256 << 10,
        min_allocation_bytes: 64 << 10,
        ..crate::verified()
    });
    heap.enter_frames();
    heap.enter_frames();
    heap.leave_frames();
    assert!(heap.in_frames());
    // 1 MiB, in 256 objects.
    for _ in 0..16 {
        heap.mutate(|cx| {
            for _ in 0..16 {
                raw::alloc_array(cx, ty[0], FILLER_WORDS).unwrap();
            }
        });
    }
    let stats = heap.stats();
    assert_eq!((stats.scavenges, stats.gcs), (0, 0));
    assert!(!heap.is_marking());
    assert!(stats.used_bytes > 128 << 10, "{}", stats.used_bytes);
    assert!(heap.should_collect());
    heap.leave_frames();
    assert!(!heap.in_frames());
    heap.mutate(|_| {});
    let stats = heap.stats();
    assert!(stats.scavenges + stats.gcs > 0 || heap.is_marking());
}

/// A scavenge given the frames' roots keeps what they reach, moved, and
/// writes their new bits back; none and an `I31` stay as they are.
#[test]
fn a_scavenge_given_roots_moves_what_they_hold() {
    let (_range, ty) = types(&[PAIR]);
    let mut heap = Heap::new(crate::verified());
    heap.enter_frames();
    let i31 = i31_bits(&mut heap, 7);
    let b = pair_bits(&mut heap, ty[0], i31, 0, 2);
    let a = pair_bits(&mut heap, ty[0], b, 0, 1);
    pair_bits(&mut heap, ty[0], 0, 0, 3);
    let mut slots = [a, 0, i31];
    // SAFETY: the slots hold an object of the heap, none and an `I31`.
    unsafe { heap.scavenge_with_roots(give(&mut slots)) };
    assert_eq!(heap.stats().last_scavenge.survived_objects, 2);
    assert_ne!(slots[0], a, "moved");
    assert_eq!(slots[1..], [0, i31]);
    assert_eq!(value_of(&mut heap, slots[0]), 1);
    let b = edge_of(&heap, slots[0], 0);
    assert_eq!(value_of(&mut heap, b), 2);
    assert_eq!(edge_of(&heap, b, 0), i31);
    heap.mutate(|cx| {
        // SAFETY: none, and an `I31`'s bits.
        unsafe {
            assert!(raw::from_bits(cx, 0).is_none());
            assert_eq!(i31_in(raw::from_bits(cx, i31)).get_u32(), 7);
        }
    });
}

/// A full GC given the frames' roots marks from them: what they reach
/// lives (promoted, its bits written back), the rest dies.
#[test]
fn a_full_gc_given_roots_keeps_what_they_reach() {
    let (_range, ty) = types(&[PAIR]);
    let mut heap = Heap::new(crate::verified());
    heap.enter_frames();
    let b = pair_bits(&mut heap, ty[0], 0, 0, 2);
    let a = pair_bits(&mut heap, ty[0], 0, b, 1);
    pair_bits(&mut heap, ty[0], a, 0, 3);
    let mut slots = [0, a];
    // SAFETY: the slots hold none and an object of the heap.
    unsafe { heap.collect_with_roots(give(&mut slots)) };
    assert_eq!(heap.stats().last_gc.marked_objects, 2);
    assert_ne!(slots[1], a, "promoted");
    assert_eq!(value_of(&mut heap, slots[1]), 1);
    let b = edge_of(&heap, slots[1], 4);
    assert_eq!(value_of(&mut heap, b), 2);
}

/// A concurrent marking starts and finishes only where the frames' roots
/// are given: the snapshot holds what they reach, and what they take from
/// an edge then cut while it runs, SATB's barrier keeps; what they let go
/// meanwhile floats (the verifier knows it from the snapshot's roots).
#[test]
fn a_marking_inside_frames_starts_and_ends_given_roots() {
    let (_range, ty) = types(&[PAIR]);
    let (mut heap, steps) = stepping_heap(0);
    heap.enter_frames();
    let z = pair_bits(&mut heap, ty[0], 0, 0, 26);
    let x = pair_bits(&mut heap, ty[0], z, 0, 24);
    let mut slots = vec![x];
    for _ in 0..1000 {
        if heap.should_collect() {
            break;
        }
        heap.mutate(|cx| {
            for _ in 0..100 {
                pair(cx, ty[0], None, None, 0);
            }
        });
    }
    assert!(!heap.is_marking(), "none starts inside frames");
    // SAFETY: the slots hold an object of the heap.
    unsafe { heap.safepoint_with_roots(give(&mut slots)) };
    assert!(heap.is_marking());
    // The frames take `z` from `x`'s edge, which they then cut.
    slots.push(edge_of(&heap, slots[0], 0));
    // SAFETY: `x`'s edge, set to none.
    unsafe { raw::write_bits_in(&mut heap, slots[0], 0, 0) };
    // The barrier's `z` published, then marked, then the marking found
    // done.
    heap.mutate(|_| {});
    steps.step();
    heap.mutate(|_| {});
    assert!(heap.is_marking(), "none ends inside frames");
    assert!(heap.should_collect());
    // The frames let `x` go.
    slots.remove(0);
    // SAFETY: the slot holds `z`.
    unsafe { heap.safepoint_with_roots(give(&mut slots)) };
    assert!(!heap.is_marking());
    assert!(heap.stats().last_gc.concurrent);
    assert_eq!(value_of(&mut heap, slots[0]), 26);
}

/// An allocation through bits never GCs: it fails as a GC is due, which a
/// safepoint inside the frames leaves be, and once one given the roots
/// ran, the next allocation doesn't wait for another.
#[test]
fn an_allocation_through_bits_fails_as_a_gc_is_due() {
    let (_range, ty) = types(&[WORDS]);
    let mut heap = Heap::new(HeapOptions {
        nursery_bytes: 256 << 10,
        ..crate::verified()
    });
    heap.enter_frames();
    let mut slots = Vec::new();
    loop {
        match raw::alloc_array_bits(&mut heap, ty[0], FILLER_WORDS) {
            Ok(bits) => slots.push(bits),
            Err(error) => {
                assert_eq!(error, raw::AllocError::GcDue);
                break;
            }
        }
        assert!(slots.len() < 1 << 20, "no GC due");
    }
    assert_eq!(heap.stats().scavenges, 0);
    heap.mutate(|_| {});
    assert_eq!(
        raw::alloc_array_bits(&mut heap, ty[0], FILLER_WORDS),
        Err(raw::AllocError::GcDue)
    );
    // SAFETY: the slots hold objects of the heap.
    unsafe { heap.safepoint_with_roots(give(&mut slots)) };
    let record = heap.stats().last_scavenge;
    assert_eq!(heap.stats().scavenges, 1);
    assert_eq!(
        record.survived_objects + record.promoted_objects,
        slots.len() - 1,
        "all but the last, old"
    );
    raw::alloc_array_bits(&mut heap, ty[0], FILLER_WORDS).unwrap();
}

/// Under `stress`, where a GC is due at every safepoint, an allocation
/// through bits succeeds after one given the roots, then not; an array too
/// large says so first.
#[test]
fn an_allocation_through_bits_after_a_gc_doesnt_wait_for_another() {
    let (_range, ty) = types(&[PAIR, EDGES]);
    let mut heap = Heap::new(HeapOptions {
        stress: true,
        ..crate::verified()
    });
    heap.enter_frames();
    assert_eq!(
        raw::alloc_bits(&mut heap, ty[0]),
        Err(raw::AllocError::GcDue)
    );
    assert_eq!(
        raw::alloc_array_bits(&mut heap, ty[1], u32::MAX),
        Err(raw::AllocError::TooLarge)
    );
    // SAFETY: no roots.
    unsafe { heap.safepoint_with_roots(give(&mut [])) };
    let mut slots = [raw::alloc_array_bits(&mut heap, ty[1], 4).unwrap()];
    assert_eq!(
        raw::alloc_bits(&mut heap, ty[0]),
        Err(raw::AllocError::GcDue)
    );
    // SAFETY: the slot holds an object of the heap.
    unsafe { heap.safepoint_with_roots(give(&mut slots)) };
    raw::alloc_bits(&mut heap, ty[0]).unwrap();
    heap.mutate(|cx| {
        // SAFETY: the array's length, its first `u32`.
        let length = unsafe { raw::data(object_of(cx, slots[0])).cast::<u32>().read() };
        assert_eq!(length, 4);
    });
}

/// The write barrier through bits: an old object's edge set to a young
/// one is in its slot set, so a scavenge given only the old one keeps it.
#[test]
fn the_write_barrier_through_bits_records_an_old_objects_edge() {
    let (_range, ty) = types(&[PAIR]);
    let mut heap = Heap::new(crate::verified());
    heap.enter_frames();
    let mut slots = [pair_bits(&mut heap, ty[0], 0, 0, 1)];
    // SAFETY: the slot holds an object of the heap.
    unsafe { heap.collect_with_roots(give(&mut slots)) };
    let young = pair_bits(&mut heap, ty[0], 0, 0, 2);
    // SAFETY: the old pair's edge, set to the young one.
    unsafe { raw::write_bits_in(&mut heap, slots[0], 0, young) };
    // SAFETY: as above.
    unsafe { heap.scavenge_with_roots(give(&mut slots)) };
    assert_eq!(heap.stats().last_scavenge.survived_objects, 1);
    let young = edge_of(&heap, slots[0], 0);
    assert_eq!(value_of(&mut heap, young), 2);
}

#[test]
#[should_panic = "an engine's frames hold roots"]
fn a_full_gc_inside_frames_needs_their_roots() {
    let mut heap = Heap::new(crate::verified());
    heap.enter_frames();
    heap.collect();
}

#[test]
#[should_panic = "an engine's frames hold roots"]
fn a_scavenge_inside_frames_needs_their_roots() {
    let mut heap = Heap::new(crate::verified());
    heap.enter_frames();
    heap.scavenge();
}

#[test]
#[should_panic = "frames entered to leave"]
fn frames_left_were_entered() {
    Heap::new(crate::verified()).leave_frames();
}

/// The roots given again are those given first: more panics, as fewer.
#[test]
fn the_roots_given_again_are_as_many() {
    for (first, again) in [(1, 2), (2, 1)] {
        let result = std::panic::catch_unwind(|| {
            let mut heap = Heap::new(crate::verified());
            let mut slots = [0u32; 2];
            let mut calls = 0;
            // SAFETY: the slots hold none.
            unsafe {
                heap.scavenge_with_roots(|visit: &mut dyn FnMut(&mut u32)| {
                    calls += 1;
                    let count = if calls == 1 { first } else { again };
                    for slot in &mut slots[..count] {
                        visit(slot);
                    }
                });
            }
        });
        let payload = result.expect_err("a panic");
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied());
        assert_eq!(
            message,
            Some("the roots given again are as many as first"),
            "{first} then {again}"
        );
    }
}

/// An embedder's roots outside the heap's tables (`Heap::set_roots_source`):
/// slots of bits, whether their holder is busy, and how often the heap asked.
#[derive(Default)]
struct Held {
    slots: RefCell<Vec<u32>>,
    busy: Cell<bool>,
    asked: Cell<usize>,
}

struct Source(Rc<Held>);

// SAFETY: the tests' slots hold none, `I31`s and objects of the heap, kept
// live by the GCs that read them; each call gives them all, in order.
unsafe impl RootsSource for Source {
    fn roots(&mut self, visit: &mut dyn FnMut(&mut u32)) -> bool {
        self.0.asked.set(self.0.asked.get() + 1);
        if self.0.busy.get() {
            return false;
        }
        for slot in self.0.slots.borrow_mut().iter_mut() {
            visit(slot);
        }
        true
    }
}

fn held(heap: &mut Heap, slots: Vec<u32>) -> Rc<Held> {
    let held = Rc::new(Held {
        slots: RefCell::new(slots),
        ..Held::default()
    });
    heap.set_roots_source(Some(Box::new(Source(held.clone()))));
    held
}

/// `a` → `b`, the bits of `a` and an `I31`, garbage beside them.
fn pairs_bits(heap: &mut Heap, ty: u32) -> (u32, u32) {
    heap.mutate(|cx| {
        let b = pair(cx, ty, None, None, 2);
        let a = pair(cx, ty, Some(b), None, 1);
        pair(cx, ty, Some(a), None, 3);
        let i31 = raw::bits(cx, Some(I31::wrapping_u32(7).into()));
        (raw::bits(cx, Some(Ref::from(a))), i31)
    })
}

/// `a`'s value and its edge's object's, `a` the bits `held`'s first slot
/// holds now (read after the safepoint, in the operation).
fn held_values(heap: &mut Heap, held: &Held) -> (u64, u64) {
    heap.read(|cx| {
        let a = object_of(cx, held.slots.borrow()[0]);
        (value(a), value(left(cx, a).expect("a's edge")))
    })
}

/// A roots source's roots are read by every GC a safepoint runs: through
/// scavenges and a concurrent marking's start and end (under `stress`,
/// one at each safepoint), what they hold lives, moved, their bits written
/// back; none and an `I31` stay as they are.
#[test]
fn a_roots_sources_roots_live_through_a_safepoints_gcs() {
    let (_range, ty) = types(&[PAIR]);
    let (mut heap, _steps) = stepping_heap_with(HeapOptions {
        stress: true,
        ..crate::verified()
    });
    let (a, i31) = pairs_bits(&mut heap, ty[0]);
    let held = held(&mut heap, vec![a, 0, i31]);
    for _ in 0..6 {
        heap.mutate(|cx| {
            pair(cx, ty[0], None, None, 0);
        });
    }
    let stats = heap.stats();
    assert!(stats.scavenges >= 3, "{}", stats.scavenges);
    assert!(stats.last_gc.concurrent, "a marking ended");
    assert_eq!(stats.markings_at_limit, 0, "stress's, ended undone");
    assert_eq!(held.asked.get(), 12, "read, then written back, each GC");
    let slots = held.slots.borrow().clone();
    assert_ne!(slots[0], a, "moved");
    assert_eq!(slots[1..], [0, i31]);
    assert_eq!(held_values(&mut heap, &held), (1, 2));
    // `a` → `b` and the garbage pair beside, which died.
    assert_eq!(stats.last_gc.marked_objects, 2);
}

/// `collect` and `scavenge` read a roots source's roots too; without it
/// (`None`), what it held dies.
#[test]
fn a_full_gc_and_a_scavenge_read_a_roots_sources_roots() {
    let (_range, ty) = types(&[PAIR]);
    let mut heap = Heap::new(crate::verified());
    let (a, _) = pairs_bits(&mut heap, ty[0]);
    let held = held(&mut heap, vec![a]);
    heap.scavenge();
    assert_eq!(heap.stats().last_scavenge.survived_objects, 2);
    assert_ne!(held.slots.borrow()[0], a, "moved");
    heap.collect();
    assert_eq!(heap.stats().last_gc.marked_objects, 2);
    assert_eq!(held_values(&mut heap, &held), (1, 2));
    assert_eq!(held.asked.get(), 4);
    heap.set_roots_source(None);
    heap.collect();
    assert_eq!(heap.stats().last_gc.marked_objects, 0);
    assert_eq!(held.asked.get(), 4, "not asked");
}

/// While a roots source can't give its roots (its holder busy), no
/// safepoint runs a GC that reads them: the nursery full, objects go to
/// the old generation, past its limit; `collect` and `scavenge` run none.
/// Once it can, the next safepoint does, its roots kept.
#[test]
fn a_busy_roots_source_defers_the_gcs() {
    let (_range, ty) = types(&[PAIR, WORDS]);
    let mut heap = Heap::new(HeapOptions {
        nursery_bytes: 256 << 10,
        min_allocation_bytes: 64 << 10,
        ..crate::verified()
    });
    let (a, _) = pairs_bits(&mut heap, ty[0]);
    let held = held(&mut heap, vec![a]);
    held.busy.set(true);
    // 1 MiB, in 256 objects.
    for _ in 0..16 {
        heap.mutate(|cx| {
            for _ in 0..16 {
                raw::alloc_array(cx, ty[1], FILLER_WORDS).unwrap();
            }
        });
    }
    heap.collect();
    heap.scavenge();
    let stats = heap.stats();
    assert_eq!((stats.scavenges, stats.gcs), (0, 0));
    assert!(!heap.is_marking());
    assert!(stats.used_bytes > 128 << 10, "{}", stats.used_bytes);
    assert!(heap.should_collect());
    assert!(held.asked.get() > 2, "asked at each safepoint a GC was due");
    assert_eq!(held.slots.borrow()[0], a);
    held.busy.set(false);
    heap.mutate(|_| {});
    let stats = heap.stats();
    assert!(stats.scavenges + stats.gcs > 0 || heap.is_marking());
    assert_eq!(held_values(&mut heap, &held), (1, 2));
}

/// A GC given an engine's frames' roots reads a roots source's after
/// them, and writes both back.
#[test]
fn a_gc_given_roots_reads_a_roots_sources_too() {
    let (_range, ty) = types(&[PAIR]);
    let mut heap = Heap::new(crate::verified());
    heap.enter_frames();
    let mut slots = [pair_bits(&mut heap, ty[0], 0, 0, 4)];
    let (a, _) = pairs_bits(&mut heap, ty[0]);
    let held = held(&mut heap, vec![a]);
    let framed = slots[0];
    // SAFETY: the slot holds an object of the heap.
    unsafe { heap.scavenge_with_roots(give(&mut slots)) };
    assert_eq!(heap.stats().last_scavenge.survived_objects, 3);
    assert_ne!(slots[0], framed);
    assert_ne!(held.slots.borrow()[0], a);
    // SAFETY: as above.
    unsafe { heap.collect_with_roots(give(&mut slots)) };
    assert_eq!(heap.stats().last_gc.marked_objects, 3);
    assert_eq!(value_of(&mut heap, slots[0]), 4);
    assert_eq!(held_values(&mut heap, &held), (1, 2));
}

/// With a busy roots source, a GC given the frames' roots runs none: the
/// allocation through bits that found it due goes on (as after one).
#[test]
fn a_busy_roots_source_lets_an_allocation_through_bits_go_on() {
    let (_range, ty) = types(&[PAIR]);
    let mut heap = Heap::new(HeapOptions {
        stress: true,
        ..crate::verified()
    });
    let held = held(&mut heap, Vec::new());
    held.busy.set(true);
    heap.enter_frames();
    assert_eq!(
        raw::alloc_bits(&mut heap, ty[0]),
        Err(raw::AllocError::GcDue)
    );
    // SAFETY: no roots.
    unsafe { heap.safepoint_with_roots(give(&mut [])) };
    assert_eq!((heap.stats().scavenges, heap.stats().gcs), (0, 0));
    raw::alloc_bits(&mut heap, ty[0]).unwrap();
}

#[test]
#[should_panic = "a roots source gave its roots, and then not again"]
fn a_roots_source_gives_its_roots_again() {
    struct Once(bool);
    // SAFETY: no roots.
    unsafe impl RootsSource for Once {
        fn roots(&mut self, _: &mut dyn FnMut(&mut u32)) -> bool {
            !std::mem::replace(&mut self.0, true)
        }
    }
    let mut heap = Heap::new(crate::verified());
    heap.set_roots_source(Some(Box::new(Once(false))));
    heap.scavenge();
}

#[test]
#[should_panic = "the roots given again are as many as first"]
fn a_roots_sources_roots_given_again_are_as_many() {
    struct Fewer(usize);
    // SAFETY: its slots hold none.
    unsafe impl RootsSource for Fewer {
        fn roots(&mut self, visit: &mut dyn FnMut(&mut u32)) -> bool {
            self.0 += 1;
            let mut slots = [0u32; 2];
            for slot in &mut slots[..3 - self.0] {
                visit(slot);
            }
            true
        }
    }
    let mut heap = Heap::new(crate::verified());
    heap.set_roots_source(Some(Box::new(Fewer(0))));
    heap.scavenge();
}
