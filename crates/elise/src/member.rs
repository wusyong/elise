use std::fmt;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::{FieldKind, Gc, HeapField, HeapType, Mutation};

/// An edge from one object to another (cppgc's `Member`): a 32-bit offset
/// into the heap's cage, 0 for none. It exists only in the heap: it has no
/// constructor, no `Clone`, no `Default`, and a struct holding one can't be
/// built but by `alloc`, so it can't be moved out of an object or into one
/// past the write barrier, once there is one. The offset is atomic: the
/// threads that mark read it while the mutator writes it (std's atomic,
/// four bytes, even under loom and shuttle).
#[repr(transparent)]
pub struct Member<T: HeapType> {
    raw: AtomicU32,
    _target: PhantomData<fn() -> T>,
}

impl<T: HeapType> Member<T> {
    /// One to be written straight into an object being initialized.
    pub(crate) fn new_in_heap(raw: u32) -> Self {
        Self {
            raw: AtomicU32::new(raw),
            _target: PhantomData,
        }
    }

    pub fn get<'gc>(&self, cx: &Mutation<'gc>) -> Option<Gc<'gc, T>> {
        let raw = self.checked(cx).load(Ordering::Relaxed);
        // SAFETY: a non-null edge of an object of `cx`'s heap is a live `T`
        // (set only from a `Gc<'gc, T>`, and what it points to is marked
        // with it).
        (raw != 0).then(|| unsafe {
            Gc::from_raw(NonNull::new_unchecked(cx.inner.ptr(raw as usize).cast()))
        })
    }

    /// Sets the edge, with the generational write barrier: an old object's
    /// edge to a young one is recorded. No GC runs during an operation, and
    /// none marks alongside one yet, so there is no marking barrier.
    pub fn set<'gc>(&self, cx: &Mutation<'gc>, value: Option<Gc<'gc, T>>) {
        let raw = compress(cx, value);
        cx.inner.write_edge(self.checked(cx), raw);
    }

    pub fn is_null(&self) -> bool {
        self.raw.load(Ordering::Relaxed) == 0
    }

    /// Its offset, which means something only in `cx`'s heap.
    ///
    /// # Panics
    ///
    /// If it is in another heap.
    fn checked(&self, cx: &Mutation<'_>) -> &AtomicU32 {
        assert!(
            cx.inner.cage.contains((&raw const self.raw).cast()),
            "a Member of another heap"
        );
        &self.raw
    }
}

/// A `Gc`'s offset in `cx`'s cage, 0 for none.
pub(crate) fn compress<T>(cx: &Mutation<'_>, value: Option<Gc<'_, T>>) -> u32 {
    value.map_or(0, |gc| {
        let ptr = gc.as_ptr().cast::<u8>();
        assert!(cx.inner.cage.contains(ptr), "a Gc of another heap");
        #[expect(
            clippy::cast_possible_truncation,
            reason = "in the cage (asserted above), so below CAGE_BYTES = 2^32"
        )]
        let offset = cx.inner.offset_of(ptr) as u32;
        offset
    })
}

// SAFETY: its bytes are the offset, and `__write` writes one.
unsafe impl<T: HeapType> HeapField for Member<T> {
    type Init<'gc> = Option<Gc<'gc, T>>;
    const KIND: FieldKind = FieldKind::Member;

    unsafe fn __write<'gc>(init: Self::Init<'gc>, at: *mut Self, cx: &Mutation<'gc>) {
        let raw = compress(cx, init);
        // SAFETY: as the caller promises.
        unsafe { at.write(Self::new_in_heap(raw)) };
        // The object may be old (the nursery was full, or it is large).
        cx.inner.write_barrier(at.cast(), raw);
    }
}

// SAFETY: its bytes are `N` offsets in a row (a `Member` is one), and
// `__write` writes each.
unsafe impl<T: HeapType, const N: usize> HeapField for [Member<T>; N] {
    type Init<'gc> = [Option<Gc<'gc, T>>; N];
    const KIND: FieldKind = {
        assert!(N <= u32::MAX as usize, "fewer than 2^32 edges in a row");
        #[expect(
            clippy::cast_possible_truncation,
            reason = "asserted above: N fits 32 bits"
        )]
        FieldKind::Members(N as u32)
    };

    unsafe fn __write<'gc>(init: Self::Init<'gc>, at: *mut Self, cx: &Mutation<'gc>) {
        let raw = init.map(|edge| compress(cx, edge));
        // SAFETY: as the caller promises.
        unsafe { at.write(raw.map(Member::new_in_heap)) };
        for (i, raw) in raw.into_iter().enumerate() {
            // The object may be old, as `Member`'s.
            cx.inner
                .write_barrier(at.cast::<u32>().wrapping_add(i).cast(), raw);
        }
    }
}

/// A weak edge (cppgc's `WeakMember`): a `Member` that doesn't keep its
/// object alive. Once the object dies, a GC clears it: a full GC if the
/// object is old, a scavenge if it is young. Read while a concurrent
/// marking runs, its object is marked (it is the mutator's now, and the
/// snapshot may no longer reach it).
#[repr(transparent)]
pub struct WeakMember<T: HeapType> {
    raw: AtomicU32,
    _target: PhantomData<fn() -> T>,
}

