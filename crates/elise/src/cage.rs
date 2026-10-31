//! The cage: the address space a heap's objects live in (V8's pointer
//! compression cage). An edge is a 32-bit offset from its start, 0 being
//! null. It is 4 GiB, reserved without access and aligned to its size, and
//! committed as the heap grows: a page at a time, or a large object's
//! bytes, rounded up to the OS's pages. Pages the heap doesn't need any
//! more are discarded: their memory goes back to the OS, their addresses
//! stay reserved.
//!
//! Under Miri, which can't reserve address space, it is a smaller block
//! from `std::alloc`, committed all along; discarding does nothing.

use std::ptr::NonNull;

/// The cage's bytes.
#[cfg(not(miri))]
pub(crate) const CAGE_BYTES: usize = 1 << 32;
#[cfg(miri)]
pub(crate) const CAGE_BYTES: usize = 16 << 20;

pub(crate) struct Cage {
    base: NonNull<u8>,
    /// What to give back when the cage goes: the whole reservation, of
    /// which the cage is the aligned part.
    reservation: NonNull<u8>,
    reservation_bytes: usize,
}

// SAFETY: its pointers never change after `new`; `commit` and `discard`
// change the OS's mappings of ranges of the reservation, which the OS
// orders: a heap's threads commit one at a time (under its space's lock),
// and a worker discards (`Discarder`) only pages no thread commits.
unsafe impl Sync for Cage {}

impl Cage {
    pub(crate) fn new() -> Self {
        // SAFETY: a new reservation, given back in `drop`.
        let (reservation, reservation_bytes) = unsafe { reserve() };
        let start = reservation.as_ptr().addr();
        let aligned = start
            .checked_next_multiple_of(CAGE_BYTES)
            .expect("the reservation is below the address space's top");
        // SAFETY: twice the cage's bytes were reserved (but under Miri,
        // where the block is aligned already), so the aligned cage is in
        // the reservation.
        let base = unsafe { reservation.add(aligned.wrapping_sub(start)) };
        Self {
            base,
            reservation,
            reservation_bytes,
        }
    }

    pub(crate) fn base(&self) -> *mut u8 {
        self.base.as_ptr()
    }

    /// Whether `ptr` is in the cage.
    pub(crate) fn contains(&self, ptr: *const u8) -> bool {
        ptr.addr().wrapping_sub(self.base.as_ptr().addr()) < CAGE_BYTES
    }

    /// Makes `bytes` at `offset` readable and writable (zeroed where they
    /// were reserved; what was committed stays).
    ///
    /// # Panics
    ///
    /// If the range isn't in the cage, or the OS refuses (out of memory).
    pub(crate) fn commit(&self, offset: usize, bytes: usize) {
        let end = offset.checked_add(bytes).expect("a range in the cage");
        assert!(end <= CAGE_BYTES, "a range in the cage");
        // SAFETY: in the cage (checked), which the reservation holds.
        unsafe { commit(self.base.as_ptr().add(offset), bytes) };
    }

    /// Gives the memory of `bytes` at `offset` back to the OS, keeping the
    /// addresses: the range is reserved again, to be committed anew.
    ///
    /// # Panics
    ///
    /// If the range isn't in the cage, or the OS refuses.
    pub(crate) fn discard(&self, offset: usize, bytes: usize) {
        // SAFETY: the cage is there while borrowed; no object of the heap
        // is in the range any more (the caller's promise, as the heap's).
        unsafe { self.discarder().discard(offset, bytes) };
    }

    /// `discard` for another thread, which the cage's borrow can't reach.
    pub(crate) fn discarder(&self) -> Discarder {
        Discarder { base: self.base }
    }
}

/// A cage's `discard`, for a worker that gives free pages' memory back to
/// the OS, outside the heap's GC pauses.
#[derive(Clone, Copy)]
pub(crate) struct Discarder {
    base: NonNull<u8>,
}

// SAFETY: only its cage's address, which `discard` maps anew; what it may
// discard and when is the caller's promise there.
unsafe impl Send for Discarder {}
// SAFETY: as `Send`: it holds nothing that changes.
unsafe impl Sync for Discarder {}

impl Discarder {
    /// `Cage::discard`.
    ///
    /// # Safety
    ///
    /// The cage is still there, and no object of the heap is in the range,
    /// which no thread commits until this returns.
    ///
    /// # Panics
    ///
    /// If the range isn't in the cage, or the OS refuses.
    pub(crate) unsafe fn discard(&self, offset: usize, bytes: usize) {
        let end = offset.checked_add(bytes).expect("a range in the cage");
        assert!(end <= CAGE_BYTES, "a range in the cage");
        // SAFETY: in the cage (checked), which the reservation holds while
        // the cage is there (the caller's promise); nothing in use there
        // (the caller's promise).
        unsafe { discard(self.base.as_ptr().add(offset), bytes) };
    }
}

/// What a commit is rounded up to: the OS's page.
pub(crate) fn commit_granularity() -> usize {
    #[cfg(all(unix, not(miri)))]
    let bytes = rustix::param::page_size();
    #[cfg(all(windows, not(miri)))]
    let bytes = {
        use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
        let mut info = SYSTEM_INFO::default();
        // SAFETY: writes the struct it is given.
        unsafe { GetSystemInfo(&mut info) };
        info.dwPageSize as usize
    };
    #[cfg(miri)]
    let bytes = 4096;
    bytes
}

impl Drop for Cage {
    fn drop(&mut self) {
        // SAFETY: the reservation `new` made, given back once.
        unsafe { release(self.reservation, self.reservation_bytes) };
    }
}

