//! Marking with a JS engine's heap in one major GC, as cppgc and V8 do in
//! Chrome's unified heap; the engine's wrappers stand for the heap's
//! objects, and the engine drives the old generation's marking
//! (`HeapOptions::driven`: `Heap::start_marking`, `advance_marking`,
//! `finish_marking`).
//!
//! As cppgc's objects, those the engine refers to never move: in a driven
//! heap, a type with a `JsEdge` is always allocated in the old generation,
//! which no GC compacts.
//!
//! - **Wrappers**: a wrapper holds its object's address
//!   (`Mutation::engine_address`).
//!   The engine hands the object over as it marks the wrapper
//!   (`WrapperMarker`, from its own threads), which marks it; reading it
//!   back (`Mutation::from_engine`) marks it too while marking runs, as a
//!   weak read does: the snapshot at the beginning doesn't hold what only
//!   a wrapper held. The heap's scavenges and own full GCs know nothing of
//!   wrappers: a driven heap's full GCs are the engine's, and its wrapped
//!   objects are old.
//! - **JS edges** (`JsEdge`): an object's reference to one of the
//!   engine's (a node's wrapper: the engine's `TracedReference` itself, in
//!   the object), which the heap never reads: the marker hands its address
//!   to the engine (`JsEdgeSink`) as it scans the object, on its own
//!   threads. When the object dies, so does the reference: the engine
//!   already let go of what it didn't mark.
//!
//! The protocol, with the snapshot-at-the-beginning barrier and the
//! engine's Dijkstra one, is model-checked (`docs/soundness.md`).

use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::heap::Mutation;
use crate::page::HEADER_BYTES;
use crate::types::{self, FieldKind};
use crate::worklist::Local;
use crate::{HeapField, HeapType};

/// What the engine's threads hand wrapped objects over with while the
/// heap's old generation is marked (`Heap::start_marking` gives it): each
/// takes a `WrapperLocal`. It outlives the marking safely: what it hands
/// over then goes nowhere.
#[derive(Clone)]
pub struct WrapperMarker {
    pub(crate) marker: std::sync::Arc<crate::heap::concurrent::Marker>,
}

impl WrapperMarker {
    /// A thread's list of objects to hand over: published as it fills,
    /// with `publish`, and when dropped.
    pub fn local(&self) -> WrapperLocal {
        WrapperLocal {
            marker: self.clone(),
            local: Local::new(),
        }
    }

    /// Whether the heap's marker has nothing left of what was published:
    /// no segment, no background run (the mutator's barrier aside, which a
    /// step of the marking publishes).
    pub fn is_drained(&self) -> bool {
        self.marker.drained()
    }
}

/// One thread's wrapped objects to hand over (cppgc's `CppMarkingState`,
/// for V8's marking threads).
pub struct WrapperLocal {
    marker: WrapperMarker,
    local: Local,
}

impl WrapperLocal {
    /// Hands over `object`, of a wrapper the engine marked: marked, and
    /// marked from by the heap's marker.
    ///
    /// # Safety
    ///
    /// While the marking that gave the marker runs, `object` is a live
    /// object of its heap (a wrapper's: what `Mutation::engine_address`
    /// gave of an object of that heap, which the wrapper keeps alive). Once the marking ended or the
    /// heap is dropped, a handover touches nothing.
    pub unsafe fn hand(&mut self, object: NonNull<u8>) {
        // SAFETY: as the caller promises.
        unsafe { self.marker.marker.hand(object, &mut self.local) };
    }

    /// Publishes what it holds for the heap's marker.
    pub fn publish(&mut self) {
        self.marker.marker.worklist.flush(&mut self.local);
    }

    /// Whether it holds nothing to publish.
    pub fn is_empty(&self) -> bool {
        self.local.is_empty()
    }
}

impl Drop for WrapperLocal {
    fn drop(&mut self) {
        self.publish();
    }
}

/// The engine, as the heap's marker hands it JS edges (`JsEdge`) while it
/// drives a marking: each thread that marks takes a local. The heap calls
/// `local` only from the threads that mark, in the marking `start_marking`
/// took the sink for (the workers' runs, and the heap's thread's steps
/// within the engine's), and drops every local (at its run's or step's
/// end) before `advance_marking` reports the marking done or
/// `finish_marking` returns: an engine may count on it (V8's worklists
/// exist only while V8 marks).
pub trait JsEdgeSink: Send + Sync + 'static {
    fn local(&self) -> Box<dyn JsEdgeLocal>;
}

