//! ASan's view of the cage, under cargo-fuzz (`--cfg fuzzing`, built with
//! ASan): the cage is memory of our own mapping, which ASan sees as one
//! valid block, so a stale pointer that reads a dead object goes unseen
//! (the debug poison shows only if the value read is used). The heap
//! poisons its free slots and pages after a sweep and unpoisons a slot as
//! it allocates it: ASan then stops at the read itself. Elsewhere these do
//! nothing.

#[cfg(fuzzing)]
unsafe extern "C" {
    fn __asan_poison_memory_region(addr: *const u8, size: usize);
    fn __asan_unpoison_memory_region(addr: *const u8, size: usize);
}

/// Marks `bytes` at `ptr` as not to be touched.
pub(crate) fn poison(ptr: *const u8, bytes: usize) {
    // SAFETY: only ASan's shadow of the range changes, nothing the program
    // reads.
    #[cfg(fuzzing)]
    unsafe {
        __asan_poison_memory_region(ptr, bytes);
    }
    let _ = (ptr, bytes);
}

/// Marks `bytes` at `ptr` as usable again.
pub(crate) fn unpoison(ptr: *const u8, bytes: usize) {
    // SAFETY: as `poison`.
    #[cfg(fuzzing)]
    unsafe {
        __asan_unpoison_memory_region(ptr, bytes);
    }
    let _ = (ptr, bytes);
}