/// Reserves twice the cage's bytes, without access: the reservation and
/// its bytes.
///
/// # Safety
///
/// The caller gives it back with `release`.
#[cfg(all(unix, not(miri)))]
unsafe fn reserve() -> (NonNull<u8>, usize) {
    use rustix::mm::{MapFlags, ProtFlags, mmap_anonymous};
    let bytes = CAGE_BYTES
        .checked_mul(2)
        .expect("twice the cage fits the address space");
    // SAFETY: a new anonymous mapping, at no address of ours.
    let start = unsafe {
        mmap_anonymous(
            std::ptr::null_mut(),
            bytes,
            ProtFlags::empty(),
            MapFlags::PRIVATE | MapFlags::NORESERVE,
        )
    }
    .expect("the address space has room for a cage");
    (
        NonNull::new(start.cast()).expect("mmap gives a non-null address"),
        bytes,
    )
}

/// # Safety
///
/// `ptr` and `bytes` are in a reservation of `reserve`.
#[cfg(all(unix, not(miri)))]
unsafe fn commit(ptr: *mut u8, bytes: usize) {
    use rustix::mm::{MprotectFlags, mprotect};
    // SAFETY: as the caller promises.
    unsafe {
        mprotect(
            ptr.cast(),
            bytes,
            MprotectFlags::READ | MprotectFlags::WRITE,
        )
    }
    .expect("the OS commits the cage's pages");
}

/// # Safety
///
/// `ptr` and `bytes` are in a reservation of `reserve`, and nothing there
/// is in use.
#[cfg(all(unix, not(miri)))]
unsafe fn discard(ptr: *mut u8, bytes: usize) {
    use rustix::mm::{MapFlags, ProtFlags, mmap_anonymous};
    // A new mapping over the old (V8's decommit on macOS): its memory is
    // gone, and the range is as `reserve` left it. `MADV_FREE` would keep
    // it until the OS needs it, and the range readable.
    // SAFETY: as the caller promises.
    unsafe {
        mmap_anonymous(
            ptr.cast(),
            bytes,
            ProtFlags::empty(),
            MapFlags::PRIVATE | MapFlags::FIXED | MapFlags::NORESERVE,
        )
    }
    .expect("the OS discards the cage's pages");
}

/// # Safety
///
/// What `reserve` gave, once.
#[cfg(all(unix, not(miri)))]
unsafe fn release(reservation: NonNull<u8>, bytes: usize) {
    // SAFETY: as the caller promises.
    unsafe { rustix::mm::munmap(reservation.as_ptr().cast(), bytes) }
        .expect("the reservation is unmapped");
}

#[cfg(all(windows, not(miri)))]
unsafe fn reserve() -> (NonNull<u8>, usize) {
    use windows_sys::Win32::System::Memory::{MEM_RESERVE, PAGE_NOACCESS, VirtualAlloc};
    let bytes = CAGE_BYTES
        .checked_mul(2)
        .expect("twice the cage fits the address space");
    // SAFETY: a new reservation, at no address of ours. Windows can't
    // give back part of one, so it all stays reserved.
    let start = unsafe { VirtualAlloc(std::ptr::null(), bytes, MEM_RESERVE, PAGE_NOACCESS) };
    (
        NonNull::new(start.cast()).expect("the address space has room for a cage"),
        bytes,
    )
}

#[cfg(all(windows, not(miri)))]
unsafe fn commit(ptr: *mut u8, bytes: usize) {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, PAGE_READWRITE, VirtualAlloc};
    // SAFETY: as the caller promises: in the reservation.
    let committed = unsafe { VirtualAlloc(ptr.cast(), bytes, MEM_COMMIT, PAGE_READWRITE) };
    assert!(!committed.is_null(), "the OS commits the cage's pages");
}

#[cfg(all(windows, not(miri)))]
unsafe fn discard(ptr: *mut u8, bytes: usize) {
    use windows_sys::Win32::System::Memory::{MEM_DECOMMIT, VirtualFree};
    // SAFETY: as the caller promises: in the reservation, unused.
    let decommitted = unsafe { VirtualFree(ptr.cast(), bytes, MEM_DECOMMIT) };
    assert!(decommitted != 0, "the OS discards the cage's pages");
}

#[cfg(all(windows, not(miri)))]
unsafe fn release(reservation: NonNull<u8>, _: usize) {
    use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
    // SAFETY: as the caller promises.
    let released = unsafe { VirtualFree(reservation.as_ptr().cast(), 0, MEM_RELEASE) };
    assert!(released != 0, "the reservation is released");
}

#[cfg(miri)]
fn layout() -> std::alloc::Layout {
    std::alloc::Layout::from_size_align(CAGE_BYTES, CAGE_BYTES).expect("a power of two")
}

#[cfg(miri)]
unsafe fn reserve() -> (NonNull<u8>, usize) {
    // SAFETY: the layout isn't empty.
    let block = unsafe { std::alloc::alloc_zeroed(layout()) };
    let block = NonNull::new(block).unwrap_or_else(|| std::alloc::handle_alloc_error(layout()));
    (block, CAGE_BYTES)
}

#[cfg(miri)]
unsafe fn commit(_: *mut u8, _: usize) {}

#[cfg(miri)]
unsafe fn discard(_: *mut u8, _: usize) {}

#[cfg(miri)]
unsafe fn release(reservation: NonNull<u8>, _: usize) {
    // SAFETY: allocated in `reserve` with this layout.
    unsafe { std::alloc::dealloc(reservation.as_ptr(), layout()) };
}
