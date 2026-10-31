//! Cells read with a shared token and written with an exclusive one: the
//! thread's one token (`LocalToken`), lent as `&` to those that read and as
//! `&mut` to the one that writes, so that the compiler keeps a write from
//! overlapping a read, with no count and no flag a borrow (qcell's
//! `TLCellOwner` and `TLCell`). The DOM keeps the style phase's results in
//! them: written by style, read by layout and paint as references.
//!
//! A cell is reached only on its thread (it isn't `Sync`), and a thread has
//! one token at a time (checked as one is made): every borrow of a cell
//! is a borrow of that one token, so a `&mut` from one never overlaps a `&`
//! from another.

use std::cell::{Cell, UnsafeCell};
use std::marker::PhantomData;

thread_local! {
    // `const`, with no drop: readable as the thread ends too.
    static TAKEN: Cell<bool> = const { Cell::new(false) };
}

/// The thread's token: what its `TokenCell`s are read (`&`) and written
/// (`&mut`) with. One a thread at a time; neither `Send` nor `Sync`, so it
/// stays on its thread's stack.
pub struct LocalToken {
    _thread: PhantomData<*const ()>,
}

impl LocalToken {
    /// The thread's token.
    ///
    /// # Panics
    ///
    /// If the thread has one.
    pub fn new() -> Self {
        assert!(
            !TAKEN.replace(true),
            "a thread has one LocalToken at a time"
        );
        Self {
            _thread: PhantomData,
        }
    }
}

impl Default for LocalToken {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for LocalToken {
    /// The thread may make another: no borrow of this one is left.
    fn drop(&mut self) {
        TAKEN.set(false);
    }
}

/// A value read with `&LocalToken` and written with `&mut LocalToken`.
/// `Send` if its value is (an `External`'s value may be dropped on the
/// sweep's worker), never `Sync`.
pub struct TokenCell<T> {
    value: UnsafeCell<T>,
}

impl<T> TokenCell<T> {
    pub const fn new(value: T) -> Self {
        Self {
            value: UnsafeCell::new(value),
        }
    }

    /// The value, borrowed while `token` is.
    #[inline]
    pub fn get<'a>(&'a self, token: &'a LocalToken) -> &'a T {
        let _ = token;
        // SAFETY: the cell is this thread's (it isn't `Sync`), and the
        // thread's one token is borrowed shared for `'a`: no `&mut` from
        // `get_mut` (which borrows it exclusively) lives in `'a`.
        unsafe { &*self.value.get() }
    }

    /// The value, borrowed mutably while `token` is.
    #[inline]
    pub fn get_mut<'a>(&'a self, token: &'a mut LocalToken) -> &'a mut T {
        let _ = token;
        // SAFETY: the thread's one token is borrowed exclusively for `'a`:
        // no other reference from `get` or `get_mut` of any cell of the
        // thread lives in `'a`, and the cell is reached on no other thread.
        unsafe { &mut *self.value.get() }
    }

    /// The value replaced by `value`: the old one, for the caller to drop
    /// (with the token let go).
    pub fn replace(&self, token: &mut LocalToken, value: T) -> T {
        std::mem::replace(self.get_mut(token), value)
    }

    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }
}

impl<T: Default> Default for TokenCell<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}