impl<T: HeapType> WeakMember<T> {
    fn new_in_heap(raw: u32) -> Self {
        Self {
            raw: AtomicU32::new(raw),
            _target: PhantomData,
        }
    }

    /// Its object, if it is alive.
    pub fn get<'gc>(&self, cx: &Mutation<'gc>) -> Option<Gc<'gc, T>> {
        let raw = self.checked(cx).load(Ordering::Relaxed);
        cx.inner.read_weak(raw);
        // SAFETY: a non-null weak edge of an object of `cx`'s heap is a
        // live `T` (set only from a `Gc<'gc, T>`, and cleared once it dies;
        // marked just now if a marking runs).
        (raw != 0).then(|| unsafe {
            Gc::from_raw(NonNull::new_unchecked(cx.inner.ptr(raw as usize).cast()))
        })
    }

    /// Sets the edge, with the generational write barrier for weak edges:
    /// an old object's to a young one is recorded, apart from the strong.
    pub fn set<'gc>(&self, cx: &Mutation<'gc>, value: Option<Gc<'gc, T>>) {
        let raw = compress(cx, value);
        cx.inner.write_weak(self.checked(cx), raw);
    }

    /// # Panics
    ///
    /// If it is in another heap.
    fn checked(&self, cx: &Mutation<'_>) -> &AtomicU32 {
        assert!(
            cx.inner.cage.contains((&raw const self.raw).cast()),
            "a WeakMember of another heap"
        );
        &self.raw
    }
}

// SAFETY: its bytes are the offset, and `__write` writes one.
unsafe impl<T: HeapType> HeapField for WeakMember<T> {
    type Init<'gc> = Option<Gc<'gc, T>>;
    const KIND: FieldKind = FieldKind::Weak;

    unsafe fn __write<'gc>(init: Self::Init<'gc>, at: *mut Self, cx: &Mutation<'gc>) {
        let raw = compress(cx, init);
        // SAFETY: as the caller promises.
        unsafe { at.write(Self::new_in_heap(raw)) };
        // The object may be old, as `Member`'s.
        cx.inner.weak_barrier(at.cast(), raw);
    }
}

// SAFETY: its bytes are `N` offsets in a row, and `__write` writes each.
unsafe impl<T: HeapType, const N: usize> HeapField for [WeakMember<T>; N] {
    type Init<'gc> = [Option<Gc<'gc, T>>; N];
    const KIND: FieldKind = {
        assert!(N <= u32::MAX as usize, "fewer than 2^32 edges in a row");
        #[expect(
            clippy::cast_possible_truncation,
            reason = "asserted above: N fits 32 bits"
        )]
        FieldKind::Weaks(N as u32)
    };

    unsafe fn __write<'gc>(init: Self::Init<'gc>, at: *mut Self, cx: &Mutation<'gc>) {
        let raw = init.map(|edge| compress(cx, edge));
        // SAFETY: as the caller promises.
        unsafe { at.write(raw.map(WeakMember::new_in_heap)) };
        for (i, raw) in raw.into_iter().enumerate() {
            cx.inner
                .weak_barrier(at.cast::<u32>().wrapping_add(i).cast(), raw);
        }
    }
}

/// Any object of the heap, whatever its type: what an edge to objects of
/// several types points to (`Member<Erased>`, from `Gc::erase`; dom's
/// `AnyEdge`, cppgc's `Member<GarbageCollectedMixin>`). It has nothing to
/// read: marking follows an object's own type (its header's), never the
/// edge's.
pub struct Erased {
    _private: (),
}

// SAFETY: never allocated (it isn't a `GcObject`): its layout is that of
// nothing, and nothing reads through it.
unsafe impl HeapType for Erased {
    fn __type() -> &'static crate::TypeSlot {
        static TYPE: crate::TypeSlot =
            crate::TypeSlot::new(&crate::TypeInfo::of::<Erased>("Erased", &[]));
        &TYPE
    }
}

impl<'gc, T: HeapType> Gc<'gc, T> {
    /// The object as any object, for an edge to objects of several types.
    pub fn erase(self) -> Gc<'gc, Erased> {
        // SAFETY: the same object, of the same heap and operation; an
        // `Erased` has no bytes to read.
        unsafe { Gc::from_raw(self.as_non_null().cast()) }
    }
}

impl<'gc> Gc<'gc, Erased> {
    /// The object as a `T`, if it is one (its header's type): an edge to
    /// objects of several types read back as the one it holds.
    pub fn downcast<T: HeapType>(self) -> Option<Gc<'gc, T>> {
        // SAFETY: a live object of the operation (a `Gc` is one), whose
        // header's first word, its type's index, was written before the
        // object was published and stays (a scavenge, which moves objects,
        // runs between operations).
        let index = unsafe {
            self.as_non_null()
                .as_ptr()
                .cast::<u8>()
                .sub(crate::page::HEADER_BYTES)
                .cast::<u32>()
                .read()
        };
        // SAFETY: the same object, which its header says is a `T`.
        (index == T::__type().index()).then(|| unsafe { Gc::from_raw(self.as_non_null().cast()) })
    }
}

impl<T: HeapType> fmt::Debug for Member<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Member({})", self.raw.load(Ordering::Relaxed))
    }
}

impl<T: HeapType> fmt::Debug for WeakMember<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WeakMember({})", self.raw.load(Ordering::Relaxed))
    }
}
