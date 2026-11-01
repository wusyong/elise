//! Marking the old generation alongside the mutator (V8's
//! `ConcurrentMarking`, with a snapshot-at-the-beginning barrier rather
//! than its Dijkstra one). It starts at a safepoint, right after a scavenge
//! that promotes every survivor (so the snapshot is the old generation's),
//! with the roots marked; a job posted to the platform marks from them on
//! workers while the mutator runs. Meanwhile:
//!
//! - the barrier (`Inner::satb`) marks an edge's old value before it is
//!   overwritten, and a root's object when the root goes, and pushes it on
//!   the mutator's local list, published when full and at safepoints;
//! - what enters the old generation is black: the ranges allocation bumps
//!   through are marked whole when taken (V8's black area), and their
//!   rest cleared at the end;
//! - a scavenge first cancels the job (its runs yield at an object's end),
//!   and posts it again after: nothing the marker holds is young, and the
//!   promoted are black.
//!
//! The marker skips edges to young objects: what young objects reach of
//! the old generation is in the snapshot or new, so marked either way
//! (`details.md` of kun's RFC 0087). At a safepoint after the workers found
//! nothing left, or once the old generation reached its limit, the heap
//! finishes: cancels the job, publishes the mutator's list, and marks what
//! is left with every thread (a joining run of the same job), then sweeps.

use std::cell::Cell;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool as StdBool, Ordering as StdOrdering};
use std::time::Instant;

use super::{Base, SiteTally};
use crate::js::{JsEdge, JsEdgeLocal, JsEdgeSink};
use crate::page::{HEADER_BYTES, MarkTable};
use crate::platform::{Job, JobDelegate, JobHandle, Platform};
use crate::site;
use crate::sync::{self, AtomicBool, AtomicUsize, Mutex, Ordering};
use crate::types::{self, Kind};
use crate::worklist::{Local, Worklist};

/// A marking's work, shared with the threads that mark: a job the
/// platform runs posted (in the background) and, to finish, joined.
pub(crate) struct Marker {
    base: Base,
    marks: Arc<MarkTable>,
    young_pages: Arc<[StdBool]>,
    pub(crate) worklist: Worklist,
    /// In a marking a JS engine drives: the engine, given the JS edges of
    /// the objects scanned.
    js: Option<Arc<dyn JsEdgeSink>>,
    /// Under the verifier, the objects the engine handed over.
    pub(super) handed: Option<Mutex<Vec<usize>>>,
    /// The engine's threads may hand objects over (`hand`): until the
    /// marking ends or the heap goes (`retire`). Handovers in progress,
    /// which `retire` waits for.
    open: AtomicBool,
    handing: AtomicUsize,
    /// Runs join the finishing run, not run in the background.
    joined: AtomicBool,
    /// Background runs in progress: none, with no segment left, is done.
    active: AtomicUsize,
    /// It runs alongside the mutator (posted): the nursery may hold
    /// objects, and its background runs fence.
    concurrent: bool,
    pub(super) scanned: AtomicUsize,
    pub(super) scanned_by_workers: AtomicUsize,
    /// It counts the bytes it scans (`marked_bytes`): a marking a JS engine
    /// drives, which tells the engine the heap's live bytes as it ends.
    counts_bytes: bool,
    /// The bytes of the objects scanned (their slots'), in a marking that
    /// counts them; with the black allocated, the live old generation.
    pub(super) marked_bytes: AtomicUsize,
    /// The background runs' time, in ns (statistics: std's atomic, not
    /// the protocol's).
    pub(super) background_ns: std::sync::atomic::AtomicU64,
    /// The heap's sites, by number: the length of a thread's counts.
    sites: usize,
    /// The objects each site has that the threads marked, added up.
    pub(super) site_marked: Mutex<Vec<usize>>,
    /// The weak edges of the objects scanned whose object wasn't marked
    /// then (cppgc's weak callbacks): the end clears those whose object
    /// still isn't.
    pub(super) weak_slots: Mutex<Vec<usize>>,
    /// The ephemerons scanned whose key wasn't marked then (cppgc's
    /// discovered ephemeron pairs): looked at again once the worklist is
    /// empty, their value marked once their key is; the end clears those
    /// whose key never is.
    pub(super) discovered: Mutex<Vec<usize>>,
}

