//! A heap an engine's code alone uses, through bits (`EngineHeap`): no
//! operation runs on it, so that nothing of it stays on a thread, and it
//! moves between threads with its owner (wasmtime's `Store`, used by one
//! thread at a time, as V8's isolate under a `Locker`).

use std::cell::Cell;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use super::{
    AllocError, alloc_array_bits, alloc_bits, barrier_state_in, cage_in, id_lives_in, new_id_in,
    read_bits_in, remember_in, satb_in, size_at, write_bits_in,
};
use crate::page::{PAGE_BYTES, PAGES};
use crate::{Heap, HeapOptions, Id, Platform};

/// The cage's bytes: every object's bits are below.
pub const CAGE_BYTES: usize = crate::cage::CAGE_BYTES;

/// An offset's page is the offset shifted right by it: an index of
/// `BarrierState`'s young pages.
pub const PAGE_SHIFT: u32 = PAGE_BYTES.trailing_zeros();

const _: () = assert!(1 << PAGE_SHIFT == PAGE_BYTES && PAGES << PAGE_SHIFT == CAGE_BYTES);

/// What an engine's generated code reads for its write barriers, inline,
/// as the heap's do (`barrier_state_in`); its layout is that
/// code's, at `YOUNG_PAGES` and `MARKING`. V8's barrier reads its page's
/// flags (`MemoryChunk::kPointersToHereAreInterestingMask`) instead.
#[repr(C)]
pub struct BarrierState {
    /// The young pages' flags, a byte a page, 1 for a young one: indexed
    /// by an offset shifted right by `PAGE_SHIFT`, which any `u32` is in.
    young_pages: *const AtomicBool,
    /// Whether a marking runs, 1 or 0: the old value of an edge written
    /// meanwhile goes to `satb_in`.
    pub(super) marking: Cell<bool>,
}

impl BarrierState {
    /// Where the young pages' address is: a pointer.
    pub const YOUNG_PAGES: usize = std::mem::offset_of!(Self, young_pages);
    /// Where the marking flag is: a byte.
    pub const MARKING: usize = std::mem::offset_of!(Self, marking);

    pub(crate) fn new(young_pages: &Arc<[AtomicBool; PAGES]>) -> Self {
        Self {
            young_pages: Arc::as_ptr(young_pages).cast(),
            marking: Cell::new(false),
        }
    }

    pub(crate) fn set_marking(&self, marking: bool) {
        self.marking.set(marking);
    }
}

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
// on the heap's thread, a roots source (`Heap::set_roots_source`) its
// embedder's. An `EngineHeap` runs no operation and lends no `Heap` (so it
// has no roots source either), so whatever it owns is reached only through
// it, and moves with it (the barrier state's pointer too, to the heap's own young pages);
// its platform's workers run the jobs it gave them (`Send` already).
unsafe impl Send for EngineHeap {}

// SAFETY: its `&self` methods read the cage's base, the barrier state's
// address (not the state: generated code reads it, on the thread that has
// `&mut self` to run it), the frames' count, the
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
        cage_in(&self.heap)
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

    /// `size_at` in its cage.
    ///
    /// # Safety
    ///
    /// `object` is the offset of a live object of this heap.
    pub unsafe fn size(&self, object: u32) -> usize {
        // SAFETY: as the caller promises; the heap's cage.
        unsafe { size_at(self.cage(), object) }
    }

    /// `new_id_in`.
    ///
    /// # Safety
    ///
    /// As `new_id_in`.
    ///
    /// # Panics
    ///
    /// As `new_id_in`.
    pub unsafe fn new_id(&mut self, object: u32) -> Id {
        // SAFETY: as the caller promises.
        unsafe { new_id_in(&mut self.heap, object) }
    }

    /// `id_lives_in`.
    pub fn id_lives(&mut self, id: Id) -> bool {
        id_lives_in(&mut self.heap, id)
    }

    /// `barrier_state_in`: its code's barriers call `satb` and `remember`.
    pub fn barrier_state(&self) -> NonNull<BarrierState> {
        barrier_state_in(&self.heap)
    }

    /// `satb_in`.
    ///
    /// # Safety
    ///
    /// As `satb_in`.
    pub unsafe fn satb(&mut self, old: u32) {
        // SAFETY: as the caller promises.
        unsafe { satb_in(&mut self.heap, old) };
    }

    /// `remember_in`.
    ///
    /// # Safety
    ///
    /// As `remember_in`.
    pub unsafe fn remember(&mut self, slot: u32, value: u32) {
        // SAFETY: as the caller promises.
        unsafe { remember_in(&mut self.heap, slot, value) };
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
        // SAFETY: no operation runs on an `EngineHeap`, so no safepoint but
        // those given the roots (`safepoint_with_roots`): its frames only
        // say what `should_collect` does.
        unsafe { self.heap.leave_frames() };
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
