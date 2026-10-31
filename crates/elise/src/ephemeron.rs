use std::fmt;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::member::compress;
use crate::{FieldKind, Gc, HeapField, HeapType, Mutation};

/// An ephemeron (cppgc's `EphemeronPair`): a weak edge to its key, and an
/// edge to its value that keeps the value alive only while the key is.
/// Once the key dies, a GC clears both: a full GC if the key is old, a
/// scavenge if it is young (a scavenge keeps the value as a strong edge
/// would, as V8's does: a value whose young key died lives one scavenge
/// more). Read while a concurrent marking runs, its key is marked, as a
/// `WeakMember`'s object.
#[repr(C)]
pub struct EphemeronPair<K: HeapType, V: HeapType> {
    key: AtomicU32,
    value: AtomicU32,
    _target: PhantomData<fn() -> (K, V)>,
}

impl<K: HeapType, V: HeapType> EphemeronPair<K, V> {
    fn new_in_heap(key: u32, value: u32) -> Self {
        Self {
            key: AtomicU32::new(key),
            value: AtomicU32::new(value),
            _target: PhantomData,
        }
    }

    /// Its key, if it is alive.
    pub fn key<'gc>(&self, cx: &Mutation<'gc>) -> Option<Gc<'gc, K>> {
        let raw = self.checked(cx).load(Ordering::Relaxed);
        cx.inner.read_weak(raw);
        // SAFETY: a non-null key of an object of `cx`'s heap is a live `K`
        // (set only from a `Gc<'gc, K>`, and cleared once it dies; marked
        // just now if a marking runs).
        (raw != 0).then(|| unsafe {
            Gc::from_raw(NonNull::new_unchecked(cx.inner.ptr(raw as usize).cast()))
        })
    }

    /// Its value, if its key is alive (else none).
    pub fn value<'gc>(&self, cx: &Mutation<'gc>) -> Option<Gc<'gc, V>> {
        // The key read first, with its barrier: the value lives as long as
        // it.
        self.key(cx)?;
        let raw = self.value.load(Ordering::Relaxed);
        // SAFETY: a non-null value of an ephemeron whose key is alive is a
        // live `V` (set only from a `Gc<'gc, V>`, kept while the key is,
        // and cleared with it).
        (raw != 0).then(|| unsafe {
            Gc::from_raw(NonNull::new_unchecked(cx.inner.ptr(raw as usize).cast()))
        })
    }

    /// Sets the key and the value (none without a key), with the barriers:
    /// the key's as a weak edge's, the value's as an edge's (SATB's while
    /// marking runs, and the generational one).
    pub fn set<'gc>(&self, cx: &Mutation<'gc>, key: Option<Gc<'gc, K>>, value: Option<Gc<'gc, V>>) {
        let key = compress(cx, key);
        let value = if key == 0 { 0 } else { compress(cx, value) };
        let slot = self.checked(cx);
        cx.inner.write_edge(&self.value, value);
        cx.inner.write_ephemeron_key(slot, key);
    }

    /// Its key's slot, which means something only in `cx`'s heap.
    ///
    /// # Panics
    ///
    /// If it is in another heap.
    fn checked(&self, cx: &Mutation<'_>) -> &AtomicU32 {
        assert!(
            cx.inner.cage.contains((&raw const self.key).cast()),
            "an EphemeronPair of another heap"
        );
        &self.key
    }
}

/// Writes an ephemeron initialized with `(key, value)` at `at`, with its
/// barriers (the object may be old, as a `Member`'s).
///
/// # Safety
///
/// As `HeapField::__write`'s.
unsafe fn write<'gc, K: HeapType, V: HeapType>(
    (key, value): (Option<Gc<'gc, K>>, Option<Gc<'gc, V>>),
    at: *mut EphemeronPair<K, V>,
    cx: &Mutation<'gc>,
) {
    let key = compress(cx, key);
    let value = if key == 0 { 0 } else { compress(cx, value) };
    // SAFETY: as the caller promises.
    unsafe { at.write(EphemeronPair::new_in_heap(key, value)) };
    cx.inner.ephemeron_barrier(at.cast(), key);
    cx.inner
        .write_barrier(at.cast::<u32>().wrapping_add(1).cast(), value);
}

// SAFETY: its bytes are two offsets, the key's then the value's, and
// `__write` writes them.
unsafe impl<K: HeapType, V: HeapType> HeapField for EphemeronPair<K, V> {
    type Init<'gc> = (Option<Gc<'gc, K>>, Option<Gc<'gc, V>>);
    const KIND: FieldKind = FieldKind::Ephemeron;

    unsafe fn __write<'gc>(init: Self::Init<'gc>, at: *mut Self, cx: &Mutation<'gc>) {
        // SAFETY: as the caller promises.
        unsafe { write(init, at, cx) };
    }
}

// SAFETY: its bytes are `N` ephemerons in a row (8 bytes each), and
// `__write` writes each.
unsafe impl<K: HeapType, V: HeapType, const N: usize> HeapField for [EphemeronPair<K, V>; N] {
    type Init<'gc> = [(Option<Gc<'gc, K>>, Option<Gc<'gc, V>>); N];
    const KIND: FieldKind = {
        assert!(
            N <= u32::MAX as usize,
            "fewer than 2^32 ephemerons in a row"
        );
        #[expect(
            clippy::cast_possible_truncation,
            reason = "asserted above: N fits 32 bits"
        )]
        FieldKind::Ephemerons(N as u32)
    };

    unsafe fn __write<'gc>(init: Self::Init<'gc>, at: *mut Self, cx: &Mutation<'gc>) {
        let at = at.cast::<EphemeronPair<K, V>>();
        for (i, pair) in init.into_iter().enumerate() {
            // SAFETY: the array's `i`th, aligned and not initialized yet.
            unsafe { write(pair, at.wrapping_add(i), cx) };
        }
    }
}

impl<K: HeapType, V: HeapType> fmt::Debug for EphemeronPair<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "EphemeronPair({}, {})",
            self.key.load(Ordering::Relaxed),
            self.value.load(Ordering::Relaxed)
        )
    }
}
