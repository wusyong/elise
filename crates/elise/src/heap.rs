//! The heap: a cage of size-class pages and large objects (the old
//! generation) and young pages (the nursery), and at safepoints (the start
//! of `Heap::mutate`, `Heap::scavenge` and `Heap::collect`), stop-the-world:
//! a scavenge of the nursery, or a full GC, a scavenge that promotes every
//! survivor and then a mark-sweep of the old generation.
//!
//! A new object is bumped into the nursery's pages, objects of any size
//! together, up to the nursery's size; past it, and for objects larger
//! than a slot, it goes to the old generation (no GC runs in an operation).
//! A scavenge copies what the roots and the slot sets reach (`young`).
//!
//! Allocation bumps through a free range of a page of the size class: the
//! runs of free slots the last sweep found. When the range runs out, the
//! next comes from the same page, then from the class's page with the
//! fewest free slots (mimalloc's choice: nearly empty pages get a chance
//! to empty), then from the pool of empty pages, then from a new page.
//!
//! A page the sweep finds empty, and a dead large object's pages, go to the
//! pool, which every class and large object takes from; a page still there
//! a whole GC later is discarded, its memory given back to the OS: by a
//! worker, where the platform has some (`discard`).

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::marker::PhantomData;
use std::ptr::{self, NonNull};
use std::time::{Duration, Instant};

use crate::asan;
use crate::cage::{self, Cage};
use crate::page::{self, HEADER_BYTES, LargeObject, MarkTable, PAGE_BYTES, PAGES, Page, SlotSet};
use crate::persistent::RootTable;
use crate::platform::{self, JobHandle, Platform};
use crate::site::{self, Site, SiteStats, Sites};
use crate::sync::{self, Ordering};
use crate::types::{self, Finalizer, MAX_OBJECT_BYTES, TypeInfo};

pub(crate) mod concurrent;
mod discard;
mod fragmentation;
mod schedule;
mod sweep;
mod verify;
mod young;
pub use fragmentation::Fragmentation;

use crate::{GcObject, HeapField, HeapType, ObjectInit};

/// Bytes written over a dead object's data in debug builds: a stale pointer
/// reads it rather than an old object.
pub(super) const POISON: u8 = 0xdb;

#[derive(Clone, Copy, Debug)]
pub struct HeapOptions {
    /// GC at every safepoint: by turns a scavenge and a full GC, or, where
    /// marking runs alongside, a scavenge and a marking's start or end.
    pub stress: bool,
    /// What may be allocated in the old generation (promoted too) after a
    /// full GC before the next, at least: the next comes after as much as
    /// the last one kept (the heap grows to twice that), or this. cppgc's
    /// `HeapGrowing` grows to 1.5 times, by 640 KiB at least, but most of
    /// its marking is concurrent and every allocation counts there; a full
    /// GC here stops the world, and what dies young doesn't count (at 640
    /// KiB the new heap still ran full GCs where V8 ran none). V8's JS heap
    /// grows by 1.1 to 4 times, as its GC's and the mutator's speeds say.
    pub min_allocation_bytes: usize,
    /// While a concurrent marking runs, how far past that the old
    /// generation may grow before the pause finishes the marking, at
    /// least: half the limit, or this (V8's `kMinOvershoot`, in its
    /// `old_generation_overshoot_margin`), and at most half of what the
    /// cage has left past the limit, as the marking starts. Without it the
    /// workers rarely finished where all survives, and the pause marked the
    /// rest.
    pub min_overshoot_bytes: usize,
    /// Check the heap after every GC (V8's `--verify-heap`): it walks the
    /// whole heap again. `stress` does too, but under Miri.
    pub verify: bool,
    /// The nursery's size at first: what is allocated in it before a
    /// scavenge, in whole pages. 0 for none: every object goes to the old
    /// generation.
    pub nursery_bytes: usize,
    /// The largest the nursery grows to (V8's: it doubles each time as
    /// much as it holds has survived scavenges, and halves at a full GC
    /// after scavenges of garbage).
    pub max_nursery_bytes: usize,
    /// Measure the old generation's fragmentation each time a sweep is
    /// done and a marking starts (`HeapStats::fragmentation`,
    /// `fragmentation_at_marking`): it walks the pages.
    pub measure_fragmentation: bool,
    /// A JS engine drives the old generation's marking (a heap whose
    /// objects V8's wrappers stand for; cppgc's `CppHeap` attached to an
    /// isolate): it never starts or finishes one, nor a full GC, by
    /// itself; the engine does (`Heap::start_marking`, `advance_marking`,
    /// `finish_marking`), as the heap's size it is told grows
    /// (`Heap::used_bytes`). Scavenges are the heap's own.
    pub driven: bool,
}

impl Default for HeapOptions {
    fn default() -> Self {
        Self {
            stress: false,
            min_allocation_bytes: 2 << 20,
            min_overshoot_bytes: 32 << 20,
            verify: false,
            nursery_bytes: 1 << 20,
            max_nursery_bytes: 16 << 20,
            measure_fragmentation: false,
            driven: false,
        }
    }
}

/// What the heap holds and has done.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapStats {
    /// Full GCs.
    pub gcs: u64,
    pub scavenges: u64,
    /// Bytes of the old slots in use (headers included), of large objects
    /// (rounded up to the OS's pages), and of the nursery's objects. While
    /// a sweep runs alongside the mutator, the old generation's is what it
    /// was before the GC (garbage too) and what was allocated since; the
    /// sweep, once done, says what is live.
    pub used_bytes: usize,
    /// The nursery's part of `used_bytes`.
    pub young_bytes: usize,
    /// The nursery's size now.
    pub nursery_bytes: usize,
    /// Bytes holding memory: pages in use, pages in the pool not discarded
    /// yet, and large objects' committed bytes.
    pub committed_bytes: usize,
    /// The last full GC.
    pub last_gc: GcRecord,
    /// The last scavenge, on its own or a full GC's.
    pub last_scavenge: ScavengeRecord,
    /// All the GCs' pauses together, scavenges' too.
    pub gc_ns: u64,
    /// Objects allocated in the old generation because their site was
    /// tenured.
    pub pretenured_objects: u64,
    /// Chunks the id table holds (`Mutation::new_id`).
    pub id_chunks: usize,
    /// Concurrent markings finished, and of those, the ones the old
    /// generation's limit finished before the workers were done (the rest
    /// of the marking in the pause).
    pub concurrent_markings: u64,
    pub markings_at_limit: u64,
    /// The workers' time marking alongside the mutator, all markings
    /// together (their runs in the pause aside).
    pub background_mark_ns: u64,
    /// The workers' time sweeping alongside the mutator, all GCs together
    /// (the rest swept in a pause aside).
    pub background_sweep_ns: u64,
    /// The heap's thread's sweeping outside the pauses (a page for an
    /// allocation, the results of the workers' applied, the pages with
    /// finalizers), its time and its pages: GC time not in the trace.
    pub sweep_on_main_ns: u64,
    pub pages_swept_on_main: u64,
    /// The free ranges allocation in the old generation took (its slow
    /// path: the heap's thread's and a scavenge's threads'), and their
    /// slots: few to a range is a fragmented heap's allocation. A range a
    /// scavenge's thread gave back unused counts again when taken again.
    pub old_ranges: u64,
    pub old_range_slots: u64,
    /// With `HeapOptions::measure_fragmentation`, the old generation's
    /// pages when the last sweep was done, and when the last marking
    /// started.
    pub fragmentation: Fragmentation,
    pub fragmentation_at_marking: Fragmentation,
}

/// What a scavenge did (V8's `GCTracer`'s scavenge events): its time, and
/// the objects it copied within the nursery (survived) and to the old
/// generation (promoted), headers included.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScavengeRecord {
    pub ns: u64,
    pub survived_objects: usize,
    pub survived_bytes: usize,
    pub promoted_objects: usize,
    pub promoted_bytes: usize,
    /// Dead objects of the nursery whose finalizers it ran.
    pub finalized: usize,
    /// Copies a thread made of an object another thread copied first (and
    /// gave back, or left as garbage).
    pub copies_abandoned: usize,
    /// Copies its workers scanned and slot sets' pages they updated (the
    /// rest, the heap's thread).
    pub scanned_by_workers: usize,
    /// Slot sets' pages its workers updated.
    pub slot_pages_by_workers: usize,
}

/// What a GC did and how long it took (cppgc's `StatsCollector`, V8's
/// `GCTracer`): its pause (the whole stop-the-world), and the marking,
/// sweeping and finalizing in it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcRecord {
    pub pause_ns: u64,
    pub mark_ns: u64,
    pub sweep_ns: u64,
    pub finalize_ns: u64,
    /// The objects it marked, and the bytes of their slots (headers
    /// included) and large objects: what it kept (once its sweep is done,
    /// as its sweep finds them; 0 until then).
    pub marked_objects: usize,
    pub marked_bytes: usize,
    /// Objects its workers scanned (the rest, the heap's thread).
    pub scanned_by_workers: usize,
    /// Its marking ran alongside the mutator: `mark_ns` is its end's, in
    /// the pause, and `start_pause_ns` its start's pause.
    pub concurrent: bool,
    pub start_pause_ns: u64,
    /// Of `marked_objects`, those scanned in the pause (all, but for a
    /// concurrent marking).
    pub marked_in_pause: usize,
    /// Of `marked_objects`, those the heap's thread scanned in the steps
    /// of a marking an engine drives.
    pub marked_in_steps: usize,
}