/// A thread's JS edges for the engine to mark, by their addresses;
/// published when dropped (the end of the thread's run).
pub trait JsEdgeLocal {
    /// Marks what the JS edge at `edge` refers to, if anything.
    ///
    /// # Safety
    ///
    /// The heap calls it with a `JsEdge` of an object it marked, in the
    /// marking the sink is for: the object lives (and doesn't move) until
    /// the marking ends, and the edge's bytes are only the engine's to
    /// write, atomically, meanwhile.
    unsafe fn mark(&mut self, edge: NonNull<JsEdge>);
}

/// An object's reference to one of a JS engine's objects (a node's
/// wrapper): the engine's own reference (V8's `TracedReference`, a
/// pointer), held in the object, which the heap never reads nor writes
/// after it is made empty. Its address is the engine's to write through
/// (`as_ptr`); the heap's marker hands it to the engine (`JsEdgeSink`)
/// when it scans the object, in a marking the engine drives. A type with
/// one is allocated old in a driven heap: the engine's reference to it
/// never moves.
///
/// In a heap no engine drives, nothing reads or writes those bytes: no
/// engine has their address, and the marker hands no JS edge to anyone.
/// They are the embedder's word there (`embedder_word`): a wasm document's
/// node keeps its main store's handle in it, as a JS document's keeps its
/// main world's wrapper (Blink's `ScriptWrappable::main_world_wrapper_`).
#[repr(C, align(8))]
pub struct JsEdge {
    raw: AtomicU64,
}

impl JsEdge {
    /// Where the engine keeps its reference: 8 bytes, 8-aligned, empty
    /// (0) when the object is made, never moved, alive with the object.
    pub fn as_ptr(&self) -> *mut u64 {
        self.raw.as_ptr()
    }

    /// The embedder's word, in a heap no engine drives: 0 until set. It
    /// moves with the object (a scavenge copies it).
    ///
    /// # Panics
    ///
    /// In a driven heap, whose bytes are the engine's reference.
    pub fn embedder_word(&self, cx: &Mutation<'_>) -> u64 {
        assert!(
            !cx.inner.is_driven(),
            "a driven heap's JS edge is its engine's"
        );
        self.raw.load(Ordering::Relaxed)
    }

    /// Sets the embedder's word, in a heap no engine drives.
    ///
    /// # Panics
    ///
    /// In a driven heap, whose bytes are the engine's reference.
    pub fn set_embedder_word(&self, cx: &Mutation<'_>, word: u64) {
        assert!(
            !cx.inner.is_driven(),
            "a driven heap's JS edge is its engine's"
        );
        self.raw.store(word, Ordering::Relaxed);
    }
}

// SAFETY: its bytes are the engine's, which the GC never reads, and
// `__write` writes the empty reference.
unsafe impl HeapField for JsEdge {
    type Init<'gc> = ();
    const KIND: FieldKind = FieldKind::JsEdge;

    unsafe fn __write<'gc>((): Self::Init<'gc>, at: *mut Self, _: &Mutation<'gc>) {
        // SAFETY: as the caller promises.
        unsafe {
            at.write(Self {
                raw: AtomicU64::new(0),
            })
        };
    }
}

impl std::fmt::Debug for JsEdge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JsEdge")
    }
}

/// `T`'s number in the process's table of GC types (cppgc's `GCInfoIndex`),
/// the same for the process's life, below `MAX_TYPES`: an engine tells its
/// wrappers' types apart by it (a tag of its own for each).
#[inline]
pub fn type_index<T: HeapType>() -> u32 {
    T::__type().index()
}

/// The JS edges of the object at `object` (an engine finding the one that
/// refers to a wrapper it dropped, as V8's `EmbedderRootsHandler`).
///
/// # Safety
///
/// `object` is a live object of a heap (an engine's wrapper's, which
/// `Mutation::engine_address` gave), no GC of which runs or frees it
/// meanwhile.
pub unsafe fn js_edges_of(object: NonNull<u8>) -> impl Iterator<Item = NonNull<JsEdge>> {
    // SAFETY: as the caller promises: a live object's header, whose type's
    // index is its first word, written before the object was published.
    let index = unsafe { object.as_ptr().sub(HEADER_BYTES).cast::<u32>().read() };
    // SAFETY: a live object's type.
    let ty = unsafe { types::entry(index) };
    ty.js_edges.iter().map(move |&at| {
        // SAFETY: a field of the live object, by its type's layout.
        unsafe { object.add(at as usize).cast() }
    })
}
