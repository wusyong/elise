//! Types registered as the process runs, and their objects: for an
//! embedder whose types aren't Rust's, a wasm engine's GC types (or a
//! recorded heap replayed). Their layouts are checked as they are
//! registered, so that a wrong one is refused rather than misread; what
//! can't be checked, writing their objects' bytes and edges, is `unsafe`
//! (cppgc's `subtle`: an embedder's sharp tools, apart from its API).
//!
//! An embedder reserves a run of indices (`TypeRange`, one for each of its
//! engines) and registers each type at an id of its own in it, so that a
//! type's index is the range's base plus the embedder's id. A type it no
//! longer allocates, of which no object lives, it retires: its index takes
//! another type once every heap finished a full GC after (`types`).
//!
//! An object is a `Gc<'gc, Erased>`: a `Persistent<Erased>` roots it, a
//! `Member<Erased>` refers to it, `downcast` never finds a Rust type in it.

use std::fmt;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::member::compress;
use crate::page::HEADER_BYTES;
use crate::types::{self, Finalizer, MAX_OBJECT_BYTES};
use crate::{Erased, Gc, Mutation};

/// How many indices the table has: every type's is below it, a static
/// type's below `MAX_TYPES`, a runtime type's above.
#[expect(clippy::cast_possible_truncation, reason = "TABLE_LEN is 2^18")]
pub const MAX_INDEX: u32 = types::TABLE_LEN as u32;

/// A runtime type's layout: its bytes after the header, and where its
/// edges are among them. Its objects are allocated zeroed (each edge
/// null); none has drop glue.
#[derive(Clone, Copy, Debug)]
pub struct Layout<'a> {
    /// Its bytes, an array's fixed part's (its elements follow).
    pub size: u32,
    /// Its edges' offsets, ascending: each a 32-bit offset into the heap's
    /// cage, 0 for none, 4-aligned, inside `size`.
    pub edges: &'a [u32],
    /// If it is an array, its elements.
    pub array: Option<ArrayLayout>,
}

/// An array's elements: as many as its length, a `u32` of its fixed part
/// that `alloc_array` writes (and nothing else ever does), each
/// `element_bytes` from the end of the fixed part on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ArrayLayout {
    /// The length's offset: 4-aligned, inside the fixed part, not an edge.
    pub length_at: u32,
    /// 1, 2, 4, 8 or 16; the fixed part's size is a multiple of it (or of
    /// 8, past it).
    pub element_bytes: u32,
    /// Each element is an edge (`element_bytes` is 4).
    pub element_edges: bool,
}

impl ArrayLayout {
    /// The length of the array at `data`.
    ///
    /// # Safety
    ///
    /// `data` is an array of this layout, its length written.
    pub(crate) unsafe fn length(self, data: *const u8) -> u32 {
        // SAFETY: as the caller promises: a `u32` of the fixed part,
        // 4-aligned.
        unsafe { data.add(self.length_at as usize).cast::<u32>().read() }
    }
}

/// Why `TypeRange::register` refused a type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RegisterError {
    /// The id is past the range.
    OutOfRange,
    /// A type has the index: registered, or retired and not yet freed.
    Occupied,
    /// The layout is wrong: why.
    Layout(&'static str),
}

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange => f.write_str("the id is past the range"),
            Self::Occupied => f.write_str("a type has the index"),
            Self::Layout(why) => write!(f, "a wrong layout: {why}"),
        }
    }
}

impl std::error::Error for RegisterError {}

