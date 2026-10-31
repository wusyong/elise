use std::cell::Cell;
use std::fmt;
use std::marker::PhantomData;
use std::ops::Deref;
use std::ptr::NonNull;

/// An object of the heap, in one operation: a raw pointer, which no GC
/// invalidates before `'gc` ends. `'gc` is invariant, and every operation
/// gets its own (`Heap::mutate`'s closure is generic over it), so a `Gc`
/// can't leave its operation, nor mix with another's.
pub struct Gc<'gc, T> {
    ptr: NonNull<T>,
    _brand: PhantomData<(&'gc T, Cell<&'gc ()>)>,
}

impl<'gc, T> Gc<'gc, T> {
    /// # Safety
    ///
    /// `ptr` is a whole object of the heap of the operation `'gc` brands.
    pub(crate) unsafe fn from_raw(ptr: NonNull<T>) -> Self {
        Self {
            ptr,
            _brand: PhantomData,
        }
    }

    pub(crate) fn as_ptr(self) -> *mut T {
        self.ptr.as_ptr()
    }

    pub(crate) fn as_non_null(self) -> NonNull<T> {
        self.ptr
    }

    /// The object, for the whole operation.
    pub fn into_ref(self) -> &'gc T {
        // SAFETY: a whole object, which nothing moves or frees before `'gc`
        // ends; its fields change only through `Cell`s.
        unsafe { self.ptr.as_ref() }
    }

    pub fn ptr_eq(a: Self, b: Self) -> bool {
        a.ptr == b.ptr
    }
}

impl<T> Clone for Gc<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Gc<'_, T> {}

impl<T> Deref for Gc<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        (*self).into_ref()
    }
}

impl<T> fmt::Debug for Gc<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Gc({:p})", self.ptr)
    }
}