/// Nanoseconds since `start`, at most `u64::MAX` (585 years).
fn elapsed_ns(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

pub struct Heap {
    inner: Inner,
}

/// The root tables a `Persistent` and a `WeakPersistent` hold by address
/// (the weak roots' apart, as cppgc's weak persistent region: what keeps
/// objects alive is walked without them), in an allocation of their own
/// so that they don't move with the heap.
#[derive(Default)]
pub(crate) struct Shared {
    pub(crate) roots: RootTable,
    pub(crate) weak_roots: RootTable,
}

/// The heap's `Shared`, owned without `Box`'s claim that no other pointer
/// reaches it: every root holds one more, which each move of a `Box` (the
/// heap moves with whatever holds it) would invalidate, a unique retag
/// under Stacked Borrows. As `OwnedMarks`.
pub(crate) struct OwnedShared(NonNull<Shared>);

impl OwnedShared {
    fn new() -> Self {
        Self(NonNull::from(Box::leak(Box::<Shared>::default())))
    }

    /// Lets the tables outlive the heap (a root left by mistake still
    /// writes into them).
    fn leak(self) {
        std::mem::forget(self);
    }
}

impl std::ops::Deref for OwnedShared {
    type Target = Shared;

    fn deref(&self) -> &Shared {
        // SAFETY: a `Box`'s allocation, freed only when this drops; only
        // shared references are made of it.
        unsafe { self.0.as_ref() }
    }
}

impl Drop for OwnedShared {
    fn drop(&mut self) {
        // SAFETY: made by `Box::leak` in `new`, and freed once, here: the
        // heap's drop leaks it instead while a root holds it.
        drop(unsafe { Box::from_raw(self.0.as_ptr()) });
    }
}

pub(crate) struct Inner {
    pub(crate) shared: std::mem::ManuallyDrop<OwnedShared>,
    pub(crate) cage: Cage,
    space: RefCell<Space>,
    /// For each size class, the range being bumped through.
    cursors: Vec<Cell<Cursor>>,
    /// By page number, whether it is a young page: what the write barrier
    /// reads (V8's page flag), set and cleared only at safepoints (by a
    /// scavenge's threads too, for the to-space pages they take).
    young_pages: std::sync::Arc<[std::sync::atomic::AtomicBool]>,
    /// By page number, the old page's (or large object's, at its first
    /// page) slot set.
    remembered: Box<[SlotSet]>,
    /// The same for weak edges (`WeakMember`), apart: a scavenge updates
    /// them after it copied what the strong ones keep, and clears those
    /// whose object died.
    remembered_weak: Box<[SlotSet]>,
    /// The same for ephemerons' keys (`EphemeronPair`), apart again: a
    /// scavenge clears an ephemeron's value with its key.
    remembered_ephemeron: Box<[SlotSet]>,
    /// By page number, the mark bits of each page in use, for the threads
    /// that mark.
    mark_table: std::sync::Arc<MarkTable>,
    young: RefCell<young::Nursery>,
    /// The id table (`Mutation::new_id`).
    pub(super) ids: RefCell<crate::ids::IdTable>,
    /// The marking running is one the engine drives.
    driven_marking: Cell<bool>,
    /// The sweep alongside the mutator, and its job's handle while posted.
    sweeper: std::sync::Arc<sweep::Sweeper>,
    sweep_job: RefCell<Option<Box<dyn JobHandle>>>,
    /// The last GC's sweep isn't done: pages wait for it, or for their
    /// results to be applied.
    sweeping: Cell<bool>,
    /// The pages with objects with drop glue, for the heap's thread to
    /// sweep at the safepoint after the pause, and the dead large objects
    /// with drop glue, to finalize and free then.
    main_pages: RefCell<Vec<usize>>,
    dead_large: RefCell<Vec<usize>>,
    /// Debug builds: the dead objects of `main_pages`, poisoned once their
    /// pages are swept (their finalizers run).
    poison_later: RefCell<Vec<(usize, usize)>>,
    sweep_on_main_ns: Cell<u64>,
    pages_swept_on_main: Cell<u64>,
    /// The live bytes the sweep has found so far (the old generation's
    /// marked large objects', then each page's as its result is applied),
    /// and the last sweep's, once done: what the heap uses, and the next
    /// limit.
    swept_live: Cell<usize>,
    last_live: Cell<usize>,
    fragmentation: Cell<Fragmentation>,
    fragmentation_at_marking: Cell<Fragmentation>,
    /// The pages a worker gives back to the OS, and its job's handle once
    /// posted.
    discards: std::sync::Arc<discard::Discards>,
    discard_job: RefCell<Option<Box<dyn JobHandle>>>,
    /// The concurrent marking running, if any.
    marking: RefCell<Option<concurrent::Marking>>,
    /// Whether one runs: what the barrier looks at.
    marking_active: Cell<bool>,
    /// A marking the engine drives runs: what enters the old generation
    /// meanwhile (black, never scanned) is counted in `black_bytes`, the
    /// live bytes' share the marker doesn't scan.
    counts_live: Cell<bool>,
    black_bytes: Cell<usize>,
    /// The platform runs posted jobs: marking can run alongside.
    concurrent: bool,
    /// The last concurrent marking's start's pause.
    start_pause_ns: Cell<u64>,
    /// The nursery's page being bumped through.
    young_cursor: Cell<Cursor>,
    /// `Mutation::pretenured`s running: allocations go to the old
    /// generation.
    pretenure: Cell<u32>,
    /// What it counted and decided of the allocation sites.
    sites: RefCell<Sites>,
    /// Allocations a tenured site sent to the old generation.
    pretenured: Cell<u64>,
    /// Bytes of the nursery's objects.
    young_used: Cell<usize>,
    /// What a large object's commit is rounded up to.
    granularity: usize,
    allocated: Cell<usize>,
    used: Cell<usize>,
    /// The next GC once this much is allocated after the last.
    limit: Cell<usize>,
    /// How far past `limit` the old generation may grow while a marking
    /// runs: set as it starts, fixed until it ends, as V8's
    /// `old_generation_overshoot_margin` is (from its allocation limit and
    /// maximum size, not what is used as the marking goes).
    overshoot: Cell<usize>,
    options: HeapOptions,
    platform: std::sync::Arc<dyn Platform>,
    gcs: Cell<u64>,
    last_gc: Cell<GcRecord>,
    scavenges: Cell<u64>,
    last_scavenge: Cell<ScavengeRecord>,
    gc_ns: Cell<u64>,
    /// `HeapStats`'s counts of concurrent markings.
    concurrent_markings: Cell<u64>,
    markings_at_limit: Cell<u64>,
    background_mark_ns: Cell<u64>,
    /// Under `stress`, the safepoints so far: each scavenges, or runs a full
    /// GC (or a concurrent marking's start or end), by turns.
    stress_step: Cell<u8>,
}

/// The pages: what each is, and what is free.
#[derive(Default)]
struct Space {
    /// By page number, every page used so far (the rest of the cage was
    /// never committed).
    states: Vec<PageState>,
    /// By page number, its bytes holding memory.
    committed: Vec<usize>,
    committed_total: usize,
    /// For each size class, its pages with free slots but the current one,
    /// the fewest free last (taken with `pop`).
    available: Vec<Vec<usize>>,
    /// Free pages holding memory, and discarded ones. Either may list a
    /// page taken since by a large object: it is skipped when found.
    pool: Vec<usize>,
    discarded: Vec<usize>,
    /// `HeapStats::old_ranges` and `old_range_slots`.
    ranges: u64,
    range_slots: u64,
}

enum PageState {
    /// Page 0: offset 0 is null.
    Null,
    /// In the pool, or discarded: since the GC numbered `since` (0 for one
    /// a large object never took).
    Free {
        since: u64,
    },
    Small(Page),
    Large(LargeObject),
    /// The nursery's: objects of any size, one after another from its
    /// start up to `top` (an offset; the page being bumped through has its
    /// end in the cursor).
    Young {
        top: usize,
    },
    /// A large object's page after its first.
    LargeTail,
    /// Free, handed to the worker that gives its memory back to the OS:
    /// no thread takes it until a safepoint finds it done.
    Discarding,
}

/// A range of free slots of a class's current page, by data offset: the
/// next slot's, and the one past the range. Empty for no page.
#[derive(Clone, Copy, Default)]
struct Cursor {
    next: usize,
    limit: usize,
    page: usize,
}

/// An operation on the heap: what allocates and reads and writes edges. No
/// GC runs while one exists; `'gc` brands the pointers it hands out.
pub struct Mutation<'gc> {
    pub(crate) inner: &'gc Inner,
    _invariant: PhantomData<Cell<&'gc ()>>,
}

/// An object being initialized by `alloc`, for `GcObject::__init`.
#[doc(hidden)]
pub struct InitSlot<'a, 'gc, T> {
    ptr: *mut T,
    cx: &'a Mutation<'gc>,
}

impl<'gc, T> InitSlot<'_, 'gc, T> {
    /// Writes the field of type `F` at `offset`.
    pub fn field<F: HeapField>(&self, offset: usize, init: F::Init<'gc>) {
        // SAFETY: `GcObject`'s contract: `offset` is that of a field of type
        // `F` in `T`, so the place is in the object being initialized,
        // aligned, and not written yet.
        unsafe { F::__write(init, self.ptr.cast::<u8>().add(offset).cast::<F>(), self.cx) }
    }
}

impl Default for Heap {
    fn default() -> Self {
        Self::new(HeapOptions::default())
    }
}

impl Heap {
    /// A heap whose GCs mark on the process's pool of workers.
    pub fn new(options: HeapOptions) -> Self {
        Self::with_platform(options, platform::default_platform())
    }

    /// A heap whose GCs mark on `platform`'s workers.
    pub fn with_platform(options: HeapOptions, platform: std::sync::Arc<dyn Platform>) -> Self {
        let cage = Cage::new();
        let discards = std::sync::Arc::new(discard::Discards::new(cage.discarder()));
        let mark_table = std::sync::Arc::new(MarkTable::new());
        let sweeper =
            std::sync::Arc::new(sweep::Sweeper::new(mark_table.clone(), Base(cage.base())));
        Self {
            inner: Inner {
                shared: std::mem::ManuallyDrop::new(OwnedShared::new()),
                cage,
                space: RefCell::new(Space {
                    states: vec![PageState::Null],
                    committed: vec![0],
                    available: vec![Vec::new(); page::CLASSES.len()],
                    ..Space::default()
                }),
                cursors: vec![Cell::default(); page::CLASSES.len()],
                young_pages: (0..PAGES)
                    .map(|_| std::sync::atomic::AtomicBool::new(false))
                    .collect::<Vec<_>>()
                    .into(),
                remembered: (0..PAGES).map(|_| SlotSet::default()).collect(),
                remembered_weak: (0..PAGES).map(|_| SlotSet::default()).collect(),
                remembered_ephemeron: (0..PAGES).map(|_| SlotSet::default()).collect(),
                mark_table,
                ids: RefCell::new(crate::ids::IdTable::default()),
                driven_marking: Cell::new(false),
                sweeper,
                sweep_job: RefCell::new(None),
                sweeping: Cell::new(false),
                main_pages: RefCell::new(Vec::new()),
                dead_large: RefCell::new(Vec::new()),
                poison_later: RefCell::new(Vec::new()),
                sweep_on_main_ns: Cell::new(0),
                pages_swept_on_main: Cell::new(0),
                swept_live: Cell::new(0),
                last_live: Cell::new(0),
                fragmentation: Cell::new(Fragmentation::default()),
                fragmentation_at_marking: Cell::new(Fragmentation::default()),
                discards,
                discard_job: RefCell::new(None),
                marking: RefCell::new(None),
                marking_active: Cell::new(false),
                counts_live: Cell::new(false),
                black_bytes: Cell::new(0),
                concurrent: platform.posts_jobs(),
                start_pause_ns: Cell::new(0),
                young: RefCell::new(young::Nursery::new(
                    options.nursery_bytes,
                    options.max_nursery_bytes,
                )),
                young_cursor: Cell::new(Cursor::default()),
                pretenure: Cell::new(0),
                sites: RefCell::new(Sites::default()),
                pretenured: Cell::new(0),
                young_used: Cell::new(0),
                granularity: cage::commit_granularity(),
                allocated: Cell::new(0),
                used: Cell::new(0),
                limit: Cell::new(options.min_allocation_bytes),
                overshoot: Cell::new(0),
                options,
                platform,
                gcs: Cell::new(0),
                last_gc: Cell::new(GcRecord::default()),
                scavenges: Cell::new(0),
                last_scavenge: Cell::new(ScavengeRecord::default()),
                gc_ns: Cell::new(0),
                concurrent_markings: Cell::new(0),
                markings_at_limit: Cell::new(0),
                background_mark_ns: Cell::new(0),
                stress_step: Cell::new(0),
            },
        }
    }

