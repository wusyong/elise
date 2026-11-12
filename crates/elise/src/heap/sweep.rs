//! The old generation's sweep alongside the mutator (cppgc's concurrent
//! sweeper). A full GC's pause hands the size-class pages to this job,
//! which finds each one's free ranges and live objects from its marks
//! alone (`MarkTable::sweep`) and lists what it found; the heap's thread
//! applies that to its pages at safepoints, and when a class runs out of
//! pages to allocate on (sweeping a page of the class itself if none is
//! done: V8's allocation sweeps a page, cppgc's
//! `SweepForAllocationIfRunning`). The pages stay the heap's thread's, so
//! allocation doesn't change: V8's sweeper writes its pages' free lists
//! itself, as its pages take several threads' allocations, which a heap
//! here doesn't. Before the next marking (and a full GC), the rest is
//! swept in that pause, on the platform's threads.
//!
//! The job also runs the finalizers of the objects whose drop glue is
//! `External`s' (`Send` values: V8 frees ArrayBuffers' backing stores on
//! a worker so; cppgc runs every finalizer on the heap's thread): their
//! pages, and their dead large objects, are its alone (the heap's thread
//! doesn't sweep them for an allocation, mid-operation). Before a page's
//! finalizers run, its dead objects' bits are cleared, so a finalizer that
//! panics leaves the rest unfinalized, never finalized twice; the page goes
//! back to wait for a sweep, and the panic reaches the heap's thread when
//! it next waits for the job. A page with an object to finalize on the
//! heap's thread (a `LocalExternal`'s) is that thread's to sweep, at the
//! safepoint after the pause; so is such a dead large object. Other large
//! objects are freed in the pause.

use super::{Base, POISON};
use crate::page::{HEADER_BYTES, MarkTable, Swept};
use crate::platform::{Job, JobDelegate, Tracer};
use crate::sync::{self, AtomicUsize, Mutex, Ordering};
use crate::types;

pub(super) struct Sweeper {
    marks: std::sync::Arc<MarkTable>,
    base: Base,
    /// The heap's trace: a run on a worker is a phase of it.
    tracer: Tracer,
    state: Mutex<State>,
    /// The pages and large objects not taken yet, for `max_concurrency`,
    /// which mustn't lock.
    left: AtomicUsize,
    /// `state.swept`'s and `state.freed`'s lengths: a safepoint locks only
    /// if some are.
    swept: AtomicUsize,
    /// The background runs' time, in ns (statistics: std's atomic, not
    /// the protocol's).
    pub(super) background_ns: std::sync::atomic::AtomicU64,
}

/// What a GC's pause hands to the sweep.
pub(super) struct Handed {
    /// By size class, the pages with no object to finalize.
    pub(super) pages: Vec<Vec<usize>>,
    /// The pages whose objects to finalize are all the sweep's.
    pub(super) finalizing: Vec<usize>,
    /// The dead large objects to finalize, by first page.
    pub(super) large: Vec<usize>,
}

impl Handed {
    pub(super) fn new(classes: usize) -> Self {
        Self {
            pages: vec![Vec::new(); classes],
            finalizing: Vec::new(),
            large: Vec::new(),
        }
    }

    fn len(&self) -> usize {
        self.pages
            .iter()
            .map(Vec::len)
            .sum::<usize>()
            .saturating_add(self.finalizing.len())
            .saturating_add(self.large.len())
    }
}

/// A page or a large object to sweep.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Item {
    /// A size-class page, of the class (none for a page with finalizers).
    Page(usize, Option<usize>),
    Large(usize),
}

struct State {
    handed: Handed,
    /// What threads sweep now.
    busy: Vec<Item>,
    /// The pages swept, their results not applied yet.
    swept: Vec<(usize, Swept)>,
    /// The large objects finalized, their pages not freed yet.
    freed: Vec<usize>,
}