/// What a run found besides what it marks: weak edges and ephemerons to
/// look at again, by their slots; and the JS edges, given to the engine
/// through its local, made at the first.
#[derive(Default)]
struct Found {
    weak: Vec<usize>,
    discovered: Vec<usize>,
    js: Option<Box<dyn JsEdgeLocal>>,
}

impl Marker {
    #[expect(
        clippy::too_many_arguments,
        reason = "a marking's parts, each from the heap"
    )]
    pub(super) fn new(
        base: Base,
        marks: Arc<MarkTable>,
        young_pages: Arc<[StdBool]>,
        roots: Vec<usize>,
        published: bool,
        sites: usize,
        js: Option<Arc<dyn JsEdgeSink>>,
        verifies: bool,
    ) -> Self {
        let counts_bytes = js.is_some();
        Self {
            base,
            marks,
            young_pages,
            js,
            handed: verifies.then(|| Mutex::new(Vec::new())),
            open: AtomicBool::new(true),
            handing: AtomicUsize::new(0),
            worklist: if published {
                Worklist::with_published(Vec::new(), roots)
            } else {
                Worklist::new(roots)
            },
            joined: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            concurrent: published,
            scanned: AtomicUsize::new(0),
            scanned_by_workers: AtomicUsize::new(0),
            counts_bytes,
            marked_bytes: AtomicUsize::new(0),
            background_ns: std::sync::atomic::AtomicU64::new(0),
            sites,
            site_marked: Mutex::new(vec![0; sites]),
            weak_slots: Mutex::new(Vec::new()),
            discovered: Mutex::new(Vec::new()),
        }
    }

    /// Whether the background runs are done: none runs, and no segment is
    /// left (the mutator's list aside).
    pub(crate) fn drained(&self) -> bool {
        self.worklist.drained(&self.active)
    }

    /// Marks what is left with the platform's threads and the caller's,
    /// again while an ephemeron's key got marked: its value is marked from
    /// (cppgc's loop until no new ephemeron). The joining run is the
    /// worklist's first: it is busy from the start, with its seed, as the
    /// worklist was made (the background runs take segments without
    /// becoming busy); each next one starts with the values.
    pub(super) fn finish(&self, platform: &dyn Platform) {
        self.joined.store(true, Ordering::Relaxed);
        loop {
            platform.run(self);
            assert!(!self.worklist.poisoned(), "a thread that marked panicked");
            let mut values = Vec::new();
            self.resolve(|value| values.push(value));
            if values.is_empty() {
                return;
            }
            self.worklist.reseed(values);
        }
    }

    /// Marks the value of each discovered ephemeron whose key is marked (or
    /// young, which old marking doesn't judge), and gives each value it
    /// marked to `push`; keeps the others. Whether it marked any.
    fn resolve(&self, mut push: impl FnMut(usize)) -> bool {
        let pairs = std::mem::take(&mut *sync::lock(&self.discovered));
        // Each pair's key and value, read before a fence, as edges.
        let read: Vec<(usize, u32, u32)> = pairs
            .into_iter()
            .map(|pair| {
                // SAFETY: an ephemeron of a live object, 4-aligned, in a
                // committed page.
                unsafe {
                    (
                        pair,
                        load_edge(self.ptr(pair)),
                        load_edge(self.ptr(pair).wrapping_add(4)),
                    )
                }
            })
            .collect();
        std::sync::atomic::fence(StdOrdering::Acquire);
        let mut kept = Vec::new();
        let mut marked = false;
        for (pair, key, value) in read {
            let key = key as usize;
            if key == 0 {
                // Cleared by the mutator: nothing to keep.
            } else if self.is_young(key) || self.marks.is_marked(key) {
                let value = value as usize;
                if value != 0 && !self.is_young(value) && self.marks.mark(value) {
                    push(value);
                    marked = true;
                }
            } else {
                kept.push(pair);
            }
        }
        sync::lock(&self.discovered).append(&mut kept);
        marked
    }

    /// A step of the heap's thread in a marking alongside the mutator (the
    /// engine's, between operations): marks from the segments it takes as
    /// a background run does, until it scanned `limit` objects or it is
    /// `deadline` (if any), and publishes what it holds; out of segments, the
    /// discovered ephemerons whose key got marked give more, even as the
    /// limit runs out with the last segment: else the marking would look
    /// done with their values unmarked, left to the pause. The objects it
    /// scanned. Not counted active: only the heap's thread asks whether
    /// the workers are done, and not during a step.
    pub(super) fn step(&self, limit: usize, deadline: Option<Instant>) -> usize {
        let scanned = Cell::new(0usize);
        let mut tally = SiteTally {
            counts: vec![0; self.sites],
            site: 0,
            run: 0,
            bytes: 0,
        };
        let mut found = Found::default();
        let over = || {
            scanned.get() >= limit || deadline.is_some_and(|deadline| Instant::now() >= deadline)
        };
        loop {
            let drained = self
                .worklist
                .run_background(&over, &|| {}, |item, local, _| {
                    // SAFETY: an item is a marked old object, so a live one.
                    unsafe { self.scan::<true, false>(item, local, &mut tally, &mut found) };
                    scanned.set(scanned.get().saturating_add(1));
                });
            if !drained {
                break;
            }
            sync::lock(&self.discovered).append(&mut found.discovered);
            let mut local = Local::new();
            if !self.resolve(|value| local.push(&self.worklist, value)) {
                break;
            }
            self.worklist.flush(&mut local);
            if over() {
                break;
            }
        }
        tally.flush();
        self.add_marked_bytes(tally.bytes);
        if !found.weak.is_empty() {
            sync::lock(&self.weak_slots).append(&mut found.weak);
        }
        if !found.discovered.is_empty() {
            sync::lock(&self.discovered).append(&mut found.discovered);
        }
        if self.sites > 0 {
            site::add_counts(&mut sync::lock(&self.site_marked), &tally.counts);
        }
        self.count_scanned(scanned.get(), false);
        scanned.get()
    }

    /// Counts `scanned` objects marked, by a worker's run or not.
    fn count_scanned(&self, scanned: usize, by_worker: bool) {
        #[expect(
            clippy::disallowed_methods,
            reason = "statistics: it would take 2^64 objects to wrap"
        )]
        {
            self.scanned.fetch_add(scanned, Ordering::Relaxed);
            if by_worker {
                self.scanned_by_workers
                    .fetch_add(scanned, Ordering::Relaxed);
            }
        }
    }

    /// Adds a run's scanned bytes (`counts_bytes`).
    fn add_marked_bytes(&self, bytes: usize) {
        if bytes > 0 {
            #[expect(
                clippy::disallowed_methods,
                reason = "a heap's bytes: at most the cage's 4 GiB a marking"
            )]
            self.marked_bytes.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    fn ptr(&self, offset: usize) -> *const u8 {
        self.base.0.wrapping_add(offset)
    }

    fn is_young(&self, offset: usize) -> bool {
        self.young_pages[crate::page::number_of(offset)].load(StdOrdering::Relaxed)
    }

    /// The engine hands over `object`, of a wrapper it marked: marked, and
    /// pushed on `local` if it wasn't.
    ///
    /// # Safety
    ///
    /// `object` is a live old object of this marking's heap (a type with a
    /// JS edge, which is never young), and the marking runs.
    pub(crate) unsafe fn hand(&self, object: std::ptr::NonNull<u8>, local: &mut Local) {
        let data = object.as_ptr().addr().wrapping_sub(self.base.0.addr());
        // Another heap's object (another cage) is none of this marking's.
        if data >= crate::cage::CAGE_BYTES {
            return;
        }
        // Counted before it looks whether the marking is open, which
        // `retire` closes before it waits for the count: one of them sees
        // the other (both sequentially consistent). A handover after the
        // marking ended, or once the heap is going (its isolate torn down
        // while it marks), touches nothing of the heap's.
        #[expect(
            clippy::disallowed_methods,
            reason = "handovers in progress: at most the engine's threads"
        )]
        self.handing.fetch_add(1, Ordering::SeqCst);
        if self.open.load(Ordering::SeqCst) {
            debug_assert!(!self.is_young(data), "an object the engine holds is old");
            if self.marks.mark(data) {
                if let Some(handed) = &self.handed {
                    sync::lock(handed).push(data);
                }
                local.push(&self.worklist, data);
            }
        }
        self.handing.fetch_sub(1, Ordering::SeqCst);
    }

    /// No more handovers: the marking ended, or the heap is going. Returns
    /// once those in progress are done.
    pub(super) fn retire(&self) {
        self.open.store(false, Ordering::SeqCst);
        while self.handing.load(Ordering::SeqCst) != 0 {
            sync::thread::yield_now();
        }
    }

    /// Marks what the object at `data` points to in the old generation,
    /// counts it for its site in `tally`, and adds to `weak` its weak edges
    /// to old objects not marked yet (those marked stay so; what is written
    /// into a weak edge while marking runs is marked by its end, as all the
    /// mutator holds then is: the snapshot's, new, or read through a weak
    /// edge, which marks it; so the objects allocated black need none of
    /// this, and are never scanned). `YOUNG`: the nursery may hold
    /// objects (a concurrent marking's), whose edges are skipped. `FENCE`:
    /// the mutator runs alongside (a posted run): the edges are read in
    /// batches, each followed by an acquire fence, so that an object
    /// allocated since, published by a release write of the edge after its
    /// mark bit was set, is seen marked (C++'s fence synchronization; an
    /// acquire load of each edge slowed marking chains by a third).
    ///
    /// # Safety
    ///
    /// `data` is a live old object, marked.
    unsafe fn scan<const YOUNG: bool, const FENCE: bool>(
        &self,
        data: usize,
        local: &mut Local,
        tally: &mut SiteTally,
        found: &mut Found,
    ) {
        let header = self.ptr(data).wrapping_sub(HEADER_BYTES).cast::<u32>();
        // SAFETY: a live object's header, in a committed page, written
        // before the object was published.
        let (index, flags) = unsafe { (header.read(), header.add(1).read()) };
        tally.add(flags >> site::SITE_SHIFT);
        if self.counts_bytes {
            tally.bytes = tally.bytes.saturating_add(self.marks.slot_bytes(data));
        }
        // SAFETY: a live object's type.
        let ty = unsafe { types::entry(index) };
        let mut visit = |_: usize, edge: u32| {
            let edge = edge as usize;
            if edge != 0 && (!YOUNG || !self.is_young(edge)) && self.marks.mark(edge) {
                local.push(&self.worklist, edge);
            }
        };
        match ty.info.kind() {
            Kind::Fixed => {
                self.edges::<FENCE>(data, ty.edges.iter().map(|&at| at as usize), &mut visit);
                if !ty.weak.is_empty() {
                    let mut record = |at: usize, edge: u32| {
                        let edge = edge as usize;
                        if edge != 0
                            && (!YOUNG || !self.is_young(edge))
                            && !self.marks.is_marked(edge)
                        {
                            found.weak.push(data.saturating_add(at));
                        }
                    };
                    self.edges::<FENCE>(data, ty.weak.iter().map(|&at| at as usize), &mut record);
                }
                if !ty.ephemerons.is_empty() {
                    // Each ephemeron's key, then its value: the value marked
                    // if the key is (or young), else the pair discovered.
                    let mut key = None;
                    let mut pair = |at: usize, edge: u32| match key.take() {
                        None => key = Some(edge as usize),
                        Some(0) => {}
                        Some(k) if (YOUNG && self.is_young(k)) || self.marks.is_marked(k) => {
                            visit(at, edge);
                        }
                        Some(_) => found
                            .discovered
                            .push(data.saturating_add(at).saturating_sub(4)),
                    };
                    self.edges::<FENCE>(
                        data,
                        ty.ephemerons
                            .iter()
                            .flat_map(|&at| [at as usize, (at as usize).saturating_add(4)]),
                        &mut pair,
                    );
                }
                if !ty.js_edges.is_empty()
                    && let Some(js) = &self.js
                {
                    // The heap never reads a JS edge: the engine marks what
                    // it holds.
                    let local = found.js.get_or_insert_with(|| js.local());
                    for &at in &ty.js_edges {
                        let edge = self
                            .ptr(data)
                            .wrapping_add(at as usize)
                            .cast_mut()
                            .cast::<JsEdge>();
                        // SAFETY: a JS edge of an object this marking marked,
                        // live and old (a type with one is allocated old) until
                        // the marking ends; its bytes are the engine's. The
                        // cage's base isn't null.
                        unsafe { local.mark(std::ptr::NonNull::new_unchecked(edge)) };
                    }
                }
            }
            Kind::MemberArray => {
                // SAFETY: an array's capacity, fixed once it is published.
                let capacity = unsafe { self.ptr(data).cast::<u32>().read() } as usize;
                self.edges::<FENCE>(
                    data,
                    (1..=capacity).map(|i| i.saturating_mul(4)),
                    &mut visit,
                );
            }
            Kind::Raw => {
                // SAFETY: an object of the type, its length (an array's)
                // written before it was published, and fixed.
                let edges = unsafe { ty.raw_edges(self.ptr(data)) };
                self.edges::<FENCE>(data, edges, &mut visit);
            }
        }
    }

    /// Reads the edges of the object at `data` at `offsets` from it, and
    /// visits each with its offset (with `FENCE`, in batches, each read
    /// before a fence).
    fn edges<const FENCE: bool>(
        &self,
        data: usize,
        mut offsets: impl Iterator<Item = usize>,
        visit: &mut impl FnMut(usize, u32),
    ) {
        if !FENCE {
            for at in offsets {
                // SAFETY: an edge of a live object, by its layout.
                visit(at, unsafe { load_edge(self.ptr(data).wrapping_add(at)) });
            }
            return;
        }
        let mut batch = [(0usize, 0u32); BATCH];
        loop {
            let mut taken = 0usize;
            for (slot, at) in batch.iter_mut().zip(&mut offsets) {
                // SAFETY: as above.
                *slot = (at, unsafe { load_edge(self.ptr(data).wrapping_add(at)) });
                taken = taken.saturating_add(1);
            }
            if taken == 0 {
                return;
            }
            std::sync::atomic::fence(StdOrdering::Acquire);
            for &(at, edge) in &batch[..taken] {
                visit(at, edge);
            }
        }
    }
}