    /// Runs one operation: a safepoint first (a full GC if enough was
    /// promoted or allocated in the old generation since the last, else a
    /// scavenge if the nursery is full; under `stress`, one of them every
    /// time), then `f`. What `f` returns can't hold its `'gc`: nothing of
    /// the heap outlives it but `Persistent`s.
    pub fn mutate<R>(&mut self, f: impl for<'gc> FnOnce(&Mutation<'gc>) -> R) -> R {
        self.inner.safepoint();
        f(&Mutation {
            inner: &self.inner,
            _invariant: PhantomData,
        })
    }

    /// Runs one operation with no safepoint first, as `mutate` without its
    /// GC's work: for one that allocates nothing (a DOM's getter), which
    /// can't make a GC due (Oilpan's GC starts only as one allocates). What
    /// `f` allocates anyway counts at the next `mutate`'s safepoint. Through
    /// a shared borrow: such operations may overlap (one in another's `f`),
    /// as none collects; a GC (`mutate`, `collect`, `scavenge`, a marking's
    /// steps) needs `&mut self`, so none runs while one of them does.
    pub fn read<R>(&self, f: impl for<'gc> FnOnce(&Mutation<'gc>) -> R) -> R {
        f(&Mutation {
            inner: &self.inner,
            _invariant: PhantomData,
        })
    }

    /// Whether `mutate` would do a GC's work first: a scavenge, a full GC,
    /// or a concurrent marking's start or end.
    pub fn should_collect(&self) -> bool {
        let inner = &self.inner;
        let marking = inner.marking.borrow().is_some();
        if inner.options.driven {
            return inner.young.borrow().full;
        }
        inner.options.stress
            || inner.at_limit(marking)
            || inner.young.borrow().full
            || (marking && inner.marking_done())
            || (!marking && inner.starts_marking())
    }

    /// Whether a concurrent marking runs.
    pub fn is_marking(&self) -> bool {
        self.inner.marking.borrow().is_some()
    }

    /// Whether the last full GC's sweep runs alongside the mutator: pages
    /// wait for it, or for their results to be applied at a safepoint.
    pub fn is_sweeping(&self) -> bool {
        self.inner.sweeping.get()
    }

    /// A full GC now, at this safepoint: a scavenge promoting every
    /// survivor, then a mark-sweep of the old generation, in this pause.
    /// A concurrent marking running is finished first: what died while it
    /// ran (its floating garbage) is collected too.
    ///
    /// # Panics
    ///
    /// In a heap the engine drives (`HeapOptions::driven`): its full GCs
    /// are the engine's, which alone knows what its wrappers keep.
    pub fn collect(&mut self) {
        assert!(
            !self.inner.options.driven,
            "the engine collects the heap it drives"
        );
        self.inner.take_discarded();
        if self.inner.marking.get_mut().is_some() {
            self.inner.drain_barrier();
            self.inner.finish_marking();
        }
        self.inner.collect(true);
    }

    /// A scavenge of the nursery now, at this safepoint.
    pub fn scavenge(&mut self) {
        self.inner.take_discarded();
        self.inner.sweep_at_safepoint();
        self.inner.scavenge_alone();
    }

    pub fn stats(&self) -> HeapStats {
        let young_bytes = self.inner.young_used.get();
        HeapStats {
            gcs: self.inner.gcs.get(),
            scavenges: self.inner.scavenges.get(),
            used_bytes: self.inner.used.get().saturating_add(young_bytes),
            young_bytes,
            nursery_bytes: self.inner.young.borrow().capacity_bytes(),
            committed_bytes: self.inner.space.borrow().committed_total,
            last_gc: self.inner.last_gc.get(),
            last_scavenge: self.inner.last_scavenge.get(),
            gc_ns: self.inner.gc_ns.get(),
            concurrent_markings: self.inner.concurrent_markings.get(),
            markings_at_limit: self.inner.markings_at_limit.get(),
            background_mark_ns: self.inner.background_mark_ns.get(),
            background_sweep_ns: self
                .inner
                .sweeper
                .background_ns
                .load(std::sync::atomic::Ordering::Relaxed),
            sweep_on_main_ns: self.inner.sweep_on_main_ns.get(),
            pages_swept_on_main: self.inner.pages_swept_on_main.get(),
            old_ranges: self.inner.space.borrow().ranges,
            old_range_slots: self.inner.space.borrow().range_slots,
            fragmentation: self.inner.fragmentation.get(),
            fragmentation_at_marking: self.inner.fragmentation_at_marking.get(),
            pretenured_objects: self.inner.pretenured.get(),
            id_chunks: self.inner.ids.borrow().chunks(),
        }
    }

    /// What it counted and decided of `site`.
    pub fn site(&self, site: &Site) -> SiteStats {
        self.inner.sites.borrow().stats(site.index())
    }

    /// Root-table entries in use.
    pub fn roots(&self) -> usize {
        self.inner.shared.roots.len()
    }

    /// The bytes the heap's objects use (`HeapStats::used_bytes`): what a
    /// heap the engine drives tells it as it grows (cppgc's
    /// `ReportBufferedAllocationSizeIfPossible`).
    pub fn used_bytes(&self) -> usize {
        self.inner
            .used
            .get()
            .saturating_add(self.inner.young_used.get())
    }

    /// Starts the old generation's marking for the engine that drives it
    /// (`HeapOptions::driven`), at this safepoint, as `Heap::mutate` would
    /// start one: a scavenge promoting every survivor, the roots marked, the
    /// job posted. The engine hands its wrappers' objects over as it marks
    /// them, through what this returns; the marker gives `js` the JS edges
    /// of what it scans.
    ///
    /// # Panics
    ///
    /// If the heap isn't driven, or a marking runs.
    pub fn start_marking(
        &mut self,
        js: std::sync::Arc<dyn crate::JsEdgeSink>,
    ) -> crate::WrapperMarker {
        let inner = &mut self.inner;
        assert!(
            inner.options.driven,
            "the engine drives a driven heap's marking"
        );
        assert!(inner.marking.get_mut().is_none(), "a marking runs");
        inner.take_discarded();
        inner.sweep_at_safepoint();
        inner.driven_marking.set(true);
        inner.start_marking_with(Some(js));
        let last = inner.last_gc.get();
        let marking = inner.marking.get_mut().as_mut().expect("a marking runs");
        marking.driven = Some(concurrent::Driven {
            started: Instant::now(),
            schedule: schedule::Schedule::new(last.marked_objects, last.marked_bytes),
            stepped: 0,
            before_pause: None,
        });
        crate::WrapperMarker {
            marker: marking.marker.clone(),
        }
    }

    /// A step of the marking the engine drives, between operations, of at
    /// most `budget`: the barrier's objects published, the heap's thread
    /// marking what its schedule says it is behind (cppgc's
    /// `AdvanceMarkingWithLimits`), and the workers told of what the
    /// engine handed over. `atomic`: the engine's pause, which calls it
    /// again while either side finds more; what is left is marked here,
    /// with the platform's threads. Whether nothing is left.
    ///
    /// # Panics
    ///
    /// If no marking the engine drives runs.
    pub fn advance_marking(&mut self, atomic: bool, budget: Duration) -> bool {
        let started = Instant::now();
        let inner = &mut self.inner;
        assert!(
            inner.driven_marking.get(),
            "a marking the engine drives runs"
        );
        inner.drain_barrier();
        let platform = inner.platform.clone();
        let used = inner.used.get().saturating_add(inner.young_used.get());
        let marking = inner.marking.get_mut().as_mut().expect("a marking runs");
        let marker = marking.marker.clone();
        let driven = marking.driven.as_mut().expect("a driven marking's state");
        let marked = marker.scanned.load(Ordering::Relaxed);
        // In the engine's trace, inside its step or pause (V8's
        // `MC_INCREMENTAL_EMBEDDER_TRACING`, `MC_MARK_EMBEDDER_TRACING`).
        if atomic {
            platform.trace_begin("Gc.Mark");
            driven.before_pause.get_or_insert(marked);
            marking.cancel();
            marker.worklist.reseed(Vec::new());
            marker.finish(&*platform);
        } else {
            platform.trace_begin("Gc.Step");
            let limit = driven.schedule.step(driven.started.elapsed(), used, marked);
            if limit > 0 {
                let scanned = marker.step(limit, started.checked_add(budget));
                driven.stepped = driven.stepped.saturating_add(scanned);
            }
            marking.notify();
        }
        platform.trace_end();
        inner.marking_done()
    }

    /// Finishes the marking the engine drives, in its pause, once both
    /// sides are done: the weak edges to the dead cleared, the old
    /// generation swept (alongside the mutator, as a concurrent marking's).
    /// The dead objects' JS edges die with them: the engine already let go
    /// of what it didn't mark. The bytes the heap uses after it: the live
    /// ones (and the young generation's), not the dead the sweep has yet to
    /// free, for the engine to set its next limit from.
    ///
    /// # Panics
    ///
    /// If no marking the engine drives runs, or it isn't done.
    pub fn finish_marking(&mut self) -> usize {
        let inner = &mut self.inner;
        assert!(
            inner.driven_marking.get(),
            "a marking the engine drives runs"
        );
        inner.drain_barrier();
        assert!(
            inner.marking_done(),
            "the engine finishes a marking once it is done"
        );
        inner.finish_marking();
        inner.driven_marking.set(false);
        self.used_bytes()
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        // A marking's job reads the cage, and the discards' maps it: they
        // stop first.
        if let Some(marking) = self.inner.marking.get_mut() {
            marking.cancel();
            // An engine's threads may still hold the marker (its isolate
            // torn down while it marks): they touch the pages no more.
            marking.marker.retire();
        }
        if let Some(job) = self.inner.discard_job.get_mut().take() {
            job.cancel();
        }
        // The sweep's reads the marks of the pages the heap frees next.
        if let Some(job) = self.inner.sweep_job.get_mut().take() {
            job.cancel();
        }
        // Every object with drop glue is finalized; nothing else is read.
        let mut live: Vec<usize> = self.inner.young.get_mut().finalizable.clone();
        for (number, state) in self.inner.space.get_mut().states.iter().enumerate() {
            match state {
                PageState::Small(page) => live.extend(
                    page.finalizable()
                        .into_iter()
                        .map(|slot| page.data_of(slot)),
                ),
                PageState::Large(object) if object.is_finalizable() => {
                    live.push(large_data(number));
                }
                _ => {}
            }
        }
        for data in live {
            // SAFETY: a whole object with drop glue (its finalizable bit is
            // set once it is whole), finalized once: the heap goes.
            unsafe { self.inner.finalize(data) };
        }
        // The next cage may be mapped where this one was.
        let pages = self.inner.space.get_mut().states.len();
        asan::unpoison(self.inner.ptr(0), pages.saturating_mul(PAGE_BYTES));
        // A root outlives its heap only by mistake (Blink's
        // `PersistentRegion` asserts the same): its drop would write into
        // the tables, which are left to it.
        let left = self
            .inner
            .shared
            .roots
            .len()
            .saturating_add(self.inner.shared.weak_roots.len());
        // SAFETY: the heap's own, taken once, here, as it goes: nothing
        // reads `shared` of it after.
        let shared = unsafe { std::mem::ManuallyDrop::take(&mut self.inner.shared) };
        if left > 0 {
            // Left first: the check's panic unwinds through the roots' drops.
            shared.leak();
            debug_assert!(std::thread::panicking(), "{left} roots outlive their heap");
        }
    }
}

impl<'gc> Mutation<'gc> {
    /// Runs `f`, its allocations straight in the old generation (V8's
    /// `AllocationType::kOld`): for objects the caller knows to live long,
    /// as a parser's first document, which the nursery would only copy.
    pub fn pretenured<R>(&self, f: impl FnOnce(&Self) -> R) -> R {
        /// Ends the pretenuring, though `f` panic.
        struct Pretenuring<'a>(&'a Cell<u32>);
        impl Drop for Pretenuring<'_> {
            fn drop(&mut self) {
                self.0.set(self.0.get().saturating_sub(1));
            }
        }
        let depth = &self.inner.pretenure;
        depth.set(depth.get().saturating_add(1));
        let _pretenuring = Pretenuring(depth);
        f(self)
    }

    /// The next id of the heap's id table, for `object`: `by_id` finds it
    /// while it lives, then nothing (ids are never given again).
    ///
    /// # Panics
    ///
    /// Once 2^32 - 1 ids were given.
    pub fn new_id<T: HeapType>(&self, object: crate::Gc<'gc, T>) -> crate::Id {
        let offset = crate::member::compress(self, Some(object));
        let young = self.inner.is_young(offset as usize);
        self.inner.ids.borrow_mut().insert(offset, young)
    }

    /// The object `id` was given to, if it lives and is a `T` (any object
    /// for `Erased`). Read while a concurrent marking runs, it is marked,
    /// as through a weak edge.
    pub fn by_id<T: HeapType>(&self, id: crate::Id) -> Option<crate::Gc<'gc, T>> {
        let offset = self.inner.ids.borrow().get(id);
        if offset == 0 {
            return None;
        }
        // SAFETY: a live object's header (the table's entries are cleared
        // as their objects die), in a committed page.
        let index = unsafe { self.inner.header(offset as usize).read() };
        let erased = std::ptr::eq(T::__type(), <crate::Erased as HeapType>::__type());
        if !erased && index != T::__type().index() {
            return None;
        }
        self.inner.read_weak(offset);
        // SAFETY: a live object of `cx`'s heap, a `T` (its header says so,
        // or `T` is `Erased`), marked just now if a marking runs.
        Some(unsafe {
            crate::Gc::from_raw(NonNull::new_unchecked(
                self.inner.ptr(offset as usize).cast(),
            ))
        })
    }

    /// `object`'s address, for a JS engine's wrapper to hold
    /// (`HeapOptions::driven`): a `T` has a `JsEdge`, so it is old and
    /// never moves. The engine hands it over as it marks the wrapper
    /// (`WrapperLocal::hand`), and reads it back with `from_engine`.
    ///
    /// # Panics
    ///
    /// If the heap isn't driven, or a `T` has no `JsEdge`.
    pub fn engine_address<T: HeapType>(&self, object: crate::Gc<'gc, T>) -> NonNull<u8> {
        assert!(
            self.inner.options.driven,
            "an engine holds a driven heap's objects"
        );
        assert!(
            !types::entry(T::__type().index()).js_edges.is_empty(),
            "an object an engine holds has a JS edge"
        );
        object.as_non_null().cast()
    }

    /// The object at `address`, an engine's wrapper's. Read while a
    /// marking runs, it is marked, as through a weak edge: the engine may
    /// drop the wrapper before it hands the object over.
    ///
    /// # Safety
    ///
    /// `address` is what `engine_address` gave of a `T` of this heap, which
    /// lives (the wrapper holding it keeps it alive).
    ///
    /// # Panics
    ///
    /// If `address` isn't in this heap's cage (another heap's object).
    pub unsafe fn from_engine<T: HeapType>(&self, address: NonNull<u8>) -> crate::Gc<'gc, T> {
        let offset = self.inner.offset_of(address.as_ptr());
        assert!(offset < cage::CAGE_BYTES, "an object of this heap");
        #[expect(
            clippy::cast_possible_truncation,
            reason = "an object's offset in the 4 GiB cage"
        )]
        self.inner.read_weak(offset as u32);
        // SAFETY: as the caller promises: a live `T` of this heap, marked
        // just now if a marking runs.
        unsafe { crate::Gc::from_raw(address.cast()) }
    }

    /// A new object, written in place from `init`.
    ///
    /// # Panics
    ///
    /// If the cage is full: no GC runs in an operation.
    pub fn alloc<I: ObjectInit<'gc>>(&self, init: I) -> crate::Gc<'gc, I::Object> {
        self.alloc_at(0, init)
    }

    /// A new object from `site`, written in place from `init`: in the old
    /// generation if the heap decided the site's objects live long.
    ///
    /// # Panics
    ///
    /// As `alloc`.
    pub fn alloc_in<I: ObjectInit<'gc>>(&self, site: &Site, init: I) -> crate::Gc<'gc, I::Object> {
        self.alloc_at(site.index(), init)
    }

    /// A new object from the site numbered `site` (0 for none).
    fn alloc_at<I: ObjectInit<'gc>>(&self, site: u32, init: I) -> crate::Gc<'gc, I::Object> {
        type T<'gc, I> = <I as ObjectInit<'gc>>::Object;
        let ty = <T<'gc, I>>::__type();
        let index = ty.index();
        // An object with a JS edge is old in a heap an engine drives: the
        // engine's reference to it never moves.
        let old = self.inner.options.driven && !types::entry(index).js_edges.is_empty();
        let data = self.inner.allocate(ty.info().size() as usize, site, old);
        let ptr = self.inner.ptr(data).cast::<T<'gc, I>>();
        <T<'gc, I>>::__init(init, InitSlot { ptr, cx: self });
        self.inner.publish(data, index, site, ty.info().finalizer());
        // SAFETY: `ptr` is the object just initialized, in the cage, which
        // no GC changes before `'gc` ends.
        unsafe { crate::Gc::from_raw(NonNull::new_unchecked(ptr)) }
    }
}

impl Inner {
    /// Whether a JS engine drives the heap (`HeapOptions::driven`).
    pub(crate) fn is_driven(&self) -> bool {
        self.options.driven
    }

    pub(crate) fn ptr(&self, offset: usize) -> *mut u8 {
        debug_assert!(offset < crate::cage::CAGE_BYTES);
        self.cage.base().wrapping_add(offset)
    }

    pub(crate) fn offset_of(&self, ptr: *const u8) -> usize {
        ptr.addr().wrapping_sub(self.cage.base().addr())
    }

