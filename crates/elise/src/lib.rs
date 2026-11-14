//! The DOM's GC: a heap in a 4 GiB cage, whose
//! edges are 32-bit offsets into it, collected only at safepoints between
//! operations. This is the heap's core: pages of size classes, mark-sweep
//! at a safepoint. Generations, concurrent marking and sweeping come
//! later; the API is the one they need.
//!
//! - **An operation** is `Heap::mutate`'s closure. Inside it no GC runs, so
//!   a `Gc<'gc, T>` is a raw pointer, branded with the operation's
//!   invariant `'gc`: the compiler keeps it from outliving the operation
//!   (gc-arena's model), and nothing scans the stack.
//! - **Objects** are `#[derive(GcObject)]` structs whose fields are each a
//!   `HeapField`: a `Member<T>` (an edge), plain data (`HeapData`) or an
//!   `External<T>` (native data). The derive makes an init struct;
//!   `Mutation::alloc` writes it in place, so a `Member` never exists
//!   outside the heap.
//! - **Roots** are `Persistent<T>`: an entry of the heap's root table.
//! - **Finalizers** are only `External`s' and `LocalExternal`s' `Drop`.
//!   They can't reach the heap: either holds a `T: 'static`, and a `Gc`
//!   isn't one. An `External`'s value is `Send`, and its object's finalizer
//!   runs on the sweep's worker; an object with a `LocalExternal` (any
//!   value) is finalized on the heap's thread.
//!
//! `unsafe` stays inside this crate; code using it writes none. The
//! argument is in `docs/soundness.md`.

#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        reason = "tests"
    )
)]
#![allow(
    unsafe_code,
    reason = "the heap: raw pointers into the cage, its pages and objects"
)]

// Lets `#[derive(GcObject)]`'s `::elise::` paths resolve inside this crate.
extern crate self as elise;

mod asan;
mod bits;
mod cage;
mod data;
mod ephemeron;
mod external;
mod gc_ptr;
mod heap;
mod ids;
mod js;
mod member;
mod page;
mod persistent;
mod platform;
pub mod raw;
mod site;
mod sync;
mod token;
mod types;
mod vector;
mod worklist;

pub use data::HeapData;
pub use elise_derive::GcObject;
pub use ephemeron::EphemeronPair;
pub use external::{External, LocalExternal};
pub use gc_ptr::Gc;
pub use heap::{
    Edge, EdgeKind, Fragmentation, GcRecord, Heap, HeapObject, HeapOptions, HeapStats, InitSlot,
    Mutation, RootsSource, Walk,
};
pub use ids::Id;
pub use js::{
    JsEdge, JsEdgeLocal, JsEdgeSink, WrapperLocal, WrapperMarker, js_edges_of, type_index,
};
pub use member::{Erased, Member, WeakMember};
pub use persistent::{Persistent, WeakPersistent, roots_made};
pub use platform::{
    InlinePlatform, Job, JobDelegate, JobHandle, Platform, ThreadPool, TracingController,
};
pub use site::{MAX_SITES, PretenureDecision, Site, SiteStats};
pub use token::{LocalToken, TokenCell};
pub use types::{FieldInfo, FieldKind, MAX_TYPES, TypeInfo, TypeSlot};
pub use vector::HeapVector;

/// A type the heap holds, with its layout.
///
/// # Safety
///
/// `__type`'s `TypeInfo` lists every `Member` of the type at its offset,
/// and its size and drop glue are the type's; `derive(GcObject)`
/// implements it.
pub unsafe trait HeapType: 'static {
    #[doc(hidden)]
    fn __type() -> &'static TypeSlot;
}

/// An object made with `Mutation::alloc` from its init struct.
///
/// # Safety
///
/// `__init` writes every field of the object at its offset, each with its
/// type's `HeapField` writer; `derive(GcObject)` implements it.
pub unsafe trait GcObject: HeapType + Sized {
    /// The object's fields as they are given to `alloc`.
    type Init<'gc>;

    #[doc(hidden)]
    fn __init<'gc>(init: Self::Init<'gc>, slot: InitSlot<'_, 'gc, Self>);
}

/// An object's init struct, which names its object, so that `alloc` knows
/// what it makes (`T::Init` alone doesn't say which `T`).
pub trait ObjectInit<'gc> {
    type Object: GcObject<Init<'gc> = Self>;
}

/// A type an object's field may have: `Member`, `WeakMember`,
/// `EphemeronPair`, `External`, `LocalExternal` and plain data.
/// Anything else (`Box`, `Vec`, a reference) isn't one, so
/// `derive(GcObject)` refuses it.
///
/// # Safety
///
/// `KIND` is `Member` only for `Member`, `Weak` only for `WeakMember` and
/// `Ephemeron` only for `EphemeronPair` (`Members`, `Weaks` and
/// `Ephemerons` for their arrays), whose bytes are 32-bit offsets into the
/// heap (two for an ephemeron), and `JsEdge` only for `JsEdge` (an
/// `AtomicU64`); `__write` initializes the field. A field
/// whose drop glue may not run on another thread has `MAIN_THREAD_DROP`:
/// its object's finalizer runs on the heap's thread, not the sweep's.
#[diagnostic::on_unimplemented(
    message = "`{Self}` can't be a field of a GC object",
    label = "not a `Member`, plain data, an `External` or a `LocalExternal`",
    note = "native data (a `Box`, a `Vec`, an `Arc`, a reference) goes in an `External` if it is `Send`, else in a `LocalExternal`"
)]
pub unsafe trait HeapField: 'static {
    /// What `alloc` takes for the field.
    type Init<'gc>;
    #[doc(hidden)]
    const KIND: FieldKind = FieldKind::Data;
    /// Whether the field's drop glue must run on the heap's thread (a
    /// `LocalExternal`'s).
    #[doc(hidden)]
    const MAIN_THREAD_DROP: bool = false;

    /// # Safety
    ///
    /// `at` is the field's place in an object of `cx`'s heap being
    /// initialized: aligned and not initialized yet.
    #[doc(hidden)]
    unsafe fn __write<'gc>(init: Self::Init<'gc>, at: *mut Self, cx: &Mutation<'gc>);
}
