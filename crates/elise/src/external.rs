use std::fmt;
use std::mem::needs_drop;
use std::ops::Deref;

use crate::{HeapField, Mutation};

/// Native data in an object (an `Arc`, a `Box`, a `Vec`): the GC never
/// reads it, and its `Drop` is the object's finalizer, which runs on the
/// sweep's worker (V8 frees ArrayBuffers' backing stores so); so the value
/// is `Send`, and a value that isn't goes in a `LocalExternal`. It lends
/// only `&T` for as long as the object is borrowed; mutable data goes in
/// an atomic or a `Mutex` inside, or in a `LocalExternal`'s `Cell` or
/// `RefCell`. `T: 'static` keeps `Gc`s out of it, and no `Member` can be
/// moved into one, so a finalizer can't reach the heap.
///
/// Its value is in the object for now; it can become an index into a side
/// table without changing this API, as long as the table doesn't move its
/// entries during an operation (the `&T` it lent would dangle).
pub struct External<T: Send + 'static> {
    value: T,
}

impl<T: Send + 'static> External<T> {
    pub fn new(value: T) -> Self {
        Self { value }
    }

    pub fn get(&self) -> &T {
        &self.value
    }
}

impl<T: Send + 'static> Deref for External<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T: Send + fmt::Debug + 'static> fmt::Debug for External<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("External").field(&self.value).finish()
    }
}

// SAFETY: not a `Member`; `__write` moves the value in; its drop glue is a
// `Send` value's, which may run on another thread.
unsafe impl<T: Send + 'static> HeapField for External<T> {
    type Init<'gc> = Self;

    unsafe fn __write<'gc>(init: Self, at: *mut Self, _: &Mutation<'gc>) {
        // SAFETY: as the caller promises.
        unsafe { at.write(init) };
    }
}

/// An `External` whose value needn't be `Send` (an `Rc`, a `Persistent`): its
/// object's finalizer runs on the heap's thread, at the safepoint after
/// the GC that found it dead.
pub struct LocalExternal<T: 'static> {
    value: T,
}

impl<T: 'static> LocalExternal<T> {
    pub fn new(value: T) -> Self {
        Self { value }
    }

    pub fn get(&self) -> &T {
        &self.value
    }
}

impl<T: 'static> Deref for LocalExternal<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T: fmt::Debug + 'static> fmt::Debug for LocalExternal<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("LocalExternal").field(&self.value).finish()
    }
}

// SAFETY: not a `Member`; `__write` moves the value in; its drop glue, if
// any, runs on the heap's thread (`MAIN_THREAD_DROP`).
unsafe impl<T: 'static> HeapField for LocalExternal<T> {
    type Init<'gc> = Self;
    const MAIN_THREAD_DROP: bool = needs_drop::<T>();

    unsafe fn __write<'gc>(init: Self, at: *mut Self, _: &Mutation<'gc>) {
        // SAFETY: as the caller promises.
        unsafe { at.write(init) };
    }
}