/// Edges a posted run reads between two fences.
const BATCH: usize = 16;

/// Objects a background run scans between two counts of its progress.
const PROGRESS: usize = 512;

/// The edge at `at`, as the threads that mark read it: atomically, as the
/// mutator writes it (relaxed; a posted run fences after a batch).
///
/// # Safety
///
/// `at` is a `Member` or an array's edge, 4-aligned, in a committed page.
unsafe fn load_edge(at: *const u8) -> u32 {
    // SAFETY: as the caller promises; an edge is only written atomically
    // while anything may read it so.
    unsafe { std::sync::atomic::AtomicU32::from_ptr(at.cast_mut().cast()) }
        .load(StdOrdering::Relaxed)
}

impl Job for Marker {
    fn run(&self, delegate: &dyn JobDelegate) {
        let joining = delegate.is_joining_thread();
        let mut scanned = 0usize;
        // A background run counts its objects every `PROGRESS` as it goes,
        // for a driven marking's schedule; the others, at the end.
        let mut background = false;
        let mut tally = SiteTally {
            counts: vec![0; self.sites],
            site: 0,
            run: 0,
            bytes: 0,
        };
        // Each scan's finds; a background run hands its discovered
        // ephemerons over between its turns on the worklist.
        let found = std::cell::RefCell::new(Found::default());
        let published = &|| delegate.notify_concurrency_increase();
        if (joining || self.joined.load(Ordering::Relaxed)) && self.concurrent {
            self.worklist.run(joining, published, |data, local, _| {
                // SAFETY: an item is a marked old object, so a live one.
                unsafe {
                    self.scan::<true, false>(data, local, &mut tally, &mut found.borrow_mut())
                };
                scanned = scanned.saturating_add(1);
            });
        } else if joining || self.joined.load(Ordering::Relaxed) {
            self.worklist.run(joining, published, |data, local, _| {
                // SAFETY: as above.
                unsafe {
                    self.scan::<false, false>(data, local, &mut tally, &mut found.borrow_mut())
                };
                scanned = scanned.saturating_add(1);
            });
        } else {
            background = true;
            let mut visit = |data: usize, local: &mut Local, _: &Worklist| {
                // SAFETY: as above.
                unsafe {
                    self.scan::<true, true>(data, local, &mut tally, &mut found.borrow_mut())
                };
                scanned = scanned.saturating_add(1);
                if scanned.is_multiple_of(PROGRESS) {
                    self.count_scanned(PROGRESS, true);
                }
            };
            #[expect(
                clippy::disallowed_methods,
                reason = "a count of the runs in progress, at most the workers"
            )]
            self.active.fetch_add(1, Ordering::SeqCst);
            let started = std::time::Instant::now();
            // Out of segments (not asked to yield): the discovered
            // ephemerons whose key got marked give more to mark.
            while self.worklist.run_background(
                &|| delegate.should_yield(),
                &|| delegate.notify_concurrency_increase(),
                &mut visit,
            ) {
                sync::lock(&self.discovered).append(&mut found.borrow_mut().discovered);
                let mut local = Local::new();
                if !self.resolve(|value| local.push(&self.worklist, value)) {
                    break;
                }
                self.worklist.flush(&mut local);
            }
            let ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            #[expect(
                clippy::disallowed_methods,
                reason = "statistics: it would take 585 years of runs to wrap"
            )]
            self.background_ns
                .fetch_add(ns, std::sync::atomic::Ordering::Relaxed);
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
        tally.flush();
        self.add_marked_bytes(tally.bytes);
        let mut found = found.into_inner();
        if !found.weak.is_empty() {
            sync::lock(&self.weak_slots).append(&mut found.weak);
        }
        if !found.discovered.is_empty() {
            sync::lock(&self.discovered).append(&mut found.discovered);
        }
        if self.sites > 0 {
            site::add_counts(&mut sync::lock(&self.site_marked), &tally.counts);
        }
        let left = if background {
            scanned % PROGRESS
        } else {
            scanned
        };
        self.count_scanned(left, !joining);
    }

    fn max_concurrency(&self, active: usize) -> usize {
        // A worker for each segment to take, besides those working.
        active.saturating_add(self.worklist.available())
    }
}

