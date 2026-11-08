use std::cell::Cell;
use std::marker::PhantomData;
use std::mem::{offset_of, size_of};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::member::compress;
use crate::types::TypeInfo;
use crate::{FieldInfo, FieldKind, Gc, HeapType, Member, Mutation, TypeSlot};

/// A growable list of edges (cppgc's `HeapVector<Member<T>>`), itself an
/// object of the heap: its length and an edge to a `MemberArray` of its
/// capacity. Growing allocates a new array and copies the edges.
pub struct HeapVector<T: HeapType> {
    len: Cell<u32>,
    store: Member<MemberArray>,
    _target: PhantomData<fn() -> T>,
}

// SAFETY: its one `Member` is `store`; no drop glue. Its layout is the
// same for every `T`, so one slot serves them all.
unsafe impl<T: HeapType> HeapType for HeapVector<T> {
    fn __type() -> &'static TypeSlot {
        static TYPE: TypeSlot = TypeSlot::new(&TypeInfo::of::<HeapVector<MemberArray>>(
            "HeapVector",
            &[
                FieldInfo {
                    offset: offset_of!(HeapVector<MemberArray>, len),
                    kind: FieldKind::Data,
                    main_thread_drop: false,
                    name: c"len",
                },
                FieldInfo {
                    offset: offset_of!(HeapVector<MemberArray>, store),
                    kind: FieldKind::Member,
                    main_thread_drop: false,
                    name: c"store",
                },
            ],
        ));
        &TYPE
    }
}

impl<T: HeapType> HeapVector<T> {
    pub fn new<'gc>(cx: &Mutation<'gc>, capacity: usize) -> Gc<'gc, Self> {
        Self::new_in(cx, capacity, false)
    }

    /// `new`, in `owner`'s generation (`Mutation::alloc_beside`).
    pub fn new_beside<'gc, U>(
        cx: &Mutation<'gc>,
        owner: Gc<'gc, U>,
        capacity: usize,
    ) -> Gc<'gc, Self> {
        Self::new_in(cx, capacity, cx.is_old(owner))
    }

    /// `new`, in the old generation if `old`.
    fn new_in<'gc>(cx: &Mutation<'gc>, capacity: usize, old: bool) -> Gc<'gc, Self> {
        let store = MemberArray::new(cx, capacity, old);
        let index = Self::__type().index();
        let data = cx.inner.allocate(size_of::<Self>(), 0, old);
        let ptr = cx.inner.ptr(data).cast::<Self>();
        let raw = compress(cx, Some(store));
        let vector = Self {
            len: Cell::new(0),
            store: Member::new_in_heap(raw),
            _target: PhantomData,
        };
        // SAFETY: the object just allocated, not initialized yet.
        unsafe { ptr.write(vector) };
        // The vector may be old (the nursery filled up after its store).
        // SAFETY: the vector just written.
        let edge = unsafe { &raw const (*ptr).store };
        cx.inner.write_barrier(edge.cast(), raw);
        cx.inner
            .publish(data, index, 0, crate::types::Finalizer::None);
        // SAFETY: just initialized.
        unsafe { Gc::from_raw(NonNull::new_unchecked(ptr)) }
    }

    pub fn len(&self) -> usize {
        self.len.get() as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get<'gc>(&self, cx: &Mutation<'gc>, index: usize) -> Option<Gc<'gc, T>> {
        if index >= self.len() {
            return None;
        }
        let raw = self.store(cx).edge(index).load(Ordering::Relaxed);
        // SAFETY: an edge below the length is a live `T` (set only from a
        // `Gc<'gc, T>`, and marked with the vector).
        Some(unsafe { Gc::from_raw(NonNull::new_unchecked(cx.inner.ptr(raw as usize).cast())) })
    }

    /// # Panics
    ///
    /// If `index` is past the end.
    pub fn set<'gc>(&self, cx: &Mutation<'gc>, index: usize, value: Gc<'gc, T>) {
        assert!(index < self.len(), "index {index} past {}", self.len());
        self.store(cx).write(cx, index, compress(cx, Some(value)));
    }

    pub fn push<'gc>(&self, cx: &Mutation<'gc>, value: Gc<'gc, T>) {
        let len = self.len();
        let mut store = self.store(cx);
        if len == store.capacity() {
            // The store follows the vector: an old vector's young store
            // would only be copied till promoted.
            let old = !cx
                .inner
                .is_young(cx.inner.offset_of((&raw const *self).cast()));
            let grown = MemberArray::new(cx, len.saturating_mul(2).max(4), old);
            for i in 0..len {
                grown.write(cx, i, store.edge(i).load(Ordering::Relaxed));
            }
            self.store.set(cx, Some(grown));
            store = grown;
        }
        store.write(cx, len, compress(cx, Some(value)));
        // `edge` checked len < capacity, which fits 32 bits.
        self.len.set(self.len.get().saturating_add(1));
    }

    /// Empties it, keeping its capacity (a list refilled again and again
    /// allocates once): its edges nulled, so they keep nothing alive.
    pub fn clear(&self, cx: &Mutation<'_>) {
        let store = self.store(cx);
        for i in 0..self.len() {
            store.write(cx, i, 0);
        }
        self.len.set(0);
    }

    /// # Panics
    ///
    /// If `index` is past the end.
    pub fn remove<'gc>(&self, cx: &Mutation<'gc>, index: usize) -> Gc<'gc, T> {
        let removed = self.get(cx, index).expect("index past the end");
        let store = self.store(cx);
        // `get` found index < len, so len >= 1.
        let last = self.len().saturating_sub(1);
        for i in index..last {
            store.write(
                cx,
                i,
                store.edge(i.saturating_add(1)).load(Ordering::Relaxed),
            );
        }
        // Past the length, edges are null: they keep nothing alive.
        store.write(cx, last, 0);
        self.len.set(self.len.get().saturating_sub(1));
        removed
    }

    fn store<'gc>(&self, cx: &Mutation<'gc>) -> Gc<'gc, MemberArray> {
        self.store.get(cx).expect("a vector has a store")
    }
}