impl Sweeper {
    pub(super) fn new(marks: std::sync::Arc<MarkTable>, base: Base, tracer: Tracer) -> Self {
        Self {
            marks,
            base,
            tracer,
            state: Mutex::new(State {
                handed: Handed::new(0),
                busy: Vec::new(),
                swept: Vec::new(),
                freed: Vec::new(),
            }),
            left: AtomicUsize::new(0),
            swept: AtomicUsize::new(0),
            background_ns: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Hands what a GC's marking left to sweep: their marks don't change
    /// until their results are applied.
    pub(super) fn hand(&self, handed: Handed) {
        let mut state = sync::lock(&self.state);
        debug_assert!(state.is_done(), "the last sweep is done");
        self.left.store(handed.len(), Ordering::Relaxed);
        state.handed = handed;
    }

    /// The pages swept since the last call, with what was found, and the
    /// large objects finalized.
    pub(super) fn take_swept(&self) -> (Vec<(usize, Swept)>, Vec<usize>) {
        if self.swept.load(Ordering::Relaxed) == 0 {
            return (Vec::new(), Vec::new());
        }
        let mut state = sync::lock(&self.state);
        self.swept.store(0, Ordering::Relaxed);
        (
            std::mem::take(&mut state.swept),
            std::mem::take(&mut state.freed),
        )
    }

    /// A page of `class` not swept yet, with nothing to finalize, for the
    /// heap's thread to sweep: no one else will.
    pub(super) fn claim(&self, class: usize) -> Option<usize> {
        let mut state = sync::lock(&self.state);
        let number = state.handed.pages.get_mut(class)?.pop()?;
        self.left.fetch_sub(1, Ordering::Relaxed);
        Some(number)
    }

    /// Whether everything handed was swept and taken.
    pub(super) fn is_done(&self) -> bool {
        sync::lock(&self.state).is_done()
    }

    /// Whether page `number` (a size class's, or a large object's first)
    /// waits to be swept, or to be applied (the heap verifier's check).
    pub(super) fn holds(&self, number: usize) -> bool {
        let state = sync::lock(&self.state);
        state
            .handed
            .pages
            .iter()
            .any(|pages| pages.contains(&number))
            || state.handed.finalizing.contains(&number)
            || state.handed.large.contains(&number)
            || state
                .busy
                .iter()
                .any(|&item| matches!(item, Item::Page(at, _) | Item::Large(at) if at == number))
            || state.swept.iter().any(|&(swept, _)| swept == number)
            || state.freed.contains(&number)
    }

    /// The next item to sweep: a large object, a page to finalize, any
    /// class's page.
    fn take(&self) -> Option<Item> {
        let mut state = sync::lock(&self.state);
        let handed = &mut state.handed;
        let item = handed
            .large
            .pop()
            .map(Item::Large)
            .or_else(|| {
                handed
                    .finalizing
                    .pop()
                    .map(|number| Item::Page(number, None))
            })
            .or_else(|| {
                handed
                    .pages
                    .iter_mut()
                    .enumerate()
                    .find_map(|(class, pages)| Some(Item::Page(pages.pop()?, Some(class))))
            })?;
        state.busy.push(item);
        self.left.fetch_sub(1, Ordering::Relaxed);
        Some(item)
    }

    /// Runs the finalizers of the item's dead objects (their bits cleared
    /// first), poisons them (debug builds), and sweeps a page by its marks.
    fn sweep(&self, item: Item) -> Option<Swept> {
        let number = match item {
            Item::Page(number, _) | Item::Large(number) => number,
        };
        let dead = self.marks.take_dead_finalizable(number);
        for &data in &dead {
            let bytes = self.marks.slot_bytes(data).saturating_sub(HEADER_BYTES);
            // SAFETY: a dead object with drop glue, whole until now, of a
            // page this thread alone has (the heap's thread doesn't touch
            // it until its result is applied), in the cage, which outlives
            // the job; its bit cleared, so finalized once. Its drop glue is
            // `External`s', of `Send` values (a page or a large object with
            // a `LocalExternal`'s isn't handed to the job).
            unsafe {
                let header = self.base.0.wrapping_add(data).wrapping_sub(HEADER_BYTES);
                let drop = types::info(header.cast::<u32>().read())
                    .drop_fn()
                    .expect("a finalizable object has drop glue");
                drop(self.base.0.wrapping_add(data));
                if cfg!(debug_assertions) {
                    std::ptr::write_bytes(self.base.0.wrapping_add(data), POISON, bytes);
                }
            }
        }
        match item {
            Item::Large(_) => None,
            Item::Page(..) => Some(self.marks.sweep(number)),
        }
    }
}

impl State {
    fn is_done(&self) -> bool {
        self.handed.len() == 0
            && self.busy.is_empty()
            && self.swept.is_empty()
            && self.freed.is_empty()
    }
}

/// Puts a taken item back to wait for a sweep if its sweep panics (a
/// finalizer's): it is swept again, its finalized objects no longer
/// finalizable.
struct Taken<'a> {
    sweeper: &'a Sweeper,
    item: Item,
}

impl Drop for Taken<'_> {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        let mut state = sync::lock(&self.sweeper.state);
        state.busy.retain(|&busy| busy != self.item);
        match self.item {
            Item::Large(number) => state.handed.large.push(number),
            Item::Page(number, None) => state.handed.finalizing.push(number),
            Item::Page(number, Some(class)) => state.handed.pages[class].push(number),
        }
        #[expect(
            clippy::disallowed_methods,
            reason = "the items left to sweep, at most the pages, taken back one"
        )]
        self.sweeper.left.fetch_add(1, Ordering::Relaxed);
    }
}

impl Job for Sweeper {
    fn run(&self, delegate: &dyn JobDelegate) {
        self.tracer
            .in_worker_span(delegate, "Gc.ConcurrentSweep", || {
                let joining = delegate.is_joining_thread();
                let started = std::time::Instant::now();
                while joining || !delegate.should_yield() {
                    let Some(item) = self.take() else {
                        break;
                    };
                    let taken = Taken {
                        sweeper: self,
                        item,
                    };
                    let swept = self.sweep(item);
                    std::mem::forget(taken);
                    let mut state = sync::lock(&self.state);
                    state.busy.retain(|&busy| busy != item);
                    match (item, swept) {
                        (Item::Page(number, _), Some(swept)) => state.swept.push((number, swept)),
                        (Item::Large(number), _) => state.freed.push(number),
                        (Item::Page(..), None) => unreachable!("a page's sweep has a result"),
                    }
                    self.swept.store(
                        state.swept.len().saturating_add(state.freed.len()),
                        Ordering::Relaxed,
                    );
                }
                if !joining {
                    let ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
                    #[expect(
                        clippy::disallowed_methods,
                        reason = "statistics: it would take 585 years of runs to wrap"
                    )]
                    self.background_ns
                        .fetch_add(ns, std::sync::atomic::Ordering::Relaxed);
                }
            })
    }

    fn max_concurrency(&self, _: usize) -> usize {
        // One worker, while pages are left (cppgc's concurrent sweeper):
        // a page takes some 0.3 µs, less than waking a worker, and more
        // would contend for the lists (each its own page at a time).
        usize::from(self.left.load(Ordering::Relaxed) > 0)
    }
}