    /// The header of the object (or free slot) whose data is at `data`:
    /// its type's index, then its flags.
    fn header(&self, data: usize) -> *mut u32 {
        self.ptr(data).wrapping_sub(HEADER_BYTES).cast()
    }

    /// Room for an object with `size` bytes of data from the site numbered
    /// `site` (0 for none): its data's offset, in the nursery if it has
    /// room, the object fits a slot and nothing pretenures it (`old`, the
    /// site, or `Mutation::pretenured`), else in the old generation. It is free
    /// to the next GC until `publish`, once it is whole: were writing it to
    /// panic, a sweep would neither scan nor finalize it, only free it
    /// again, and a scavenge wouldn't reach it.
    pub(crate) fn allocate(&self, size: usize, site: u32, old: bool) -> usize {
        let bytes = size
            .checked_add(HEADER_BYTES)
            .filter(|&bytes| bytes <= MAX_OBJECT_BYTES)
            .expect("an object fits a quarter of the cage");
        let mut sites = (site != 0).then(|| self.sites.borrow_mut());
        let tenured = sites.as_ref().is_some_and(|sites| sites.is_tenured(site));
        if tenured {
            self.pretenured.set(self.pretenured.get().saturating_add(1));
        }
        if bytes <= page::MAX_SLOT_BYTES
            && !old
            && self.pretenure.get() == 0
            && !tenured
            && let Some(data) = self.allocate_young(bytes)
        {
            if let Some(sites) = &mut sites {
                sites.created(site);
            }
            return data;
        }
        if let Some(sites) = &mut sites {
            sites.entered_old(site);
        }
        self.allocate_old(bytes)
    }

    /// Room in the old generation for an object of `bytes`, header
    /// included: its data's offset.
    fn allocate_old(&self, bytes: usize) -> usize {
        let (data, bytes) = match page::class_for(bytes) {
            Some(class) => (self.allocate_small(class), page::CLASSES[class]),
            None => self.allocate_large(bytes),
        };
        self.allocated
            .set(self.allocated.get().saturating_add(bytes));
        self.used.set(self.used.get().saturating_add(bytes));
        if self.counts_live.get() {
            self.black_bytes
                .set(self.black_bytes.get().saturating_add(bytes));
        }
        data
    }

    fn allocate_small(&self, class: usize) -> usize {
        let cursor = &self.cursors[class];
        let mut range = cursor.get();
        if range.next >= range.limit {
            range = self.refill(class);
        }
        let data = range.next;
        // The range is in one page, so next + slot_bytes is at most its end.
        range.next = data.saturating_add(page::CLASSES[class]);
        cursor.set(range);
        asan::unpoison(self.header(data).cast(), page::CLASSES[class]);
        data
    }

    /// A new range for `class`'s cursor.
    fn refill(&self, class: usize) -> Cursor {
        let mut space = self.space.borrow_mut();
        let page = self.cursors[class].get().page;
        let ranged = matches!(space.states.get(page), Some(PageState::Small(page)) if !page.ranges.is_empty());
        if self.sweeping.get() && !ranged && space.available[class].is_empty() {
            self.sweep_for(&mut space, class);
        }
        refill(
            &self.cage,
            &mut space,
            &self.mark_table,
            self.marking_active.get(),
            class,
            self.cursors[class].get().page,
        )
    }

    fn free_page(&self, space: &mut Space) -> usize {
        free_page(&self.cage, space)
    }

    fn commit(&self, space: &mut Space, number: usize, bytes: usize) {
        commit(&self.cage, space, number, bytes);
    }

    /// Room for a large object of `bytes`, header included, on pages of its
    /// own (the first free run of pages long enough, else new ones), its
    /// bytes committed: its data's offset, and its committed bytes.
    fn allocate_large(&self, bytes: usize) -> (usize, usize) {
        let mut space = self.space.borrow_mut();
        let pages = bytes.div_ceil(PAGE_BYTES);
        let committed = bytes
            .checked_next_multiple_of(self.granularity)
            .expect("an object fits a quarter of the cage");
        let mut run = 0usize;
        let mut first = None;
        for (number, state) in space.states.iter().enumerate() {
            if !matches!(state, PageState::Free { .. }) {
                run = 0;
                continue;
            }
            run = run.saturating_add(1);
            if run == pages {
                first = Some(number.saturating_add(1).saturating_sub(pages));
                break;
            }
        }
        // Else the free run at the end, and new pages after it.
        let first = first.unwrap_or_else(|| {
            let first = space.states.len().saturating_sub(run);
            new_pages(&mut space, pages.saturating_sub(run));
            first
        });
        for at in 0..pages {
            // at < pages = ceil(bytes / PAGE_BYTES) <= committed / PAGE_BYTES rounded up.
            let rest = committed.saturating_sub(at.saturating_mul(PAGE_BYTES));
            let number = first.saturating_add(at);
            self.commit(&mut space, number, rest.min(PAGE_BYTES));
            space.states[number] = PageState::LargeTail;
        }
        let object = LargeObject::new(first, pages, committed);
        // Black while marking runs.
        object.set_marked(self.marking_active.get());
        self.mark_table.insert(first, &object.marks);
        space.states[first] = PageState::Large(object);
        asan::unpoison(self.ptr(page::start_of(first)), committed);
        (large_data(first), committed)
    }

    /// Records the object at `data`, now whole: its header (its type's
    /// `index`, and in its flags only its site's number), and its bit in
    /// `finalizable` if it has drop glue (and in `marks`, in debug builds,
    /// unless a marking runs: then its range is black already, and setting
    /// the bit again would hide a range left white from the tests).
    pub(crate) fn publish(&self, data: usize, index: u32, site: u32, finalizer: Finalizer) {
        let finalizable = finalizer != Finalizer::None;
        // SAFETY: the object's header, in a committed page, 8-aligned.
        unsafe {
            self.header(data).write(index);
            self.header(data).add(1).write(site << site::SITE_SHIFT);
        }
        if self.is_young(data) {
            if finalizable {
                self.young.borrow_mut().finalizable.push(data);
            }
            return;
        }
        if self.marking_active.get()
            && let Some(marking) = self.marking.borrow_mut().as_mut()
        {
            marking.allocated(data);
        }
        // The bitmaps are the page's: only an object with drop glue looks
        // its page up, off allocation's fast path. In debug builds every
        // object does, for the next GC to poison it if it dies.
        let mark = cfg!(debug_assertions) && !self.marking_active.get();
        if !finalizable && !mark {
            return;
        }
        match &mut self.space.borrow_mut().states[page::number_of(data)] {
            PageState::Small(page) => page.publish(page.slot_of(data), finalizer, mark),
            PageState::Large(object) => {
                if mark {
                    object.set_marked(true);
                }
                object.set_finalizable(finalizable);
                object.main = finalizer == Finalizer::Main;
            }
            _ => unreachable!("an object is allocated on a page in use"),
        }
    }