/// The edges of a `HeapVector`: a capacity, then that many 32-bit offsets
/// (0 for none). Its bytes are read only through raw pointers from the
/// cage, never through a `&MemberArray`, which would cover only its first
/// four.
#[repr(C)]
pub(crate) struct MemberArray {
    capacity: u32,
}

static MEMBER_ARRAY: TypeInfo = TypeInfo::member_array("MemberArray");

// SAFETY: the GC walks its edges by its kind.
unsafe impl HeapType for MemberArray {
    fn __type() -> &'static TypeSlot {
        static TYPE: TypeSlot = TypeSlot::new(&MEMBER_ARRAY);
        &TYPE
    }
}

impl MemberArray {
    /// An array of `capacity` empty edges, in the old generation if `old`.
    fn new<'gc>(cx: &Mutation<'gc>, capacity: usize, old: bool) -> Gc<'gc, Self> {
        let bytes = capacity
            .checked_add(1)
            .and_then(|words| words.checked_mul(4))
            .expect("an array's size fits");
        let capacity = u32::try_from(capacity).expect("a capacity fits 32 bits");
        let index = Self::__type().index();
        let data = cx.inner.allocate(bytes, 0, old);
        let ptr = cx.inner.ptr(data).cast::<u32>();
        // SAFETY: the capacity and edges just allocated; the slot may hold
        // an old object's bytes, so every edge is cleared.
        unsafe {
            ptr.write(capacity);
            for i in 1..=capacity as usize {
                ptr.add(i).write(0);
            }
        }
        cx.inner
            .publish(data, index, 0, crate::types::Finalizer::None);
        // SAFETY: just initialized.
        unsafe { Gc::from_raw(NonNull::new_unchecked(ptr.cast())) }
    }
}

impl<'gc> Gc<'gc, MemberArray> {
    fn capacity(self) -> usize {
        // SAFETY: an array's first four bytes are its capacity.
        unsafe { self.as_ptr().cast::<u32>().read() as usize }
    }

    /// Its edge at `index`, which must be below the capacity.
    fn edge(self, index: usize) -> &'gc AtomicU32 {
        assert!(index < self.capacity());
        // SAFETY: inside the array (index + 1 <= capacity), 4-aligned,
        // through the cage's pointer; read and written as atomics (and,
        // in a scavenge, plainly by its threads, while nothing else runs).
        unsafe { AtomicU32::from_ptr(self.as_ptr().cast::<u32>().add(1).add(index)) }
    }

    /// Writes edge `index`, with the write barrier (the array may be old,
    /// `raw` young).
    fn write(self, cx: &Mutation<'gc>, index: usize, raw: u32) {
        cx.inner.write_edge(self.edge(index), raw);
    }
}