/// A concurrent marking, as the heap's thread keeps it.
pub(super) struct Marking {
    pub(super) marker: Arc<Marker>,
    /// The posted job; none while a scavenge runs, or to finish.
    pub(super) handle: Option<Box<dyn JobHandle>>,
    /// The barrier's objects not published yet.
    local: Local,
    /// The objects before marking and their bytes, to poison the dead
    /// ones (debug builds).
    pub(super) before: Vec<(usize, usize)>,
    /// Under the verifier: what the roots reached at the start, what weak
    /// reads reached when they marked their objects, and the objects
    /// allocated in the old generation since (all marked, and the only
    /// marked objects the roots may no longer reach).
    pub(super) floating: Option<HashSet<usize>>,
    /// In a marking an engine drives: when it started, how much the heap's
    /// thread marks at each step, what it marked in them, and the objects
    /// marked before the engine's pause (its first atomic step).
    pub(super) driven: Option<Driven>,
}

pub(super) struct Driven {
    pub(super) started: Instant,
    pub(super) schedule: super::schedule::Schedule,
    pub(super) stepped: usize,
    pub(super) before_pause: Option<usize>,
}

impl Marking {
    pub(super) fn new(
        marker: Arc<Marker>,
        before: Vec<(usize, usize)>,
        floating: Option<HashSet<usize>>,
    ) -> Self {
        Self {
            marker,
            handle: None,
            local: Local::new(),
            before,
            floating,
            driven: None,
        }
    }

