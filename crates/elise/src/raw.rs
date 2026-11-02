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
//!
//! A runtime type's edge holds none, an object, or a 31-bit integer in
//! place of one (`I31`, wasm's `i31ref`): its bits shifted left by one, the
//! lowest set, which no object's offset has. The GC skips those. An edge
//! is read and written as a `Ref`, or as the bits it holds (`read_bits`,
//! `write_bits`), the form a wasm engine's code keeps them in.
//!
//! The engine's code allocates and writes edges through bits outside any
//! operation (`alloc_bits`, `write_bits_in`), on `&mut Heap`, so that none
//! pays an operation's cost: an allocation never GCs, it fails as one is
//! due, for the engine to run with its frames' roots
//! (`Heap::safepoint_with_roots`). What it hands the embedder's code, an
//! operation turns back into objects (`from_bits`).
//!
//! An engine whose heap only its code uses owns an `EngineHeap`: the same,
//! with no operation, so that it moves between threads as its owner does.

use std::fmt;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::heap::Inner;
use crate::member::compress;
use crate::page::HEADER_BYTES;
use crate::types::{self, Finalizer, MAX_OBJECT_BYTES};
use crate::{Erased, Gc, Heap, Mutation};

mod engine;

pub use engine::{CAGE_BYTES, EngineHeap};

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
    /// Its edges' offsets, ascending, 4-aligned, inside `size`: each holds
    /// a 32-bit offset into the heap's cage, 0 for none, or an `I31`.
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
    let data = allocate(cx.inner, index, struct_size(index), None);
    // SAFETY: the object just initialized (each edge null), in the cage,
    // which no GC changes before `'gc` ends.
    unsafe { object(cx, data) }
}

/// A new array of the runtime type with `index`, of `length` elements,
/// zeroed: `None` if it would be larger than an object may be.
///
/// # Panics
///
/// If no runtime type has `index`, it is retired, or it isn't an array; if
/// the cage is full.
pub fn alloc_array<'gc>(cx: &Mutation<'gc>, index: u32, length: u32) -> Option<Gc<'gc, Erased>> {
    let (size, length_at) = array_size(index, length)?;
    let data = allocate(cx.inner, index, size, Some((length_at, length)));
    // SAFETY: as `alloc`'s.
    Some(unsafe { object(cx, data) })
}

/// Why `alloc_bits` or `alloc_array_bits` allocated nothing: wasmtime's
/// `GcHeap::alloc_raw`'s two failures.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AllocError {
    /// A GC is due, which the heap leaves to the engine, holding its
    /// frames' roots (`Heap::safepoint_with_roots`); the allocation right
    /// after it doesn't wait for another.
    GcDue,
    /// The array would be larger than an object may be.
    TooLarge,
}

/// `alloc` for an engine's code, outside any operation: the object's bits.
/// It never GCs.
///
/// # Errors
///
/// `GcDue` if a GC is due.
///
/// # Panics
///
/// As `alloc`.
pub fn alloc_bits(heap: &mut Heap, index: u32) -> Result<u32, AllocError> {
    let size = struct_size(index);
    let inner = heap.for_bits().ok_or(AllocError::GcDue)?;
    Ok(bits_of(allocate(inner, index, size, None)))
}

/// `alloc_array` for an engine's code, as `alloc_bits`.
///
/// # Errors
///
/// `TooLarge` if the array would be larger than an object may be, else
/// `GcDue` if a GC is due.
///
/// # Panics
///
/// As `alloc_array`.
pub fn alloc_array_bits(heap: &mut Heap, index: u32, length: u32) -> Result<u32, AllocError> {
    let (size, length_at) = array_size(index, length).ok_or(AllocError::TooLarge)?;
    let inner = heap.for_bits().ok_or(AllocError::GcDue)?;
    Ok(bits_of(allocate(
        inner,
        index,
        size,
        Some((length_at, length)),
    )))
}

/// The size of a struct of the runtime type with `index`.
fn struct_size(index: u32) -> usize {
    let entry = types::raw_entry(index);
    assert!(
        entry.array.is_none(),
        "the GC type with index {index} is an array"
    );
    entry.info.size() as usize
}

/// The size of an array of the runtime type with `index`, of `length`
/// elements, and where its length is: `None` if it would be larger than an
/// object may be.
fn array_size(index: u32, length: u32) -> Option<(usize, u32)> {
    let entry = types::raw_entry(index);
    let array = entry
        .array
        .unwrap_or_else(|| panic!("the GC type with index {index} isn't an array"));
    let size = (length as usize)
        .checked_mul(array.element_bytes as usize)?
        .checked_add(entry.info.size() as usize)
        .filter(|&size| size.saturating_add(HEADER_BYTES) <= MAX_OBJECT_BYTES)?;
    Some((size, array.length_at))
}

