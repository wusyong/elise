//! A heap an engine's code alone uses, through bits (`EngineHeap`): no
//! operation runs on it, so that nothing of it stays on a thread, and it
//! moves between threads with its owner (wasmtime's `Store`, used by one
//! thread at a time, as V8's isolate under a `Locker`).

use std::sync::Arc;

use super::{AllocError, alloc_array_bits, alloc_bits, read_bits_in, write_bits_in};
use crate::page::HEADER_BYTES;
use crate::{Heap, HeapOptions, Id, Platform, types};

/// The cage's bytes: every object's bits are below.
pub const CAGE_BYTES: usize = crate::cage::CAGE_BYTES;

/// A heap of runtime types' objects, allocated and written through bits
/// (as `alloc_bits`, `write_bits_in`), which lends no `Heap`: no operation
/// runs on it, so none leaves a root (`Persistent`, `WeakPersistent`), a
/// `LocalExternal` or an id. Its `&self` methods read the cage's base and
/// objects' bytes, nothing of the heap's state that `&mut self` changes
/// but the frames' count; the rest takes `&mut self`.
pub struct EngineHeap {
    heap: Heap,
}

// SAFETY: a `Heap` isn't `Send` for what operations leave on its thread:
// roots a `Persistent` holds by address in its tables (dropped on that
// thread as the heap runs on another), `LocalExternal`s its finalizers drop
// on the heap's thread. An `EngineHeap` runs no operation and lends no
// `Heap`, so whatever it owns is reached only through it, and moves with
// it; its platform's workers run the jobs it gave them (`Send` already).
unsafe impl Send for EngineHeap {}

// SAFETY: its `&self` methods read the cage's base, the frames' count, the
// used bytes' counts (plain reads of `Cell`s, which only `&mut self`
// writes: a `Cell` is never written through `&Inner` but by the heap's own
// thread, in a call holding `&mut self`), and objects' headers, lengths and
// edges (an edge atomically, as the workers do); none borrows a `RefCell`.
unsafe impl Sync for EngineHeap {}

impl EngineHeap {
    /// A heap whose GCs mark on `platform`'s workers (an `InlinePlatform`
    /// for none).
    pub fn new(options: HeapOptions, platform: Arc<dyn Platform>) -> Self {
        Self {
            heap: Heap::with_platform(options, platform),
        }
    }

    /// The cage's start: an object's data is at its bits past it.
    pub fn cage(&self) -> *mut u8 {
        self.heap.inner().cage.base()
    }

    /// `alloc_bits`.
    ///
    /// # Errors
    ///
    /// As `alloc_bits`.
    pub fn alloc(&mut self, index: u32) -> Result<u32, AllocError> {
        alloc_bits(&mut self.heap, index)
    }

    /// `alloc_array_bits`.
    ///
    /// # Errors
    ///
    /// As `alloc_array_bits`.
    pub fn alloc_array(&mut self, index: u32, length: u32) -> Result<u32, AllocError> {
        alloc_array_bits(&mut self.heap, index, length)
    }

    /// `read_bits_in`.
    ///
    /// # Safety
    ///
    /// As `read_bits_in`.
    pub unsafe fn read_bits(&self, object: u32, at: u32) -> u32 {
        // SAFETY: as the caller promises.
        unsafe { read_bits_in(&self.heap, object, at) }
    }

    /// `write_bits_in`.
    ///
    /// # Safety
    ///
    /// As `write_bits_in`.
    pub unsafe fn write_bits(&mut self, object: u32, at: u32, bits: u32) {
        // SAFETY: as the caller promises.
        unsafe { write_bits_in(&mut self.heap, object, at, bits) };
    }

    /// The bytes of the object with `object` bits that its type's layout
    /// describes (an array's elements too), from `cage() + object` on.
    ///
    /// # Safety
    ///
    /// `object` is the offset of a live object of this heap.
    pub unsafe fn size(&self, object: u32) -> usize {
        let data = self.heap.inner().ptr(object as usize);
        // SAFETY: as the caller promises, a live object's header, whose
        // first word is its type's index (written before it was published,
        // and kept), and whose type isn't freed while it lives.
        unsafe {
            let index = data.sub(HEADER_BYTES).cast::<u32>().read();
            types::entry(index).raw_size(data)
        }
    }

    /// `Mutation::new_id`: the next id of the heap's id table, for the
    /// object with `object` bits.
    ///
    /// # Safety
    ///
    /// `object` is the offset of a live object of this heap (the table's
    /// GCs read its mark bits and its page).
    ///
    /// # Panics
    ///
    /// Once 2^32 - 1 ids were given.
    pub unsafe fn new_id(&mut self, object: u32) -> Id {
        let inner = self.heap.inner();
        let young = inner.is_young(object as usize);
        inner.ids.borrow_mut().insert(object, young)
    }

    /// Whether `id`'s object lives, as far as the GCs found: unlike
    /// `Mutation::by_id`, it doesn't mark the object, so it tells what
    /// died, not an object to take (one a marking running hasn't reached
    /// yet still lives). `&mut self`: it borrows the id table.
    pub fn id_lives(&mut self, id: Id) -> bool {
        self.heap.inner().ids.borrow().get(id) != 0
    }

    /// Bytes in use, old and young (`HeapStats::used_bytes`).
    pub fn used_bytes(&self) -> usize {
        self.heap.inner().used_bytes()
    }

    /// `Heap::enter_frames`.
    pub fn enter_frames(&mut self) {
        self.heap.enter_frames();
    }

    /// `Heap::leave_frames`.
    ///
    /// # Panics
    ///
    /// As `Heap::leave_frames`.
    pub fn leave_frames(&mut self) {
        self.heap.leave_frames();
    }

    /// `Heap::in_frames`.
    pub fn in_frames(&self) -> bool {
        self.heap.in_frames()
    }

    /// `Heap::should_collect`.
    pub fn should_collect(&mut self) -> bool {
        self.heap.should_collect()
    }

    /// `Heap::safepoint_with_roots`.
    ///
    /// # Safety
    ///
    /// As `Heap::safepoint_with_roots`.
    pub unsafe fn safepoint_with_roots(&mut self, roots: impl FnMut(&mut dyn FnMut(&mut u32))) {
        // SAFETY: as the caller promises.
        unsafe { self.heap.safepoint_with_roots(roots) };
    }

    /// `Heap::scavenge_with_roots`.
    ///
    /// # Safety
    ///
    /// As `Heap::scavenge_with_roots`.
    pub unsafe fn scavenge_with_roots(&mut self, roots: impl FnMut(&mut dyn FnMut(&mut u32))) {
        // SAFETY: as the caller promises.
        unsafe { self.heap.scavenge_with_roots(roots) };
    }

    /// `Heap::collect_with_roots`.
    ///
    /// # Safety
    ///
    /// As `Heap::collect_with_roots`.
    pub unsafe fn collect_with_roots(&mut self, roots: impl FnMut(&mut dyn FnMut(&mut u32))) {
        // SAFETY: as the caller promises.
        unsafe { self.heap.collect_with_roots(roots) };
    }
}
