mod alloc;
mod gc_ptr;
mod root;
mod state;
mod trace;

use std::pin::Pin;

use once_cell::sync::Lazy;

use crate::state::GcState;

pub use crate::gc_ptr::GcPtr;
pub use crate::root::Root;
pub use crate::trace::{NullTrace, Trace};

static GC: Lazy<GcState> = Lazy::new(|| GcState::default());

/// Allocate an unmanaged GcPtr
pub fn alloc_unmanaged<T: Trace>(data: T) -> GcPtr<T> {
    GcPtr::new(data)
}

/// Allocate a managed GcPtr
pub fn alloc<T: Trace>(data: T) -> GcPtr<T> {
    let gc_ptr = alloc_unmanaged(data);
    unsafe {
        manage(gc_ptr);
    }
    gc_ptr
}

/// Manage a GcPtr
///
/// Invariants: ptr must not be dangling and must not already be managed
pub unsafe fn manage<T: Trace + ?Sized>(ptr: GcPtr<T>) {
    with_gc(|gc| gc.manage(ptr))
}

/// Count objects managed by the GC
pub fn count_managed_objects() -> usize {
    with_gc(|gc| gc.count_objects())
}

/// Count roots into the GC
pub fn count_roots() -> usize {
    with_gc(|gc| gc.count_roots())
}

fn set_root<T: Trace + ?Sized>(idx: usize, ptr: GcPtr<T>) {
    with_gc(|gc| gc.set_root(idx, ptr))
}

fn pop_root(idx: usize) {
    with_gc(|gc| gc.pop_root(idx))
}

fn with_gc<T, F: FnOnce(Pin<&GcState>) -> T>(f: F) -> T {
    let gc: Pin<&GcState> = unsafe { Pin::new_unchecked(&GC) };
    f(gc)
}

pub fn collect() {
    with_gc(|gc| gc.collect())
}