/// An object's bits: its data's offset.
fn bits_of(data: usize) -> u32 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "an offset in the cage, below CAGE_BYTES = 2^32"
    )]
    let bits = data as u32;
    bits
}

/// The object at `data`, in `cx`'s heap.
///
/// # Safety
///
/// A live object's data offset, which no GC moves before `'gc` ends.
unsafe fn object<'gc>(cx: &Mutation<'gc>, data: usize) -> Gc<'gc, Erased> {
    // SAFETY: as the caller promises; the cage's base isn't null.
    unsafe { Gc::from_raw(NonNull::new_unchecked(cx.inner.ptr(data).cast())) }
}

/// An object of `size` bytes of the runtime type with `index`, zeroed, an
/// array's `length` written at its offset: its data's offset.
fn allocate(inner: &Inner, index: u32, size: usize, length: Option<(u32, u32)>) -> usize {
    let data = inner.allocate(size, 0, false);
    let ptr = inner.ptr(data);
    // SAFETY: the object's bytes, just allocated in a committed page, which
    // nothing reads before it is published; an array's length is a `u32`
    // of them, 4-aligned (its layout's, checked), as the data is 8-aligned.
    unsafe {
        ptr::write_bytes(ptr, 0, size);
        if let Some((at, length)) = length {
            ptr.add(at as usize).cast::<u32>().write(length);
        }
    }
    inner.publish(data, index, 0, Finalizer::None);
    data
}

/// A 31-bit integer an edge holds in place of an object (wasm's `i31ref`,
/// as wasmtime's `I31`): as bits, the integer shifted left by one, the
/// lowest bit set, which no object's offset has (each is 4-aligned).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct I31(u32);

impl I31 {
    /// The low 31 bits of `value`.
    pub fn wrapping_u32(value: u32) -> Self {
        Self(value & I31_MAX)
    }

    /// The low 31 bits of `value`.
    pub fn wrapping_i32(value: i32) -> Self {
        Self::wrapping_u32(value.cast_unsigned())
    }

    /// `value`, if it fits in 31 bits unsigned.
    pub fn new_u32(value: u32) -> Option<Self> {
        (value <= I31_MAX).then_some(Self(value))
    }

    /// `value`, if it fits in 31 bits signed.
    pub fn new_i32(value: i32) -> Option<Self> {
        let wrapped = Self::wrapping_i32(value);
        (wrapped.get_i32() == value).then_some(wrapped)
    }

    /// The integer, zero-extended.
    pub fn get_u32(self) -> u32 {
        self.0
    }

    /// The integer, sign-extended.
    pub fn get_i32(self) -> i32 {
        // The 31 bits at the top, then an arithmetic shift back.
        (self.0 << 1).cast_signed() >> 1
    }

    /// As an edge holds it.
    fn bits(self) -> u32 {
        (self.0 << 1) | 1
    }
}

/// The largest `I31`, unsigned.
const I31_MAX: u32 = u32::MAX >> 1;

/// Whether an edge's `bits` are an `I31`'s rather than an object's offset
/// (or none): what the GC skips.
#[inline]
pub(crate) fn is_i31(bits: u32) -> bool {
    bits & 1 != 0
}

/// What a runtime type's edge that isn't none holds.
#[derive(Clone, Copy, Debug)]
pub enum Ref<'gc> {
    Object(Gc<'gc, Erased>),
    I31(I31),
}

impl<'gc> From<Gc<'gc, Erased>> for Ref<'gc> {
    fn from(object: Gc<'gc, Erased>) -> Self {
        Self::Object(object)
    }
}

impl From<I31> for Ref<'_> {
    fn from(value: I31) -> Self {
        Self::I31(value)
    }
}

/// `value` as an edge's bits: 0 for none, an object's offset in `cx`'s
/// cage, or an `I31`'s.
///
/// # Panics
///
/// If `value` is an object of another heap.
pub fn bits<'gc>(cx: &Mutation<'gc>, value: Option<Ref<'gc>>) -> u32 {
    match value {
        None => 0,
        Some(Ref::Object(object)) => compress(cx, Some(object)),
        Some(Ref::I31(value)) => value.bits(),
    }
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
) -> Option<Ref<'gc>> {
    // SAFETY: as the caller promises.
    let bits = unsafe { read_bits(cx, object, at) };
    // SAFETY: an object's offset in an edge of a live object of `cx`'s heap
    // is a live object (written only as one, and marked through).
    unsafe { from_bits(cx, bits) }
}