    /// Pushes `data`, marked just now by the barrier.
    pub(super) fn push(&mut self, data: usize) {
        self.local.push(&self.marker.worklist, data);
        if self.local.take_published()
            && let Some(handle) = &self.handle
        {
            handle.notify_concurrency_increase();
        }
    }

    /// Publishes the barrier's objects.
    pub(super) fn flush(&mut self) {
        if !self.local.is_empty() {
            self.marker.worklist.flush(&mut self.local);
            if let Some(handle) = &self.handle {
                handle.notify_concurrency_increase();
            }
        }
    }

    /// Whether everything is marked: the background runs are done, and
    /// the barrier holds nothing.
    pub(super) fn done(&self) -> bool {
        self.local.is_empty() && self.marker.drained()
    }

    /// Tells the job of segments published by other threads (the engine's).
    pub(super) fn notify(&self) {
        if let Some(handle) = &self.handle {
            handle.notify_concurrency_increase();
        }
    }

    /// Posts the job (again) to `platform`.
    pub(super) fn post(&mut self, platform: &dyn Platform) {
        debug_assert!(self.handle.is_none(), "posted twice");
        self.handle = platform.post_job(self.marker.clone());
    }

    /// Stops the job: its runs yield and return.
    pub(super) fn cancel(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.cancel();
        }
    }

    /// What a weak read reached when it marked its object, under the
    /// verifier.
    pub(super) fn floats(&mut self, reached: HashSet<usize>) {
        if let Some(floating) = &mut self.floating {
            floating.extend(reached);
        }
    }

    /// An object allocated in the old generation while marking, under the
    /// verifier.
    pub(super) fn allocated(&mut self, data: usize) {
        if let Some(floating) = &mut self.floating {
            floating.insert(data);
        }
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use loom::sync::Arc;
    use loom::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

    /// The publication of an object allocated while marking runs, as the
    /// mutator (`Inner::write_edge`: the object written, its mark bit set
    /// with its black range, then the edge written with release) and a
    /// posted run (`Marker::edges`: the edge read relaxed, then an acquire
    /// fence) do it, in every interleaving: a marker that sees the edge
    /// sees the object marked and whole, so it never scans it half-made.
    #[test]
    fn an_edge_written_with_release_publishes_its_objects_mark_bit() {
        loom::model(|| {
            let edge = Arc::new(AtomicU32::new(0));
            let mark = Arc::new(AtomicU64::new(0));
            let field = Arc::new(AtomicU32::new(0));
            let mutator = {
                let (edge, mark, field) = (edge.clone(), mark.clone(), field.clone());
                loom::thread::spawn(move || {
                    mark.fetch_or(1, Ordering::Relaxed);
                    field.store(7, Ordering::Relaxed);
                    edge.store(1, Ordering::Release);
                })
            };
            let seen = edge.load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            if seen == 1 {
                assert_eq!(mark.load(Ordering::Relaxed) & 1, 1, "seen unmarked");
                assert_eq!(field.load(Ordering::Relaxed), 7, "seen half-made");
            }
            mutator.join().unwrap();
        });
    }
}