/// The layout's problem, if any.
fn check(layout: &Layout<'_>) -> Result<(), &'static str> {
    let size = layout.size as usize;
    if size.saturating_add(HEADER_BYTES) > MAX_OBJECT_BYTES {
        return Err("larger than an object may be");
    }
    let mut last = None;
    for &at in layout.edges {
        if !at.is_multiple_of(4) {
            return Err("an edge isn't 4-aligned");
        }
        if (at as usize).saturating_add(4) > size {
            return Err("an edge is past the object's size");
        }
        if last.is_some_and(|last| at <= last) {
            return Err("the edges aren't ascending");
        }
        last = Some(at);
    }
    if let Some(array) = layout.array {
        if !array.length_at.is_multiple_of(4) || (array.length_at as usize).saturating_add(4) > size
        {
            return Err("the length isn't a 4-aligned u32 of the fixed part");
        }
        if layout.edges.binary_search(&array.length_at).is_ok() {
            return Err("the length is an edge");
        }
        if !matches!(array.element_bytes, 1 | 2 | 4 | 8 | 16) {
            return Err("an element isn't 1, 2, 4, 8 or 16 bytes");
        }
        if !layout.size.is_multiple_of(array.element_bytes.min(8)) {
            return Err("the elements aren't aligned");
        }
        if array.element_edges && array.element_bytes != 4 {
            return Err("an edge element isn't 4 bytes");
        }
    }
    Ok(())
}

/// A run of the table's indices, reserved for an embedder's runtime types
/// (one for each of its engines): it registers and retires them by ids of
/// its own, an index being the base plus an id. Dropped, its indices are
/// free to other ranges: the empty ones then, the retired ones once freed;
/// one still registered stays taken for the process's life.
#[derive(Debug)]
pub struct TypeRange {
    base: u32,
    len: u32,
}

impl TypeRange {
    /// `len` indices, or `None` if the table has no such run free.
    pub fn reserve(len: u32) -> Option<Self> {
        types::reserve(len).map(|base| Self { base, len })
    }

    /// The first index.
    pub fn base(&self) -> u32 {
        self.base
    }

    pub fn len(&self) -> u32 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The index of `id`, if it is in the range.
    pub fn index(&self, id: u32) -> Option<u32> {
        (id < self.len).then(|| self.base.saturating_add(id))
    }

    /// Registers a type at `id`, its layout checked: its index. `keepalive`
    /// is dropped with its entry, once it is retired and freed (on the
    /// thread that frees it: a heap's, at the end of a full GC, or the
    /// one retiring it, dropping a heap or the range).
    ///
    /// # Errors
    ///
    /// If `id` is past the range, a type has its index, or the layout is
    /// wrong.
    pub fn register(
        &self,
        id: u32,
        layout: &Layout<'_>,
        keepalive: Box<dyn Send + Sync>,
    ) -> Result<u32, RegisterError> {
        let index = self.index(id).ok_or(RegisterError::OutOfRange)?;
        check(layout).map_err(RegisterError::Layout)?;
        types::register_raw(
            index,
            layout.size,
            layout.edges.into(),
            layout.array,
            keepalive,
        )
        .map_err(|types::Occupied| RegisterError::Occupied)?;
        Ok(index)
    }

    /// Retires the type at `id`: its entry is freed, and its index free to
    /// take another type, once every heap alive now finished a full GC
    /// that starts after.
    ///
    /// # Safety
    ///
    /// Nothing allocates the type from now on, nor while this runs, and no
    /// object of it is reachable in any heap: from a root, an edge, a weak
    /// edge or an id. (A GC reads a dead object's type until its sweep
    /// frees it; one reached again would be read after its type is freed.)
    ///
    /// # Panics
    ///
    /// If `id` is past the range, or no type that isn't retired has its
    /// index.
    pub unsafe fn retire(&self, id: u32) {
        let index = self.index(id).expect("an id in the range");
        // SAFETY: as the caller promises.
        unsafe { types::retire(index) };
    }
}

impl Drop for TypeRange {
    fn drop(&mut self) {
        types::release(self.base, self.len);
    }
}

/// A new object of the runtime type with `index`, zeroed: in the nursery,
/// as `Mutation::alloc`'s.
///
/// # Panics
///
/// If no runtime type has `index`, it is retired, or it is an array; if the
/// cage is full.
pub fn alloc<'gc>(cx: &Mutation<'gc>, index: u32) -> Gc<'gc, Erased> {
    let entry = types::raw_entry(index);
    assert!(
        entry.array.is_none(),
        "the GC type with index {index} is an array"
    );
    allocate(cx, index, entry.info.size() as usize, None)
}

/// A new array of the runtime type with `index`, of `length` elements,
/// zeroed: `None` if it would be larger than an object may be.
///
/// # Panics
///
/// If no runtime type has `index`, it is retired, or it isn't an array; if
/// the cage is full.
pub fn alloc_array<'gc>(cx: &Mutation<'gc>, index: u32, length: u32) -> Option<Gc<'gc, Erased>> {
    let entry = types::raw_entry(index);
    let array = entry
        .array
        .unwrap_or_else(|| panic!("the GC type with index {index} isn't an array"));
    let size = (length as usize)
        .checked_mul(array.element_bytes as usize)?
        .checked_add(entry.info.size() as usize)
        .filter(|&size| size.saturating_add(HEADER_BYTES) <= MAX_OBJECT_BYTES)?;
    Some(allocate(cx, index, size, Some((array.length_at, length))))
}