    /// The type of the object at `data`.
    fn type_of(&self, data: usize) -> &'static TypeInfo {
        // SAFETY: an object's header, in a committed page.
        let index = unsafe { self.header(data).read() };
        types::info(index)
    }

    /// Runs the finalizer of the object at `data`.
    ///
    /// # Safety
    ///
    /// The object is whole, has drop glue, and is finalized once.
    unsafe fn finalize(&self, data: usize) {
        let drop = self
            .type_of(data)
            .drop_fn()
            .expect("a finalizable object has drop glue");
        // SAFETY: as the caller promises.
        unsafe { drop(self.ptr(data)) };
    }

    /// A safepoint: under `stress`, a scavenge or a full GC by turns; else a
    /// running marking's barrier drained, and its end once nothing is left
    /// or the old generation reached its limit; a full GC at the limit
    /// with none running (no workers to run one alongside); a marking's
    /// start at half the limit; a scavenge once the nursery is full.
    fn safepoint(&mut self) {
        self.take_discarded();
        self.sweep_at_safepoint();
        if self.options.driven {
            // The engine starts and finishes markings (`Heap::start_marking`);
            // the barrier's objects go to the workers meanwhile. Under
            // `stress`, a scavenge every time.
            if self.marking.get_mut().is_some() {
                self.drain_barrier();
                if let Some(marking) = self.marking.get_mut() {
                    marking.notify();
                }
            }
            if self.options.stress || self.young.get_mut().full {
                self.scavenge_alone();
            }
            return;
        }
        if self.options.stress {
            // A scavenge, then a full GC; where marking runs alongside, a
            // scavenge, a marking's start, a scavenge while it runs, its
            // end, and so on.
            let step = self.stress_step.get();
            self.stress_step.set(step.wrapping_add(1));
            if step.is_multiple_of(2) {
                if self.marking.get_mut().is_some() {
                    self.drain_barrier();
                }
                self.scavenge_alone();
            } else if self.marking.get_mut().is_some() {
                self.drain_barrier();
                self.finish_marking();
            } else if self.concurrent {
                self.start_marking();
            } else {
                self.collect(false);
            }
            return;
        }
        if self.marking.get_mut().is_some() {
            self.drain_barrier();
            if self.at_limit(true) || self.marking_done() {
                if !self.marking_done() {
                    self.markings_at_limit
                        .set(self.markings_at_limit.get().saturating_add(1));
                }
                self.finish_marking();
            } else if self.young.get_mut().full {
                self.scavenge_alone();
            }
        } else if self.at_limit(false) {
            self.collect(false);
        } else if self.starts_marking() {
            self.start_marking();
        } else if self.young.get_mut().full {
            self.scavenge_alone();
        }
    }

    /// Whether the old generation reached its limit, past it by the
    /// overshoot margin while a marking runs.
    fn at_limit(&self, marking: bool) -> bool {
        let limit = self.limit.get();
        if !marking {
            return self.allocated.get() >= limit;
        }
        self.allocated.get() >= limit.saturating_add(self.overshoot.get())
    }

    /// The overshoot margin for a marking starting now: half the limit, or
    /// `min_overshoot_bytes` if more, but at most half of what the cage
    /// has left past the limit.
    fn overshoot_margin(&self) -> usize {
        let limit = self.limit.get();
        let left = cage::CAGE_BYTES.saturating_sub(self.used.get().saturating_add(limit));
        (limit / 2)
            .max(self.options.min_overshoot_bytes)
            .min(left / 2)
    }

    /// The pages the worker discarded: free again, to be committed anew.
    fn take_discarded(&mut self) {
        let done = self.discards.take_done();
        let space = self.space.get_mut();
        for number in done {
            space.states[number] = PageState::Free {
                since: self.gcs.get(),
            };
            discarded(space, number);
        }
    }

    /// Whether a concurrent marking starts at this safepoint: the platform
    /// runs one alongside, and the old generation reached half its limit
    /// (the least of cppgc's range for its incremental marking's start).
    fn starts_marking(&self) -> bool {
        self.concurrent && self.allocated.get() >= self.limit.get() / 2
    }

    /// Whether to check the heap after a GC. Not under Miri, which checks
    /// the stress for undefined behavior: the verifier walks the whole heap
    /// again after each GC, which made its runs take over half an hour;
    /// `cargo test` runs it.
    fn verifies(&self) -> bool {
        self.options.verify || (self.options.stress && !cfg!(miri))
    }

    /// A full GC now, with no marking running: the last one's sweep
    /// finished, a scavenge promoting every survivor, marking with the
    /// platform's threads, sweeping (in this pause too if `atomic`).
    fn collect(&mut self, atomic: bool) {
        debug_assert!(self.marking.get_mut().is_none(), "a marking runs");
        let platform = self.platform.clone();
        let started = Instant::now();
        platform.trace_begin("Gc.Pause");
        self.finish_sweeping(&*platform);
        // A nursery whose scavenges since the last full GC kept next to
        // nothing halves.
        self.young.get_mut().resize_at_full_gc();
        // The nursery emptied first: the old generation then holds every
        // live object, and the mark-sweep never sees a young page.
        self.scavenge(&*platform, true);
        let phase = Instant::now();
        platform.trace_begin("Gc.Mark");
        let marking = self.begin_marking(false, None);
        let (mut record, before, floating) = self.end_marking(&*platform, marking);
        platform.trace_end();
        record.mark_ns = elapsed_ns(phase);
        self.sweep_phases(&*platform, &mut record, before, atomic);
        platform.trace_end();
        record.pause_ns = elapsed_ns(started);
        self.gc_done(record, floating.as_ref());
    }

    /// Starts a concurrent marking, in a pause of its own (`Gc.Pause`
    /// holding its scavenge and `Gc.MarkStart`): the scavenge promoting
    /// every survivor, the roots marked, then the job posted.
    fn start_marking(&mut self) {
        self.start_marking_with(None);
    }

    /// `start_marking`, the marker giving `js` the JS edges it scans (a
    /// marking the engine drives).
    fn start_marking_with(&mut self, js: Option<std::sync::Arc<dyn crate::JsEdgeSink>>) {
        let platform = self.platform.clone();
        let started = Instant::now();
        platform.trace_begin("Gc.Pause");
        self.finish_sweeping(&*platform);
        self.young.get_mut().resize_at_full_gc();
        self.scavenge(&*platform, true);
        // After the sweep (the limit) and the scavenge (what it promoted).
        self.overshoot.set(self.overshoot_margin());
        platform.trace_begin("Gc.MarkStart");
        let mut marking = self.begin_marking(true, js);
        platform.trace_end();
        marking.post(&*platform);
        *self.marking.get_mut() = Some(marking);
        platform.trace_end();
        let ns = elapsed_ns(started);
        self.start_pause_ns.set(ns);
        self.gc_ns.set(self.gc_ns.get().saturating_add(ns));
    }

    /// Finishes the concurrent marking, in a pause (`Gc.Pause` holding
    /// `Gc.Mark`, `Gc.Sweep` and `Gc.Finalize`).
    fn finish_marking(&mut self) {
        let platform = self.platform.clone();
        let started = Instant::now();
        platform.trace_begin("Gc.Pause");
        let phase = Instant::now();
        platform.trace_begin("Gc.Mark");
        let marking = self.marking.get_mut().take().expect("a marking runs");
        let (mut record, before, floating) = self.end_marking(&*platform, marking);
        platform.trace_end();
        record.mark_ns = elapsed_ns(phase);
        record.concurrent = true;
        record.start_pause_ns = self.start_pause_ns.get();
        self.concurrent_markings
            .set(self.concurrent_markings.get().saturating_add(1));
        self.sweep_phases(&*platform, &mut record, before, false);
        platform.trace_end();
        record.pause_ns = elapsed_ns(started);
        self.gc_done(record, floating.as_ref());
    }

    /// A full GC's record kept, its time counted, and the heap checked.
    fn gc_done(&mut self, record: GcRecord, floating: Option<&HashSet<usize>>) {
        self.last_gc.set(record);
        self.gc_ns
            .set(self.gc_ns.get().saturating_add(record.pause_ns));
        if self.verifies() {
            self.verify(floating);
        }
    }

    /// A scavenge on its own, in the trace's `Gc.Scavenge`: a concurrent
    /// marking's job cancelled for it and posted again after.
    fn scavenge_alone(&mut self) {
        let platform = self.platform.clone();
        if self.marking.get_mut().is_some() {
            // The roots removed since the last safepoint, while their
            // objects haven't moved.
            self.drain_barrier();
        }
        if let Some(marking) = self.marking.get_mut() {
            marking.cancel();
        }
        let record = self.scavenge(&*platform, false);
        if let Some(marking) = self.marking.get_mut() {
            marking.post(&*platform);
        }
        self.gc_ns.set(self.gc_ns.get().saturating_add(record.ns));
        if self.verifies() {
            self.verify_young();
        }
    }

    /// The start of a marking, the nursery empty: every page's marks
    /// cleared, the roots marked, the barrier on, the ranges allocation
    /// bumps through black. `concurrent`: its job is posted, so the roots
    /// are published for workers; else the joining thread starts with
    /// them, and wakes no worker for a small graph.
    fn begin_marking(
        &mut self,
        concurrent: bool,
        js: Option<std::sync::Arc<dyn crate::JsEdgeSink>>,
    ) -> concurrent::Marking {
        debug_assert!(!self.sweeping.get(), "the last sweep is done");
        // Where V8 chooses the pages to evacuate.
        if self.options.measure_fragmentation {
            self.measure_fragmentation(&self.fragmentation_at_marking);
        }
        let space = self.space.get_mut();
        // The objects before marking, to poison the dead ones (debug only).
        let mut before = Vec::new();
        for (number, state) in space.states.iter_mut().enumerate() {
            match state {
                PageState::Small(page) => {
                    if cfg!(debug_assertions) {
                        before.extend(
                            page.objects()
                                .into_iter()
                                .map(|slot| (page.data_of(slot), page.slot_bytes)),
                        );
                    }
                    page.clear_marks();
                }
                PageState::Large(object) => {
                    if cfg!(debug_assertions) && object.is_marked() {
                        before.push((large_data(number), object.bytes));
                    }
                    object.set_marked(false);
                }
                _ => {}
            }
        }
        let roots: Vec<usize> = self
            .shared
            .roots
            .in_use()
            .into_iter()
            .filter(|&offset| self.mark_table.mark(offset))
            .collect();
        let floating = self.verifies().then(|| self.reachable());
        self.blacken_cursors(true);
        self.marking_active.set(true);
        self.counts_live.set(js.is_some());
        self.black_bytes.set(0);
        self.shared.roots.watch_removals(true);
        let verifies = self.verifies();
        let marker = concurrent::Marker::new(
            Base(self.cage.base()),
            self.mark_table.clone(),
            self.young_pages.clone(),
            roots,
            concurrent,
            self.sites.get_mut().len(),
            js,
            verifies,
        );
        concurrent::Marking::new(std::sync::Arc::new(marker), before, floating)
    }

    /// The end of a marking: its job cancelled, the barrier's objects (and
    /// roots removed since the last safepoint) marked from, what is left
    /// marked on every thread, the barrier off, the black ranges' rest
    /// cleared. The record so far, the objects before it, and (under the
    /// verifier) the marked objects the roots may no longer reach.
    fn end_marking(
        &mut self,
        platform: &dyn Platform,
        mut marking: concurrent::Marking,
    ) -> (GcRecord, Vec<(usize, usize)>, Option<HashSet<usize>>) {
        marking.cancel();
        // The engine's marking is done (it finishes the heap's): a wrapper
        // it marks from now on is of its next one.
        marking.marker.retire();
        self.drain_removed_roots(&mut marking);
        marking.flush();
        let scanned = marking.marker.scanned.load(Ordering::Relaxed);
        // A driven marking's pause began with the engine's first atomic
        // step.
        let (before_pause, marked_in_steps) =
            marking.driven.as_ref().map_or((scanned, 0), |driven| {
                (driven.before_pause.unwrap_or(scanned), driven.stepped)
            });
        marking.marker.finish(platform);
        let weak = std::mem::take(&mut *sync::lock(&marking.marker.weak_slots));
        let pairs = std::mem::take(&mut *sync::lock(&marking.marker.discovered));
        platform.trace_begin("Gc.Weak");
        self.clear_weak(weak, pairs);
        platform.trace_end();
        self.blacken_cursors(false);
        self.marking_active.set(false);
        // A marking the engine drives: the heap uses its live bytes from
        // now on (what the sweep will find), which the engine sets its next
        // limit from, as V8 does cppgc's (`NotifyMarkingCompleted`); not
        // what it used, the dead too, until the sweep is done.
        if self.counts_live.replace(false) {
            self.used.set(
                marking
                    .marker
                    .marked_bytes
                    .load(Ordering::Relaxed)
                    .saturating_add(self.black_bytes.get()),
            );
        }
        self.shared.roots.watch_removals(false);
        let marker = &marking.marker;
        let marked_objects = marker.scanned.load(Ordering::Relaxed);
        let record = GcRecord {
            marked_objects,
            marked_in_pause: marked_objects.saturating_sub(before_pause),
            marked_in_steps,
            scanned_by_workers: marker.scanned_by_workers.load(Ordering::Relaxed),
            ..GcRecord::default()
        };
        self.background_mark_ns.set(
            self.background_mark_ns.get().saturating_add(
                marker
                    .background_ns
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
        );
        let site_marked = std::mem::take(&mut *sync::lock(&marker.site_marked));
        self.sites.get_mut().collected(&site_marked);
        // Under the verifier, what the engine handed over (its wrappers'
        // objects) and what they reach: marked, as the roots' are.
        let mut floating = marking.floating;
        if let (Some(floating), Some(handed)) = (&mut floating, &marking.marker.handed) {
            let handed = std::mem::take(&mut *sync::lock(handed));
            floating.extend(self.reachable_from(handed));
        }
        (record, marking.before, floating)
    }

    /// Clears the weak edges at `slots` (those the marker found to objects
    /// not marked then), the weak roots and the id table's entries whose
    /// old object isn't marked (it is dead), and the ephemerons at `pairs` (those still discovered:
    /// their key never got marked) whose key isn't, key and value both
    /// (cppgc's `ClearKeyAndValueIfKeyIsDead`). A young one's is a
    /// scavenge's to judge.
    fn clear_weak(&mut self, slots: Vec<usize>, pairs: Vec<usize>) {
        let dead = |raw: u32| {
            let data = raw as usize;
            raw != 0 && !self.is_young(data) && !self.mark_table.is_marked(data)
        };
        for slot in slots {
            // SAFETY: a weak edge of a marked object, 4-aligned, in a
            // committed page; written atomically by the mutator, which
            // doesn't run now.
            let edge = unsafe { std::sync::atomic::AtomicU32::from_ptr(self.ptr(slot).cast()) };
            if dead(edge.load(std::sync::atomic::Ordering::Relaxed)) {
                edge.store(0, std::sync::atomic::Ordering::Relaxed);
            }
        }
        self.shared
            .weak_roots
            .update_entries(|entry| if dead(entry) { 0 } else { entry });
        // The id table's, the whole of it (V8's weak global handles).
        self.ids.borrow_mut().clear(dead);
        for pair in pairs {
            // SAFETY: an ephemeron of a marked object, its key then its
            // value, 4-aligned, in a committed page; written atomically by
            // the mutator, which doesn't run now.
            let (key, value) = unsafe {
                (
                    std::sync::atomic::AtomicU32::from_ptr(self.ptr(pair).cast()),
                    std::sync::atomic::AtomicU32::from_ptr(self.ptr(pair).add(4).cast()),
                )
            };
            if dead(key.load(std::sync::atomic::Ordering::Relaxed)) {
                key.store(0, std::sync::atomic::Ordering::Relaxed);
                value.store(0, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    /// At a safepoint while marking: the barrier's objects published, and
    /// the roots removed since marked from.
    fn drain_barrier(&mut self) {
        let mut marking = self.marking.get_mut().take().expect("a marking runs");
        self.drain_removed_roots(&mut marking);
        marking.flush();
        *self.marking.get_mut() = Some(marking);
    }

    /// The objects of the roots removed while marking (SATB's barrier for
    /// roots): marked, and pushed on the barrier's list.
    fn drain_removed_roots(&mut self, marking: &mut concurrent::Marking) {
        let removed = self.shared.roots.take_removed();
        for offset in removed {
            let offset = offset as usize;
            if offset != 0 && !self.is_young(offset) && self.mark_table.mark(offset) {
                marking.push(offset);
            }
        }
    }

    /// Whether the concurrent marking can finish: nothing is left to mark.
    fn marking_done(&self) -> bool {
        self.marking
            .borrow()
            .as_ref()
            .is_some_and(|marking| marking.done() && self.shared.roots.removed_none())
    }

    /// The weak read barrier: `raw`, a weak edge's or root's value just
    /// read, is the mutator's now; while marking runs it is marked as the
    /// SATB barrier marks (the snapshot may no longer reach it).
    #[inline]
    pub(crate) fn read_weak(&self, raw: u32) {
        if self.marking_active.get() {
            if self.verifies() {
                self.floats(raw);
            }
            self.satb(raw);
        }
    }

    /// Under the verifier, what an old object about to be marked by a weak
    /// read reaches now: it may float, as the snapshot's.
    fn floats(&self, raw: u32) {
        let data = raw as usize;
        if raw == 0 || self.is_young(data) || self.mark_table.is_marked(data) {
            return;
        }
        let reached = self.reachable_from(vec![data]);
        if let Some(marking) = self.marking.borrow_mut().as_mut() {
            marking.floats(reached);
        }
    }

    /// The barrier (SATB): `old`, an edge's value about to be overwritten
    /// while marking runs, is marked if it is old and wasn't, and pushed.
    pub(crate) fn satb(&self, old: u32) {
        let old = old as usize;
        if old == 0 || self.is_young(old) || !self.mark_table.mark(old) {
            return;
        }
        self.marking
            .borrow_mut()
            .as_mut()
            .expect("the barrier runs while marking")
            .push(old);
    }

    /// Marks (or clears) the rest of the range each size class's cursor
    /// bumps through: black while marking runs, its rest cleared at the
    /// end.
    fn blacken_cursors(&self, marked: bool) {
        let space = self.space.borrow();
        for cursor in &self.cursors {
            let cursor = cursor.get();
            if cursor.next < cursor.limit
                && let PageState::Small(page) = &space.states[cursor.page]
            {
                page.marks.set_range(cursor.next, cursor.limit, marked);
            }
        }
    }

    /// A GC's sweep, into `record`: the size-class pages handed to the
    /// sweep (`sweep`), the dead large objects freed, the pool's pages of
    /// an earlier GC discarded; `used`, the next limit and the record's
    /// `marked_bytes` set once the sweep is done, from the live bytes it
    /// found (`sweep_done`). `atomic` (`Heap::collect`, as
    /// cppgc's forced GC; a platform with no workers): all of it in this
    /// pause, the finalizers too; else the sweep runs alongside the
    /// mutator, and the pages with finalizers wait for the next safepoint.
    /// Each phase is a trace event of the platform's. `before`: the
    /// objects before marking, to poison the dead ones (debug builds).
    fn sweep_phases(
        &mut self,
        platform: &dyn Platform,
        record: &mut GcRecord,
        before: Vec<(usize, usize)>,
        atomic: bool,
    ) {
        let gc = self.gcs.get().saturating_add(1);
        let phase = Instant::now();
        platform.trace_begin("Gc.Sweep");
        let space = self.space.get_mut();
        let main_pages = self.main_pages.get_mut();
        let dead_large = self.dead_large.get_mut();
        let mut handed = sweep::Handed::new(page::CLASSES.len());
        let mut large_live = 0usize;
        for number in 0..space.states.len() {
            // The pages to free, from this one.
            let free = match &mut space.states[number] {
                PageState::Small(page) => {
                    page.hand();
                    if page.main_objects > 0 {
                        main_pages.push(number);
                    } else if page.finalizable_objects > 0 {
                        handed.finalizing.push(number);
                    } else {
                        handed.pages[page.class].push(number);
                    }
                    0
                }
                PageState::Large(object) if object.is_marked() => {
                    large_live = large_live.saturating_add(object.bytes);
                    0
                }
                // Freed once finalized: at the next safepoint, or by the
                // sweep's worker.
                PageState::Large(object) if object.is_finalizable() && object.main => {
                    dead_large.push(number);
                    0
                }
                PageState::Large(object) if object.is_finalizable() => {
                    handed.large.push(number);
                    0
                }
                PageState::Large(object) => object.pages,
                _ => 0,
            };
            let end = number.saturating_add(free);
            for (at, page) in (number..end).zip(&mut space.states[number..end]) {
                self.mark_table.remove(at);
                *page = PageState::Free { since: gc };
            }
            space.pool.extend(number..end);
        }
        for pages in &mut space.available {
            pages.clear();
        }
        for cursor in &self.cursors {
            cursor.set(Cursor::default());
        }
        // Discard the pages in the pool since an earlier GC: on a worker,
        // where the platform has some.
        let pool = std::mem::take(&mut space.pool);
        let mut discarding = Vec::new();
        for number in pool {
            match space.states[number] {
                PageState::Free { since } if since < gc && self.concurrent => {
                    space.states[number] = PageState::Discarding;
                    discarding.push(number);
                }
                PageState::Free { since } if since < gc => {
                    self.cage.discard(page::start_of(number), PAGE_BYTES);
                    discarded(space, number);
                }
                PageState::Free { .. } => space.pool.push(number),
                _ => {}
            }
        }
        if !discarding.is_empty() {
            self.discards.push(&discarding);
            let job = self.discard_job.get_mut();
            match job {
                Some(job) => job.notify_concurrency_increase(),
                None => *job = platform.post_job(self.discards.clone()),
            }
        }
        // What the heap uses, and the next limit, once the sweep is done:
        // until then, what it used, and what is allocated since.
        self.swept_live.set(large_live);
        self.allocated.set(0);
        self.gcs.set(gc);
        // The dead objects poisoned, but those whose finalizers run later
        // (the sweep's worker poisons those it finalizes).
        let later: HashSet<usize> = main_pages.iter().chain(&*dead_large).copied().collect();
        for (data, bytes) in before {
            if self.mark_table.is_marked(data) {
                continue;
            }
            if later.contains(&page::number_of(data)) {
                self.poison_later.get_mut().push((data, bytes));
            } else if !self.mark_table.is_finalizable(data) {
                // SAFETY: a dead object's data, in a committed page (pages
                // freed by this GC are discarded by a later one at the
                // earliest), which nothing reads (the sweep reads marks).
                unsafe {
                    ptr::write_bytes(self.ptr(data), POISON, bytes.saturating_sub(HEADER_BYTES))
                };
            }
        }
        if cfg!(fuzzing) {
            self.poison_free();
        }
        self.sweeper.hand(handed);
        self.sweeping.set(true);
        let atomic = atomic || !self.concurrent;
        if atomic {
            platform.run(&*self.sweeper);
            self.apply_swept(&mut self.space.borrow_mut());
        } else {
            *self.sweep_job.get_mut() = platform.post_job(self.sweeper.clone());
        }
        platform.trace_end();
        record.sweep_ns = elapsed_ns(phase);
        if atomic {
            let phase = Instant::now();
            platform.trace_begin("Gc.Finalize");
            self.sweep_main_pages();
            self.sweep_done();
            record.marked_bytes = self.last_live.get();
            platform.trace_end();
            record.finalize_ns = elapsed_ns(phase);
        }
    }

    /// The sweep of an earlier GC finished, in this pause (before a
    /// marking, or a full GC): the pages left swept on the platform's
    /// threads, every result applied, the pages with finalizers swept.
    fn finish_sweeping(&mut self, platform: &dyn Platform) {
        if !self.sweeping.get() {
            return;
        }
        platform.trace_begin("Gc.Sweep");
        if let Some(job) = self.sweep_job.get_mut().take() {
            job.cancel();
        }
        platform.run(&*self.sweeper);
        self.apply_swept(&mut self.space.borrow_mut());
        self.sweep_main_pages();
        self.sweep_done();
        platform.trace_end();
    }

    /// At a safepoint while the sweep runs: the results so far applied, the
    /// pages with finalizers swept (the first time), and the sweep ended
    /// if nothing is left.
    fn sweep_at_safepoint(&self) {
        if !self.sweeping.get() {
            return;
        }
        let started = Instant::now();
        let applied = self.apply_swept(&mut self.space.borrow_mut());
        let swept = self.sweep_main_pages();
        if applied > 0 || swept > 0 {
            self.sweep_on_main(started, swept);
        }
        self.sweep_done();
    }

    /// The sweep ended if no page waits for it: the live bytes it found are
    /// what the heap uses (with what was allocated since the GC), the next
    /// limit and the GC's marked bytes; under the verifier, the pages'
    /// accounts checked. V8 and cppgc set the limit at the end of marking,
    /// from the bytes their markers counted; a count of each object's
    /// bytes as it is marked slowed marking here by 3 to 21%, and the
    /// sweep is done some milliseconds later.
    fn sweep_done(&self) {
        if !self.sweeper.is_done() || !self.main_pages.borrow().is_empty() {
            return;
        }
        self.sweeping.set(false);
        let live = self.swept_live.get();
        self.last_live.set(live);
        self.used.set(live.saturating_add(self.allocated.get()));
        self.limit.set(live.max(self.options.min_allocation_bytes));
        let mut last = self.last_gc.get();
        last.marked_bytes = live;
        self.last_gc.set(last);
        if self.options.measure_fragmentation {
            self.measure_fragmentation(&self.fragmentation);
        }
        if self.verifies() {
            self.verify_space();
        }
    }

    /// Counts the heap's thread's sweeping since `started`, `pages` swept.
    fn sweep_on_main(&self, started: Instant, pages: usize) {
        self.sweep_on_main_ns.set(
            self.sweep_on_main_ns
                .get()
                .saturating_add(elapsed_ns(started)),
        );
        self.pages_swept_on_main.set(
            self.pages_swept_on_main
                .get()
                .saturating_add(u64::try_from(pages).unwrap_or(u64::MAX)),
        );
    }

    /// Sweeping runs and `class` has no page to allocate on: the results so
    /// far applied, then the class's pages swept here until one has room
    /// (or none is left).
    fn sweep_for(&self, space: &mut Space, class: usize) {
        let started = Instant::now();
        self.apply_swept(space);
        let mut swept = 0usize;
        while space.available[class].is_empty()
            && let Some(number) = self.sweeper.claim(class)
        {
            let result = self.mark_table.sweep(number);
            let PageState::Small(page) = &mut space.states[number] else {
                unreachable!("a page to sweep is in use");
            };
            let live = result.live;
            page.apply(result);
            self.applied(space, number, live);
            self.poison_page(space, number);
            swept = swept.saturating_add(1);
        }
        self.sweep_on_main(started, swept);
    }

    /// Applies the sweep's results so far to their pages: how many.
    fn apply_swept(&self, space: &mut Space) -> usize {
        let (results, freed) = self.sweeper.take_swept();
        let count = results.len().saturating_add(freed.len());
        for (number, result) in results {
            let PageState::Small(page) = &mut space.states[number] else {
                unreachable!("a swept page is in use");
            };
            let live = result.live;
            page.apply(result);
            self.applied(space, number, live);
            self.poison_page(space, number);
        }
        // The large objects the sweep finalized: their pages freed.
        for number in freed {
            self.free_large(space, number);
        }
        count
    }

    /// Frees the dead large object on page `number` (finalized, if it had
    /// drop glue): its pages go to the pool.
    fn free_large(&self, space: &mut Space, number: usize) {
        let PageState::Large(object) = &space.states[number] else {
            unreachable!("a dead large object's page");
        };
        let end = number.saturating_add(object.pages);
        let gc = self.gcs.get();
        for at in number..end {
            self.mark_table.remove(at);
            space.states[at] = PageState::Free { since: gc };
            self.poison_page(space, at);
        }
        space.pool.extend(number..end);
    }

    /// Tells ASan not to touch page `number`'s free slots, or the page if
    /// it is free, under fuzzing: once nothing reads them (the finalizers
    /// of its dead objects ran).
    fn poison_page(&self, space: &Space, number: usize) {
        if !cfg!(fuzzing) {
            return;
        }
        match &space.states[number] {
            PageState::Small(page) => {
                for (start, bytes) in page.free_spans() {
                    asan::poison(self.ptr(start), bytes);
                }
            }
            PageState::Free { .. } => asan::poison(self.ptr(page::start_of(number)), PAGE_BYTES),
            _ => {}
        }
    }

    /// Page `number`, just swept (`live` objects left): offered to its
    /// class, the fewest free slots last, or put in the pool if empty.
    fn applied(&self, space: &mut Space, number: usize, live: usize) {
        let PageState::Small(page) = &space.states[number] else {
            unreachable!("a swept page is in use");
        };
        if self.verifies()
            && let Err(problem) = page.check()
        {
            panic!("heap verifier: page {number}: {problem}");
        }
        self.swept_live.set(
            self.swept_live
                .get()
                .saturating_add(live.saturating_mul(page.slot_bytes)),
        );
        if live == 0 {
            self.mark_table.remove(number);
            space.states[number] = PageState::Free {
                since: self.gcs.get(),
            };
            space.pool.push(number);
            return;
        }
        if page.free > 0 {
            let (class, free) = (page.class, page.free);
            let states = &space.states;
            let at = space.available[class].partition_point(|&other| match &states[other] {
                PageState::Small(other) => other.free >= free,
                _ => true,
            });
            space.available[class].insert(at, number);
        }
    }

    /// Sweeps the pages with finalizers (and frees the dead large objects
    /// with drop glue): the heap whole again, then the finalizers run (one
    /// may panic, or drop a `Persistent`: a panic leaves the rest
    /// unfinalized, never finalized twice), then the dead objects are
    /// poisoned (debug builds). How many pages.
    fn sweep_main_pages(&self) -> usize {
        let pages = std::mem::take(&mut *self.main_pages.borrow_mut());
        let large = std::mem::take(&mut *self.dead_large.borrow_mut());
        if pages.is_empty() && large.is_empty() {
            return 0;
        }
        let mut dead = Vec::new();
        {
            let mut space = self.space.borrow_mut();
            for &number in &pages {
                let PageState::Small(page) = &mut space.states[number] else {
                    unreachable!("a page to sweep is in use");
                };
                let first = dead.len();
                let live = page.sweep(&mut dead);
                let main = dead[first..]
                    .iter()
                    .filter(|&&data| self.type_of(data).finalizer() == Finalizer::Main)
                    .count();
                page.main_objects = page.main_objects.saturating_sub(main);
                self.applied(&mut space, number, live);
            }
            for &number in &large {
                if let PageState::Large(object) = &space.states[number] {
                    object.set_finalizable(false);
                }
                dead.push(large_data(number));
            }
        }
        for &data in &dead {
            // SAFETY: a dead object with drop glue, whole until now (its
            // slot handed out by no allocation since: none runs at a
            // safepoint), finalized once (it is no longer finalizable).
            unsafe { self.finalize(data) };
        }
        let mut space = self.space.borrow_mut();
        let gc = self.gcs.get();
        let mut freed = Vec::new();
        for number in large {
            let PageState::Large(object) = &space.states[number] else {
                unreachable!("a dead large object's page");
            };
            let end = number.saturating_add(object.pages);
            for at in number..end {
                self.mark_table.remove(at);
                space.states[at] = PageState::Free { since: gc };
            }
            space.pool.extend(number..end);
            freed.extend(number..end);
        }
        drop(space);
        for (data, bytes) in std::mem::take(&mut *self.poison_later.borrow_mut()) {
            // SAFETY: a dead object's data, finalized, in a committed page
            // (freed pages are discarded a GC later at the earliest).
            unsafe { ptr::write_bytes(self.ptr(data), POISON, bytes.saturating_sub(HEADER_BYTES)) };
        }
        let space = self.space.borrow();
        for &number in pages.iter().chain(&freed) {
            self.poison_page(&space, number);
        }
        pages.len()
    }
}

impl Inner {
    /// Tells ASan not to touch the free slots and pages, under fuzzing.
    fn poison_free(&self) {
        for (number, state) in self.space.borrow().states.iter().enumerate() {
            match state {
                PageState::Small(page) => {
                    for (start, bytes) in page.free_spans() {
                        asan::poison(self.ptr(start), bytes);
                    }
                }
                PageState::Free { .. } => {
                    asan::poison(self.ptr(page::start_of(number)), PAGE_BYTES)
                }
                _ => {}
            }
        }
    }
}

/// The heap's start, for the threads that mark.
#[derive(Clone, Copy)]
pub(super) struct Base(pub(super) *mut u8);

// SAFETY: the threads that mark read the heap's headers and edges through
// it: an edge is an atomic (written with release while marking runs, read
// relaxed and then fenced with acquire), a header is written before its
// object is published; a
// scavenge's threads read and write while nothing else runs (`Platform::run`
// returns once they are done, and a marking's job is cancelled first). A
// posted job outlives no heap: the heap cancels it before it goes.
unsafe impl Send for Base {}
// SAFETY: as for `Send`.
unsafe impl Sync for Base {}

/// A marking thread's count of the objects of each site, the last site's
/// kept apart (and of the bytes it scanned): objects in a row are mostly of one site, so most counts
/// touch no memory (counting each in the array slowed marking chains by a
/// sixth).
pub(super) struct SiteTally {
    counts: Vec<usize>,
    site: u32,
    run: usize,
    /// The bytes of the objects the thread scanned, in a marking that
    /// counts them (`Marker::marked_bytes`).
    bytes: usize,
}

impl SiteTally {
    fn add(&mut self, site: u32) {
        if site != self.site {
            self.flush();
            self.site = site;
        }
        self.run = self.run.saturating_add(1);
    }

    /// The last site's run into its count.
    fn flush(&mut self) {
        if let Some(count) = self.counts.get_mut(self.site as usize) {
            *count = count.saturating_add(self.run);
        }
        self.run = 0;
    }
}

/// A new range for a cursor of `class` whose page was `number`: the rest
/// of that page's ranges, then the class's page with the fewest free slots,
/// then a free page. The heap's thread's cursors take ranges so, and a
/// scavenge's threads' (under the space's lock).
fn refill(
    cage: &Cage,
    space: &mut Space,
    marks: &MarkTable,
    black: bool,
    class: usize,
    number: usize,
) -> Cursor {
    let mut number = number;
    loop {
        if let Some(PageState::Small(page)) = space.states.get_mut(number)
            && let Some((next, limit)) = page.take_range()
        {
            // While marking runs, what is allocated is black: the range
            // whole, its rest cleared at the end.
            if black {
                page.marks.set_range(next, limit, true);
            }
            let slots = limit.saturating_sub(next).checked_div(page.slot_bytes);
            space.ranges = space.ranges.saturating_add(1);
            space.range_slots = space
                .range_slots
                .saturating_add(slots.map_or(0, |slots| u64::try_from(slots).unwrap_or(u64::MAX)));
            return Cursor {
                next,
                limit,
                page: number,
            };
        }
        number = match space.available[class].pop() {
            Some(number) => number,
            None => {
                let number = free_page(cage, space);
                let page = Page::new(number, class);
                marks.insert(number, &page.marks);
                space.states[number] = PageState::Small(page);
                number
            }
        };
    }
}

/// A free page, committed: from the pool, else a discarded one, else a new
/// one.
fn free_page(cage: &Cage, space: &mut Space) -> usize {
    let is_free =
        |space: &Space, number: usize| matches!(space.states[number], PageState::Free { .. });
    let number = loop {
        match space.pool.pop() {
            Some(number) if is_free(space, number) => break number,
            Some(_) => {}
            None => match space.discarded.pop() {
                Some(number) if is_free(space, number) => break number,
                Some(_) => {}
                None => break new_pages(space, 1),
            },
        }
    };
    commit(cage, space, number, PAGE_BYTES);
    number
}

/// `count` never-used pages, after the last page used: the first's number.
fn new_pages(space: &mut Space, count: usize) -> usize {
    let first = space.states.len();
    let end = first.checked_add(count).filter(|&end| end <= PAGES);
    assert!(end.is_some(), "the heap's cage is full");
    for _ in 0..count {
        space.states.push(PageState::Free { since: 0 });
        space.committed.push(0);
    }
    first
}

/// Page `number`, free, holds no memory any more.
fn discarded(space: &mut Space, number: usize) {
    space.committed_total = space
        .committed_total
        .saturating_sub(space.committed[number]);
    space.committed[number] = 0;
    space.discarded.push(number);
}

/// Makes the first `bytes` of page `number` hold memory.
fn commit(cage: &Cage, space: &mut Space, number: usize, bytes: usize) {
    let held = space.committed[number];
    if held < bytes {
        cage.commit(page::start_of(number), bytes);
        space.committed[number] = bytes;
        space.committed_total = space
            .committed_total
            .saturating_add(bytes.saturating_sub(held));
    }
}

/// The data's offset of the large object on page `number`.
fn large_data(number: usize) -> usize {
    page::start_of(number).saturating_add(HEADER_BYTES)
}

#[cfg(all(test, not(any(loom, shuttle))))]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::{GcObject, Member, Persistent};

    #[derive(GcObject)]
    struct Leaf {
        value: Cell<u32>,
    }

    /// Filling 4 GiB takes too long: the cage is made to look full.
    #[test]
    #[should_panic = "the heap's cage is full"]
    fn a_full_cage_panics_in_an_operation() {
        let mut heap = Heap::default();
        let space = heap.inner.space.get_mut();
        space.states.resize_with(PAGES, || PageState::LargeTail);
        space.committed.resize(PAGES, 0);
        heap.mutate(|cx| {
            cx.alloc(LeafInit {
                value: Cell::new(0),
            });
        });
    }

    #[derive(GcObject)]
    struct Pair {
        next: Member<Pair>,
        value: Cell<u32>,
    }

    /// The other class: its page empties at the GC.
    #[derive(GcObject)]
    struct Wide {
        values: [Cell<u64>; 30],
    }

    /// A heap right after a GC that kept a rooted chain of three and freed
    /// a page of `Wide` garbage: the verifier is happy with it. The root's
    /// data, and the garbage's.
    fn collected() -> (Heap, Persistent<Pair>, usize, usize) {
        let mut heap = Heap::with_platform(
            HeapOptions::default(),
            std::sync::Arc::new(crate::InlinePlatform),
        );
        let (root, garbage) = heap.mutate(|cx| {
            let pair = |next| {
                cx.alloc(PairInit {
                    next,
                    value: Cell::new(1),
                })
            };
            let root = pair(Some(pair(Some(pair(None)))));
            let wide = cx.alloc(WideInit {
                values: std::array::from_fn(|_| Cell::new(0)),
            });
            (
                Persistent::new(cx, root),
                cx.inner
                    .offset_of(wide.into_ref() as *const Wide as *const u8),
            )
        });
        heap.collect();
        heap.inner.verify(None);
        let data = heap.inner.shared.roots.in_use()[0];
        (heap, root, data, garbage)
    }

    fn page_of(heap: &Heap, data: usize, f: impl FnOnce(&Page, usize)) {
        match &heap.inner.space.borrow().states[page::number_of(data)] {
            PageState::Small(page) => f(page, page.slot_of(data)),
            _ => unreachable!("a small object"),
        }
    }

    #[test]
    #[should_panic = "is reachable but unmarked"]
    fn the_verifier_finds_a_live_object_unmarked() {
        let (heap, _root, data, _) = collected();
        page_of(&heap, data, |page, slot| page.unmark(slot));
        heap.inner.verify(None);
    }

    /// An object the roots no longer reach (its edge cleared behind the
    /// GC's back), still marked: what a marker that marked too much leaves.
    #[test]
    #[should_panic = "objects are marked, but the roots reach"]
    fn the_verifier_finds_an_unreachable_object_marked() {
        let (heap, _root, data, _) = collected();
        // SAFETY: the root's `next`, a `Member` of a whole object.
        unsafe {
            heap.inner
                .ptr(data + std::mem::offset_of!(Pair, next))
                .cast::<u32>()
                .write(0);
        }
        heap.inner.verify(None);
    }

    /// The same after a concurrent marking: a marked object the roots no
    /// longer reach may float, if it was in the snapshot or allocated while
    /// marking; else it is reported.
    #[test]
    #[should_panic = "neither reachable, nor in the snapshot, nor allocated while marking"]
    fn the_verifier_finds_an_unreachable_object_marked_and_not_floating() {
        let (heap, _root, data, _) = collected();
        let next = heap
            .inner
            .ptr(data + std::mem::offset_of!(Pair, next))
            .cast::<u32>();
        // SAFETY: the root's `next`, a `Member` of a whole object.
        let target = unsafe { next.read() } as usize;
        // SAFETY: as above.
        unsafe { next.write(0) };
        heap.inner.verify(Some(&HashSet::from([target])));
        // Others floating, not it.
        heap.inner.verify(Some(&HashSet::from([data])));
    }

    /// Flags an old object can't have: `AGED`, and a site this heap never
    /// allocated from.
    #[test]
    #[should_panic = "has flags 0x2"]
    fn the_verifier_finds_an_old_object_aged() {
        let (heap, _root, data, _) = collected();
        // SAFETY: the root's header's flags, in a committed page.
        unsafe { heap.inner.header(data).add(1).write(young::AGED) };
        heap.inner.verify(None);
    }

    #[test]
    #[should_panic = "is of site 5, which the heap never allocated from"]
    fn the_verifier_finds_an_object_of_an_unknown_site() {
        let (heap, _root, data, _) = collected();
        // SAFETY: as above.
        unsafe { heap.inner.header(data).add(1).write(5 << site::SITE_SHIFT) };
        heap.inner.verify(None);
    }

    /// A free slot marked: its page's count of free slots is off.
    #[test]
    #[should_panic = "free slots counted"]
    fn the_verifier_finds_a_free_slot_marked() {
        let (heap, _root, data, _) = collected();
        page_of(&heap, data, |page, slot| {
            page.mark(slot + 3);
        });
        heap.inner.verify(None);
    }

    #[test]
    #[should_panic = "holds the marked slot"]
    fn the_verifier_finds_a_free_range_over_a_live_object() {
        let (heap, _root, data, _) = collected();
        let slot = match &heap.inner.space.borrow().states[page::number_of(data)] {
            PageState::Small(page) => page.slot_of(data),
            _ => unreachable!(),
        };
        if let PageState::Small(page) =
            &mut heap.inner.space.borrow_mut().states[page::number_of(data)]
        {
            page.ranges.insert(0, (slot as u32, slot as u32 + 1));
        }
        heap.inner.verify(None);
    }

    #[test]
    #[should_panic = "isn't on a page in use"]
    fn the_verifier_finds_an_edge_to_a_freed_page() {
        let (heap, _root, data, garbage) = collected();
        // SAFETY: the root's `next`, a `Member` of a whole object.
        unsafe {
            heap.inner
                .ptr(data + std::mem::offset_of!(Pair, next))
                .cast::<u32>()
                .write(garbage as u32);
        }
        heap.inner.verify(None);
    }

    /// With `verify`, every GC runs the verifier: a heap broken before a
    /// GC (in what the GC leaves as it is) fails it.
    #[test]
    #[should_panic = "heap verifier: the pages commit"]
    fn a_gc_with_verify_runs_the_verifier() {
        let (mut heap, _root, _, _) = collected();
        heap.inner.options.verify = true;
        heap.inner.space.get_mut().committed_total += 1;
        heap.collect();
    }

    /// An old object's edge to a young one written behind the barrier's
    /// back: a scavenge would miss what it keeps.
    #[test]
    #[should_panic = "isn't in a slot set"]
    fn the_verifier_finds_an_old_edge_not_remembered() {
        let (mut heap, root, data, _) = collected();
        heap.mutate(|cx| {
            let young = cx.alloc(PairInit {
                next: None,
                value: Cell::new(2),
            });
            let young = cx
                .inner
                .offset_of(young.into_ref() as *const Pair as *const u8);
            // SAFETY: the root's `next`, a `Member` of a whole object.
            unsafe {
                cx.inner
                    .ptr(data + std::mem::offset_of!(Pair, next))
                    .cast::<u32>()
                    .write(young as u32);
            }
        });
        heap.inner.verify_young();
        drop(root);
    }

    #[derive(GcObject)]
    struct WeakHolder {
        weak: crate::WeakMember<Pair>,
    }

    /// A heap right after a full GC that kept a rooted, old `WeakHolder`
    /// with no weak edge yet: its root, its data, and a freed object's on
    /// its page (a `Pair`, of its class), which is in use.
    fn weak_holder() -> (Heap, Persistent<WeakHolder>, usize, usize) {
        let mut heap = Heap::with_platform(
            HeapOptions::default(),
            std::sync::Arc::new(crate::InlinePlatform),
        );
        let (root, garbage) = heap.mutate(|cx| {
            // Old from the start: the pair dies on the holder's page.
            cx.pretenured(|cx| {
                let holder = cx.alloc(WeakHolderInit { weak: None });
                let pair = cx.alloc(PairInit {
                    next: None,
                    value: Cell::new(0),
                });
                (
                    Persistent::new(cx, holder),
                    cx.inner
                        .offset_of(pair.into_ref() as *const Pair as *const u8),
                )
            })
        });
        heap.collect();
        heap.inner.verify(None);
        let data = heap.inner.shared.roots.in_use()[0];
        (heap, root, data, garbage)
    }

    /// A weak edge to a freed object (its slot free, its page in use), left
    /// behind: what a GC that didn't clear it leaves.
    #[test]
    #[should_panic = "a weak edge to a dead object, not cleared"]
    fn the_verifier_finds_a_weak_edge_to_a_dead_object() {
        let (heap, _root, data, garbage) = weak_holder();
        assert_eq!(page::number_of(garbage), page::number_of(data));
        // SAFETY: the holder's weak edge, of a whole object.
        unsafe {
            heap.inner
                .ptr(data + std::mem::offset_of!(WeakHolder, weak))
                .cast::<u32>()
                .write(garbage as u32);
        }
        heap.inner.verify(None);
    }

    /// An old object's weak edge to a young one written behind the
    /// barrier's back: not in a weak slot set.
    #[test]
    #[should_panic = "old to young, but not in a weak slot set"]
    fn the_verifier_finds_an_old_weak_edge_not_remembered() {
        let (mut heap, root, data, _) = weak_holder();
        heap.mutate(|cx| {
            let young = cx.alloc(PairInit {
                next: None,
                value: Cell::new(2),
            });
            let young = cx
                .inner
                .offset_of(young.into_ref() as *const Pair as *const u8);
            // SAFETY: the holder's weak edge, of a whole object.
            unsafe {
                cx.inner
                    .ptr(data + std::mem::offset_of!(WeakHolder, weak))
                    .cast::<u32>()
                    .write(young as u32);
            }
        });
        heap.inner.verify_young();
        drop(root);
    }

    #[derive(GcObject)]
    struct PairHolder {
        pair: crate::EphemeronPair<Pair, Pair>,
    }

    /// A heap right after a full GC that kept a rooted, old `PairHolder`
    /// whose key and value are `Pair`s, the key rooted: the roots, the
    /// holder's data, and a freed `Pair`'s on their page.
    fn pair_holder() -> (
        Heap,
        (Persistent<PairHolder>, Persistent<Pair>),
        usize,
        usize,
    ) {
        let mut heap = Heap::with_platform(
            HeapOptions::default(),
            std::sync::Arc::new(crate::InlinePlatform),
        );
        let (roots, garbage) = heap.mutate(|cx| {
            cx.pretenured(|cx| {
                let pair = |value| {
                    cx.alloc(PairInit {
                        next: None,
                        value: Cell::new(value),
                    })
                };
                let (key, value) = (pair(1), pair(2));
                let holder = cx.alloc(PairHolderInit {
                    pair: (Some(key), Some(value)),
                });
                let garbage = pair(3);
                (
                    (Persistent::new(cx, holder), Persistent::new(cx, key)),
                    cx.inner
                        .offset_of(garbage.into_ref() as *const Pair as *const u8),
                )
            })
        });
        heap.collect();
        heap.inner.verify(None);
        let data = heap.inner.shared.roots.in_use()[0];
        (heap, roots, data, garbage)
    }

    /// An ephemeron whose live key's value was freed: what a marking that
    /// didn't mark through the pair leaves.
    #[test]
    #[should_panic = "a live key's value, not marked"]
    fn the_verifier_finds_a_live_keys_value_not_marked() {
        let (heap, _roots, data, garbage) = pair_holder();
        // SAFETY: the holder's ephemeron's value, of a whole object.
        unsafe {
            heap.inner
                .ptr(data + std::mem::offset_of!(PairHolder, pair) + 4)
                .cast::<u32>()
                .write(garbage as u32);
        }
        heap.inner.verify(None);
    }

    /// A value whose key was cleared, left: what an end that cleared only
    /// the key leaves.
    #[test]
    #[should_panic = "a value without a key"]
    fn the_verifier_finds_a_value_without_a_key() {
        let (heap, _roots, data, _) = pair_holder();
        // SAFETY: the holder's ephemeron's key, of a whole object.
        unsafe {
            heap.inner
                .ptr(data + std::mem::offset_of!(PairHolder, pair))
                .cast::<u32>()
                .write(0);
        }
        heap.inner.verify(None);
    }

    /// An old ephemeron's key to a young object written behind the barrier's
    /// back: not in an ephemeron slot set.
    #[test]
    #[should_panic = "its key, old to young, isn't in an ephemeron slot set"]
    fn the_verifier_finds_an_old_key_not_remembered() {
        let (mut heap, roots, data, _) = pair_holder();
        heap.mutate(|cx| {
            let young = cx.alloc(PairInit {
                next: None,
                value: Cell::new(4),
            });
            let young = cx
                .inner
                .offset_of(young.into_ref() as *const Pair as *const u8);
            // SAFETY: the holder's ephemeron's key, of a whole object.
            unsafe {
                cx.inner
                    .ptr(data + std::mem::offset_of!(PairHolder, pair))
                    .cast::<u32>()
                    .write(young as u32);
            }
        });
        heap.inner.verify_young();
        drop(roots);
    }

    /// An id left to a freed object: what a full GC that didn't clear it
    /// leaves.
    #[test]
    #[should_panic = "a weak edge to a dead object, not cleared"]
    fn the_verifier_finds_an_id_to_a_dead_object() {
        let (heap, _root, _, garbage) = collected();
        heap.inner.ids.borrow_mut().insert(garbage as u32, false);
        heap.inner.verify(None);
    }

    /// A chunk of the id table with no object left: what a full GC that
    /// didn't free it leaves.
    #[test]
    #[should_panic = "the id table holds a chunk with no object"]
    fn the_verifier_finds_an_empty_id_chunk() {
        let (heap, _root, data, _) = collected();
        let id = heap.inner.ids.borrow_mut().insert(data as u32, false);
        heap.inner.ids.borrow_mut().empty_for_testing(id);
        heap.inner.verify(None);
    }

    /// A young object's id not in the list of young ids: what an id given
    /// without listing it leaves, a scavenge then missing it.
    #[test]
    #[should_panic = "a young object's, not listed as young"]
    fn the_verifier_finds_a_young_id_not_listed() {
        let (mut heap, root, _, _) = collected();
        heap.mutate(|cx| {
            let young = cx.alloc(PairInit {
                next: None,
                value: Cell::new(2),
            });
            let young = cx
                .inner
                .offset_of(young.into_ref() as *const Pair as *const u8);
            cx.inner.ids.borrow_mut().insert(young as u32, false);
        });
        heap.inner.verify_young();
        drop(root);
    }

    /// An old object's id listed as young: what a scavenge that kept a
    /// promoted object's id in the list leaves.
    #[test]
    #[should_panic = "is listed as young, but its object isn't"]
    fn the_verifier_finds_an_old_id_listed_as_young() {
        let (heap, _root, data, _) = collected();
        heap.inner.ids.borrow_mut().insert(data as u32, true);
        heap.inner.verify_young();
    }

    /// A slot set's entry where no live object is (the freed page's): what
    /// a sweep of an object holding one would leave.
    #[test]
    #[should_panic = "which no live object holds"]
    fn the_verifier_finds_a_slot_set_entry_no_object_holds() {
        let (heap, _root, _, garbage) = collected();
        let number = page::number_of(garbage);
        heap.inner.remembered_weak[number]
            .insert(garbage.saturating_sub(page::start_of(number)), PAGE_BYTES);
        heap.inner.verify(None);
    }

    /// With `verify`, a scavenge on its own runs the verifier: an old edge
    /// to a young object written behind the barrier's back is left pointing
    /// to from-space, which it finds.
    #[test]
    #[should_panic = "heap verifier"]
    fn a_scavenge_with_verify_runs_the_verifier() {
        let (mut heap, root, data, _) = collected();
        heap.inner.options.verify = true;
        heap.mutate(|cx| {
            let young = cx.alloc(PairInit {
                next: None,
                value: Cell::new(2),
            });
            let young = cx
                .inner
                .offset_of(young.into_ref() as *const Pair as *const u8);
            // SAFETY: the root's `next`, a `Member` of a whole object.
            unsafe {
                cx.inner
                    .ptr(data + std::mem::offset_of!(Pair, next))
                    .cast::<u32>()
                    .write(young as u32);
            }
        });
        heap.scavenge();
        drop(root);
    }

    /// A reachable object whose header says it was copied: a stale edge.
    #[test]
    #[should_panic = "is forwarded"]
    fn the_verifier_finds_a_forwarded_object_reachable() {
        let mut heap = Heap::with_platform(
            HeapOptions::default(),
            std::sync::Arc::new(crate::InlinePlatform),
        );
        let root = heap.mutate(|cx| {
            Persistent::new(
                cx,
                cx.alloc(PairInit {
                    next: None,
                    value: Cell::new(1),
                }),
            )
        });
        heap.scavenge();
        heap.inner.verify_young();
        let data = heap.inner.shared.roots.in_use()[0];
        // SAFETY: the young root's flags, in its header.
        unsafe { heap.inner.header(data).add(1).write(1) };
        heap.inner.verify_young();
        drop(root);
    }

    #[test]
    #[should_panic = "the pages commit"]
    fn the_verifier_finds_commits_miscounted() {
        let (heap, _root, _, _) = collected();
        heap.inner.space.borrow_mut().committed_total += 1;
        heap.inner.verify(None);
    }
    /// A page waiting for a sweep that doesn't hold it: it would never get
    /// free ranges.
    #[test]
    #[should_panic = "waits for a sweep that doesn't hold it"]
    fn the_verifier_finds_a_page_no_sweep_holds() {
        let (heap, _root, data, _) = collected();
        let mut space = heap.inner.space.borrow_mut();
        let PageState::Small(page) = &mut space.states[page::number_of(data)] else {
            unreachable!()
        };
        page.hand();
        drop(space);
        heap.inner.verify(None);
    }

    /// A page waiting for its sweep with a free range: allocation would
    /// hand out a slot the sweep may find live.
    #[test]
    #[should_panic = "not swept yet, but it has free ranges"]
    fn the_verifier_finds_an_unswept_page_with_ranges() {
        let (heap, _root, data, _) = collected();
        let number = page::number_of(data);
        heap.inner.main_pages.borrow_mut().push(number);
        let mut space = heap.inner.space.borrow_mut();
        let PageState::Small(page) = &mut space.states[number] else {
            unreachable!()
        };
        page.hand();
        page.ranges.push((0, 1));
        drop(space);
        heap.inner.verify(None);
    }

    /// A sweep's result that frees a live object's slot, checked as it is
    /// applied.
    #[test]
    #[should_panic = "heap verifier: page"]
    fn the_verifier_checks_a_page_as_its_sweep_is_applied() {
        let (mut heap, _root, data, _) = collected();
        heap.inner.options.verify = true;
        let number = page::number_of(data);
        let mut space = heap.inner.space.borrow_mut();
        let PageState::Small(page) = &mut space.states[number] else {
            unreachable!()
        };
        let slots = u32::try_from(page.slots).unwrap();
        page.hand();
        page.apply(page::Swept {
            ranges: vec![(0, slots)],
            live: 1,
        });
        heap.inner.applied(&mut space, number, 1);
    }

    /// A page with an object to finalize on the heap's thread, held by the
    /// sweep's worker: it would run that `Drop` on another thread.
    #[test]
    #[should_panic = "has objects to finalize on the heap's thread, but the sweep's worker holds it"]
    fn the_verifier_finds_a_main_thread_page_on_the_job() {
        let (heap, _root, data, _) = collected();
        let number = page::number_of(data);
        let mut space = heap.inner.space.borrow_mut();
        let PageState::Small(page) = &mut space.states[number] else {
            unreachable!()
        };
        page.hand();
        page.main_objects = 1;
        page.finalizable_objects = 1;
        drop(space);
        let mut handed = sweep::Handed::new(page::CLASSES.len());
        handed.finalizing.push(number);
        heap.inner.sweeper.hand(handed);
        heap.inner.verify(None);
    }

    /// A large object kept unmarked (and unreachable), neither waiting for
    /// its finalizer nor held by the sweep.
    #[test]
    #[should_panic = "is unmarked but kept"]
    fn the_verifier_finds_a_large_object_unmarked() {
        #[derive(GcObject)]
        struct Huge {
            words: [Cell<u64>; 5000],
        }
        let (mut heap, _root, _, _) = collected();
        let huge = heap.mutate(|cx| {
            let huge = cx.alloc(HugeInit {
                words: std::array::from_fn(|_| Cell::new(0)),
            });
            Persistent::new(cx, huge)
        });
        heap.collect();
        let number = heap
            .inner
            .space
            .borrow()
            .states
            .iter()
            .position(|state| matches!(state, PageState::Large(_)));
        let space = heap.inner.space.borrow();
        let Some(PageState::Large(object)) = number.map(|number| &space.states[number]) else {
            unreachable!()
        };
        object.set_marked(false);
        drop(space);
        drop(huge);
        heap.inner.verify(None);
    }

    /// The sweep done, the heap counts other bytes than its marked slots.
    #[test]
    #[should_panic = "once the sweep is done"]
    fn the_verifier_finds_the_used_bytes_miscounted_once_swept() {
        let (heap, _root, _, _) = collected();
        heap.inner.used.set(heap.inner.used.get() + 16);
        heap.inner.verify_space();
    }

    /// The sweep done, a page still waits for it.
    #[test]
    #[should_panic = "isn't swept, but the sweep is done"]
    fn the_verifier_finds_a_page_unswept_once_done() {
        let (heap, _root, data, _) = collected();
        let mut space = heap.inner.space.borrow_mut();
        let PageState::Small(page) = &mut space.states[page::number_of(data)] else {
            unreachable!()
        };
        page.hand();
        drop(space);
        heap.inner.verify_space();
    }

    /// A page being discarded that no worker was handed: nothing would
    /// give it back to the pages the heap takes.
    #[test]
    #[should_panic = "is being discarded, but the job doesn't hold it"]
    fn the_verifier_finds_a_discarding_page_not_handed() {
        let (heap, _root, _, _) = collected();
        let mut space = heap.inner.space.borrow_mut();
        let number = new_pages(&mut space, 1);
        space.states[number] = PageState::Discarding;
        drop(space);
        heap.inner.verify(None);
    }
}
