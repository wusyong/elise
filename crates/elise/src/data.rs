use std::cell::{Cell, RefCell};
use std::marker::PhantomData;

use crate::{HeapField, Mutation};

/// Plain data an object holds as it is: no pointers (no references, raw
/// pointers, `Box`, `Arc`, `Vec`: those go in an `External`) and no drop
/// glue. The GC copies it bytewise and never reads it; with the side table
/// the heap would hold no native pointer.
///
/// # Safety
///
/// The type holds no pointer and has no drop glue.
#[diagnostic::on_unimplemented(
    message = "`{Self}` can't be a field of a GC object",
    label = "not a `Member`, plain data, an `External` or a `LocalExternal`",
    note = "native data (a `Box`, a `Vec`, an `Arc`, a reference) goes in an `External` if it is `Send`, else in a `LocalExternal`"
)]
pub unsafe trait HeapData: 'static {}

macro_rules! plain {
    ($($ty:ty),*) => {
        // SAFETY: numbers.
        $( unsafe impl HeapData for $ty {} )*
    };
}

plain!(
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    f32,
    f64,
    bool,
    char,
    ()
);

// SAFETY: as their elements.
unsafe impl<T: HeapData, const N: usize> HeapData for [T; N] {}
// SAFETY: as its value.
unsafe impl<T: HeapData> HeapData for Option<T> {}
// SAFETY: as its value.
unsafe impl<T: HeapData> HeapData for Cell<T> {}
// SAFETY: as its value; a borrow of it can't outlive the operation.
unsafe impl<T: HeapData> HeapData for RefCell<T> {}
// SAFETY: holds nothing.
unsafe impl<T: ?Sized + 'static> HeapData for PhantomData<T> {}

// SAFETY: not a `Member`; `__write` moves the value in.
unsafe impl<T: HeapData> HeapField for T {
    type Init<'gc> = T;

    unsafe fn __write<'gc>(init: T, at: *mut T, _: &Mutation<'gc>) {
        // SAFETY: as the caller promises.
        unsafe { at.write(init) };
    }
}
