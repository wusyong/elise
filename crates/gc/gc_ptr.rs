use std::pin::Pin;
use std::ptr::NonNull;

use crate::alloc::{Allocation, Data, Ptr};
use crate::trace::Trace;

pub struct GcPtr<T: ?Sized> {
    inner: Ptr<Allocation<T>>,
}

impl<T: Trace> GcPtr<T> {
    pub(crate) fn new(data: T) -> GcPtr<T> {
        GcPtr {
            inner: Allocation::new(data),
        }
    }
}

impl<T: ?Sized> GcPtr<T> {
    /// Get a reference to the GC'd data
    ///
    /// Invariants: GcPtr must not be dangling
    pub unsafe fn data(&self) -> &T {
        self.inner.as_ref().data()
    }

    /// Tell if this ptr is managed or not
    ///
    /// Invariants: GcPtr must not be dangling
    pub unsafe fn is_unmanaged(&self) -> bool {
        self.inner.as_ref().is_unmanaged()
    }

    /// Free the data behind this GcPtr
    ///
    /// Invariants: GcPtr must not be dangling, must not be managed and must not be read again
    pub unsafe fn deallocate(self) {
        drop(Box::from_raw(self.inner.as_ptr()))
    }

    pub(crate) fn erased(self) -> Ptr<Allocation<Data>> {
        unsafe {
            Ptr(NonNull::new_unchecked(
                self.inner.as_ptr() as *mut Allocation<Data>
            ))
        }
    }

    pub(crate) unsafe fn erased_pinned<'a>(self) -> Pin<&'a Allocation<Data>> {
        Pin::new_unchecked(&*self.erased().as_ptr())
    }
}

unsafe impl<T: ?Sized + Send> Send for GcPtr<T> {}
unsafe impl<T: ?Sized + Sync> Sync for GcPtr<T> {}

unsafe impl<T: Trace + ?Sized> Trace for GcPtr<T> {
    unsafe fn mark(&self) {
        self.inner.as_ref().mark();
    }

    unsafe fn manage(&self) {
        super::manage(*self)
    }

    unsafe fn finalize(&mut self) {}
}

impl<T: ?Sized> Clone for GcPtr<T> {
    fn clone(&self) -> GcPtr<T> {
        *self
    }
}

impl<T: ?Sized> Copy for GcPtr<T> {}