/// An object of `size` bytes of the runtime type with `index`, zeroed, an
/// array's `length` written at its offset.
fn allocate<'gc>(
    cx: &Mutation<'gc>,
    index: u32,
    size: usize,
    length: Option<(u32, u32)>,
) -> Gc<'gc, Erased> {
    let data = cx.inner.allocate(size, 0, false);
    let ptr = cx.inner.ptr(data);
    // SAFETY: the object's bytes, just allocated in a committed page, which
    // nothing reads before it is published; an array's length is a `u32`
    // of them, 4-aligned (its layout's, checked), as the data is 8-aligned.
    unsafe {
        ptr::write_bytes(ptr, 0, size);
        if let Some((at, length)) = length {
            ptr.add(at as usize).cast::<u32>().write(length);
        }
    }
    cx.inner.publish(data, index, 0, Finalizer::None);
    // SAFETY: the object just initialized (each edge null), in the cage,
    // which no GC changes before `'gc` ends.
    unsafe { Gc::from_raw(NonNull::new_unchecked(ptr.cast())) }
}

/// The index of `object`'s type.
pub fn index_of(object: Gc<'_, Erased>) -> u32 {
    // SAFETY: a live object (a `Gc` is one), whose header's first word, its
    // type's index, was written before it was published and stays.
    unsafe {
        object
            .as_ptr()
            .cast::<u8>()
            .sub(HEADER_BYTES)
            .cast::<u32>()
            .read()
    }
}

/// `object`'s bytes: what its type's layout describes. The embedder writes
/// its data through it; its edges only with `write_edge`, its length
/// never.
pub fn data(object: Gc<'_, Erased>) -> NonNull<u8> {
    object.as_non_null().cast()
}

/// The edge at `at` of `object`, a runtime type's.
///
/// # Safety
///
/// `at` is one of the edges of `object`'s type's layout (a fixed part's,
/// or an element's below its length).
///
/// # Panics
///
/// If `object` is another heap's.
pub unsafe fn read_edge<'gc>(
    cx: &Mutation<'gc>,
    object: Gc<'gc, Erased>,
    at: u32,
) -> Option<Gc<'gc, Erased>> {
    // SAFETY: as the caller promises.
    let raw = unsafe { edge(cx, object, at) }.load(Ordering::Relaxed);
    // SAFETY: a non-null edge of a live object of `cx`'s heap is a live
    // object (set only from a `Gc<'gc, _>`, and marked through).
    (raw != 0)
        .then(|| unsafe { Gc::from_raw(NonNull::new_unchecked(cx.inner.ptr(raw as usize).cast())) })
}

/// Sets the edge at `at` of `object`, a runtime type's, with the barriers
/// a `Member`'s `set` has: the generational one, and a marking's (the old
/// value marked).
///
/// # Safety
///
/// As `read_edge`.
///
/// # Panics
///
/// If `object` or `value` is another heap's.
pub unsafe fn write_edge<'gc>(
    cx: &Mutation<'gc>,
    object: Gc<'gc, Erased>,
    at: u32,
    value: Option<Gc<'gc, Erased>>,
) {
    let raw = compress(cx, value);
    // SAFETY: as the caller promises.
    cx.inner.write_edge(unsafe { edge(cx, object, at) }, raw);
}

/// The edge at `at` of `object`.
///
/// # Safety
///
/// As `read_edge`.
unsafe fn edge<'gc>(cx: &Mutation<'gc>, object: Gc<'gc, Erased>, at: u32) -> &'gc AtomicU32 {
    let slot = object.as_ptr().cast::<u8>().wrapping_add(at as usize);
    assert!(cx.inner.cage.contains(slot), "an object of another heap");
    // SAFETY: as the caller promises, an edge of a live object, 4-aligned
    // (its layout's, checked, from 8-aligned data), which no GC frees or
    // moves before `'gc` ends; an edge is only written atomically while
    // anything may read it so.
    unsafe { AtomicU32::from_ptr(slot.cast()) }
}