/// What `bits` hold: none, an `I31`, or an object of `cx`'s heap. How an
/// operation takes what an engine's code holds as bits (a wasm function's
/// argument to a host function).
///
/// # Safety
///
/// `bits` are 0, an `I31`'s (odd), or the offset of a live object of
/// `cx`'s heap: one the engine's frames hold (a GC given their roots kept
/// it, and none runs before `'gc` ends), or what `bits` or `read_bits`
/// gave in this operation.
pub unsafe fn from_bits<'gc>(cx: &Mutation<'gc>, bits: u32) -> Option<Ref<'gc>> {
    if bits == 0 {
        None
    } else if is_i31(bits) {
        Some(Ref::I31(I31(bits >> 1)))
    } else {
        // SAFETY: as the caller promises.
        Some(Ref::Object(unsafe { object(cx, bits as usize) }))
    }
}

/// Sets the edge at `at` of `object`, a runtime type's, with the barriers
/// a `Member`'s `set` has: the generational one, and a marking's (the old
/// value marked); neither for an `I31`.
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
    value: Option<Ref<'gc>>,
) {
    let bits = bits(cx, value);
    // SAFETY: as the caller promises; `bits` none, an `I31`'s, or a live
    // object's of `cx`'s heap (from a `Gc<'gc, _>`).
    unsafe { write_bits(cx, object, at, bits) };
}

/// The bits the edge at `at` of `object`, a runtime type's, holds: 0 for
/// none, an object's offset in the heap's cage, or an `I31`'s (odd).
///
/// # Safety
///
/// As `read_edge`.
///
/// # Panics
///
/// If `object` is another heap's.
pub unsafe fn read_bits<'gc>(cx: &Mutation<'gc>, object: Gc<'gc, Erased>, at: u32) -> u32 {
    // SAFETY: as the caller promises.
    unsafe { edge(cx, object, at) }.load(Ordering::Relaxed)
}

/// Sets the edge at `at` of `object`, a runtime type's, to `bits`, with
/// `write_edge`'s barriers.
///
/// # Safety
///
/// As `read_edge`; and `bits` is 0, an `I31`'s (odd), or the offset of an
/// object of `cx`'s heap that is live (`bits` of a `Gc<'gc, _>`, or what
/// `read_bits` read in this operation).
///
/// # Panics
///
/// If `object` is another heap's.
pub unsafe fn write_bits<'gc>(cx: &Mutation<'gc>, object: Gc<'gc, Erased>, at: u32, bits: u32) {
    // SAFETY: as the caller promises.
    cx.inner
        .write_raw_edge(unsafe { edge(cx, object, at) }, bits);
}

/// `read_bits` for an engine's code, outside any operation: `object` the
/// bits of the object whose edge it reads.
///
/// # Safety
///
/// `object` is the offset of a live object of `heap` (one the engine's
/// frames hold, as `from_bits`'s), and `at` one of the edges of its type's
/// layout (a fixed part's, or an element's below its length).
pub unsafe fn read_bits_in(heap: &Heap, object: u32, at: u32) -> u32 {
    // SAFETY: as the caller promises.
    unsafe { edge_in(heap.inner(), object, at) }.load(Ordering::Relaxed)
}

/// `write_bits` for an engine's code, outside any operation (wasmtime's
/// `GcHeap::write_gc_ref`): `object` the bits of the object whose edge it
/// sets, with the barriers.
///
/// # Safety
///
/// As `read_bits_in`; and `bits` is 0, an `I31`'s (odd), or the offset of
/// a live object of `heap`.
pub unsafe fn write_bits_in(heap: &mut Heap, object: u32, at: u32, bits: u32) {
    let inner = heap.inner();
    // SAFETY: as the caller promises.
    inner.write_raw_edge(unsafe { edge_in(inner, object, at) }, bits);
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

/// The edge at `at` of the object at `object`, in `inner`'s cage.
///
/// # Safety
///
/// As `read_bits_in`.
unsafe fn edge_in(inner: &Inner, object: u32, at: u32) -> &AtomicU32 {
    let slot = inner.ptr(object as usize).wrapping_add(at as usize);
    assert!(inner.cage.contains(slot), "an edge in the heap's cage");
    // SAFETY: as `edge`'s: the caller promises an edge of a live object,
    // which no GC frees or moves while `inner` is borrowed (a GC needs the
    // heap's `&mut`).
    unsafe { AtomicU32::from_ptr(slot.cast()) }
}
