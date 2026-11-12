//! The young generation (V8's `Scavenger`): the nursery's pages, bumped
//! through with objects of any size; the write barrier that records an
//! old object's edge to a young one in the old page's slot set; and the
//! scavenge, at a safepoint, on the platform's threads.
//!
//! A scavenge copies what the roots and the slot sets reach out of the
//! nursery's pages (from-space): an object copied once is aged in its
//! header and stays young (to-space); copied again, it goes to the old
//! generation (promoted), as an object that survived once does in V8's
//! semi-space. A full GC's scavenge promotes every survivor. Each copy is
//! scanned in turn, its edges to from-space copied too. Dead objects are
//! never read, but those with drop glue, which the nursery lists as they
//! are allocated: those not forwarded are finalized. From-space's pages go
//! back to the pool.
//!
//! The work is a `Job` of the heap's platform, as marking's: the copies to
//! scan go through the segmented worklist, the slot sets a page at a time,
//! and each thread copies into its own to-space buffer (a LAB, cut from the
//! young pages under a lock) and its own cursor of each size class (a range
//! taken under the lock). Two threads may copy the same object: each copies
//! it, then swaps the old copy's header (8 bytes, one atomic) to where its
//! copy is, with the flag `FORWARDED` (V8's map word); the loser takes the
//! winner's and gives its own copy back if it is the last of its buffer,
//! else leaves it, a whole object no edge points to. A LAB's unused end is
//! a filler (a header whose type is `FILLER` and whose flags are its bytes),
//! so that a young page is objects and fillers from its start to its top.

use std::sync::atomic::{AtomicBool as StdBool, AtomicU64 as StdU64, Ordering as StdOrdering};
use std::time::Instant;

use super::{
    Base, Cursor, HEADER_BYTES, Inner, POISON, PageState, ScavengeRecord, Space, elapsed_ns,
    free_page, refill,
};
use crate::cage::Cage;
use crate::page::{self, MarkTable, PAGE_BYTES, SlotSet};
use crate::platform::{Job, JobDelegate, Platform, Tracer};
use crate::site::{self, ScavengeCounts};
use crate::sync::{self, AtomicUsize, Mutex, Ordering};
use crate::types::{self, Kind};
use crate::worklist::{Local, SEGMENT, Worklist};

/// A header's flags: the object was copied, and its first word is where to.
pub(super) const FORWARDED: u32 = 1;
/// The object survived a scavenge: the next promotes it.
pub(super) const AGED: u32 = 2;
/// The flags a copy keeps: its site's number.
pub(super) const SITE_BITS: u32 = !0 << site::SITE_SHIFT;
/// A header's type for free bytes on a young page: its flags are their
/// count, header included.
pub(super) const FILLER: u32 = u32::MAX;
/// The to-space a scavenge's thread takes at a time.
const LAB_BYTES: usize = 32 << 10;

/// A worklist item's tag, in the bits an offset doesn't use: a copy in the
/// old generation (data offsets are 8-aligned), or the index of a slot
/// set's page (shifted past the tag). A copy in to-space has none.
const OLD: usize = 1;
const SLOTS: usize = 2;
/// The item of the roots, the joining thread's first.
const ROOTS: usize = usize::MAX;

/// The nursery: its pages, what a scavenge needs of it, and its size (V8's
/// `Heap::ResizeNewSpace`): it doubles each time the scavenges' survivors
/// add up to what it holds, up to its largest, and halves at a full GC
/// after scavenges that kept under a sixteenth of what they saw, down to
/// its first size.
pub(crate) struct Nursery {
    /// Pages it may hold before an allocation goes to the old generation.
    capacity_pages: usize,
    first_pages: usize,
    max_pages: usize,
    /// Bytes the scavenges kept (copied or promoted) since it last grew.
    survived_since_growing: usize,
    /// Bytes the scavenges saw, and kept, since the last full GC.
    seen_since_full_gc: usize,
    kept_since_full_gc: usize,
    /// Its pages, in the order they were taken.
    pub(super) pages: Vec<usize>,
    /// Its objects with drop glue (their data's offsets).
    pub(super) finalizable: Vec<usize>,
    /// An allocation found it full and went to the old generation: the
    /// next safepoint scavenges.
    pub(super) full: bool,
}

impl Nursery {
    /// A nursery of `bytes` (whole pages) growing up to `max_bytes`, or
    /// none for 0.
    pub(super) fn new(bytes: usize, max_bytes: usize) -> Self {
        let pages = bytes.div_ceil(PAGE_BYTES);
        Self {
            capacity_pages: pages,
            first_pages: pages,
            max_pages: max_bytes.div_ceil(PAGE_BYTES).max(pages),
            survived_since_growing: 0,
            seen_since_full_gc: 0,
            kept_since_full_gc: 0,
            pages: Vec::new(),
            finalizable: Vec::new(),
            full: false,
        }
    }

    pub(super) fn capacity_bytes(&self) -> usize {
        self.capacity_pages.saturating_mul(PAGE_BYTES)
    }

    /// A scavenge on its own saw `seen` bytes of the nursery and kept
    /// `kept`: it doubles if the kept add up to what it holds.
    fn scavenged(&mut self, seen: usize, kept: usize) {
        self.seen_since_full_gc = self.seen_since_full_gc.saturating_add(seen);
        self.kept_since_full_gc = self.kept_since_full_gc.saturating_add(kept);
        self.survived_since_growing = self.survived_since_growing.saturating_add(kept);
        if self.survived_since_growing > self.capacity_bytes() {
            self.capacity_pages = self.capacity_pages.saturating_mul(2).min(self.max_pages);
            self.survived_since_growing = 0;
        }
    }

    /// A full GC starts: it halves if the scavenges since the last kept
    /// under a sixteenth of what they saw.
    pub(super) fn resize_at_full_gc(&mut self) {
        if self.kept_since_full_gc.saturating_mul(16) < self.seen_since_full_gc {
            self.capacity_pages = (self.capacity_pages / 2).max(self.first_pages);
            // Counted against the larger nursery: the smaller starts over,
            // or it would double straight back.
            self.survived_since_growing = 0;
        }
        self.seen_since_full_gc = 0;
        self.kept_since_full_gc = 0;
    }
}

/// A header's two words: its first (a type's index, or where the object
/// went) and its flags.
fn split(header: u64) -> (u32, u32) {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the two halves of a 64-bit word"
    )]
    let words = (header as u32, (header >> 32) as u32);
    words
}

fn join(first: u32, flags: u32) -> u64 {
    u64::from(first) | u64::from(flags) << 32
}

/// An object's header as the scavenge's threads swap it: one 64-bit
/// atomic. std's for a header in the cage, loom's for the model of it.
pub(super) trait HeaderWord {
    fn load(&self) -> u64;
    /// Swaps `seen` for `new`, or gives what is there instead.
    fn swap_if(&self, seen: u64, new: u64) -> Result<(), u64>;
}

impl HeaderWord for StdU64 {
    fn load(&self) -> u64 {
        self.load(StdOrdering::Acquire)
    }

    fn swap_if(&self, seen: u64, new: u64) -> Result<(), u64> {
        self.compare_exchange(seen, new, StdOrdering::AcqRel, StdOrdering::Acquire)
            .map(|_| ())
    }
}

/// What became of an object a thread set out to copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Forward {
    /// It was copied already, to there.
    Already(u32),
    /// This thread's copy is the one.
    Won(u32),
    /// Another thread's copy (`winner`) is the one: this thread's (`ours`)
    /// isn't.
    Lost { winner: u32, ours: u32 },
}

impl Forward {
    /// Where the object is now.
    pub(super) fn to(self) -> u32 {
        match self {
            Forward::Already(to) | Forward::Won(to) | Forward::Lost { winner: to, .. } => to,
        }
    }
}

/// Copies the object whose header is `header`, unless it was: `copy` makes
/// the copy from the header's words (a type's index and flags) and gives
/// where it is; then the header is swapped to where, flagged `FORWARDED`,
/// if no other thread swapped it first.
pub(super) fn forward(header: &impl HeaderWord, copy: impl FnOnce(u32, u32) -> u32) -> Forward {
    let seen = header.load();
    let (index, flags) = split(seen);
    if flags & FORWARDED != 0 {
        return Forward::Already(index);
    }
    let ours = copy(index, flags);
    match header.swap_if(seen, join(ours, FORWARDED)) {
        Ok(()) => Forward::Won(ours),
        Err(now) => {
            let (winner, flags) = split(now);
            assert!(flags & FORWARDED != 0, "only forwarding changes a header");
            Forward::Lost { winner, ours }
        }
    }
}

/// The bytes of the object (or filler) at `data` whose header's words are
/// `index` and `flags`, header included, rounded up to 8: what it took in
/// the nursery, and what a copy takes.
///
/// # Safety
///
/// `index` and `flags` are the header of an object or filler of the
/// nursery, at `data` in the cage at `base`.
unsafe fn object_bytes(base: *mut u8, data: usize, index: u32, flags: u32) -> usize {
    if index == FILLER {
        return flags as usize;
    }
    // SAFETY: as the caller promises, an object's type, which a GC may read.
    let ty = unsafe { types::entry(index) };
    let size = match ty.info.kind() {
        Kind::MemberArray => {
            // SAFETY: an array's capacity, in its first word.
            let capacity = unsafe { base.wrapping_add(data).cast::<u32>().read() } as usize;
            capacity.saturating_add(1).saturating_mul(4)
        }
        Kind::Fixed => ty.info.size() as usize,
        // SAFETY: an object of the type, its length (an array's) written.
        Kind::Raw => unsafe { ty.raw_size(base.wrapping_add(data)) },
    };
    size.saturating_add(HEADER_BYTES)
        .checked_next_multiple_of(8)
        .expect("an object fits the cage")
}

/// Writes a filler over `bytes` (a multiple of 8) at `at`, a header's
/// offset, if any.
fn fill(base: *mut u8, at: usize, bytes: usize) {
    if bytes == 0 {
        return;
    }
    let header = base.wrapping_add(at).cast::<u32>();
    // SAFETY: at least a header's room of a young page's free bytes.
    unsafe {
        header.write(FILLER);
        header
            .add(1)
            .write(u32::try_from(bytes).expect("a filler is in a page"));
    }
}

impl Inner {
    /// Whether `offset` is on a young page.
    #[inline]
    pub(crate) fn is_young(&self, offset: usize) -> bool {
        self.young_pages[page::number_of(offset)].load(StdOrdering::Relaxed)
    }

    /// Room in the nursery for an object of `bytes`, header included: its
    /// data's offset, or `None` if the nursery is full (it then is till the
    /// next scavenge).
    pub(super) fn allocate_young(&self, bytes: usize) -> Option<usize> {
        let bytes = bytes
            .checked_next_multiple_of(8)
            .expect("an object fits a slot");
        let mut cursor = self.young_cursor.get();
        if cursor.limit.saturating_sub(cursor.next) < bytes {
            cursor = self.young_page()?;
        }
        let header = cursor.next;
        // `bytes` fits between next and limit, in one page.
        cursor.next = header.saturating_add(bytes);
        self.young_cursor.set(cursor);
        self.young_used
            .set(self.young_used.get().saturating_add(bytes));
        crate::asan::unpoison(self.ptr(header), bytes);
        Some(header.saturating_add(HEADER_BYTES))
    }

    /// The nursery's next page to bump through, the current one's end
    /// kept: `None` if the nursery has all its pages. Out of line, as
    /// `refill`.
    #[cold]
    #[inline(never)]
    fn young_page(&self) -> Option<Cursor> {
        self.close_young_page();
        let mut young = self.young.borrow_mut();
        if young.pages.len() >= young.capacity_pages {
            // No nursery: no scavenge to wait for.
            young.full = young.capacity_pages > 0;
            return None;
        }
        let mut space = self.space.borrow_mut();
        let number = self.free_page(&mut space);
        let start = page::start_of(number);
        space.states[number] = PageState::Young { top: start };
        self.young_pages[number].store(true, StdOrdering::Relaxed);
        young.pages.push(number);
        Some(Cursor {
            next: start,
            limit: start.saturating_add(PAGE_BYTES),
            page: number,
            ..Cursor::default()
        })
    }

    /// Keeps the end of the nursery's page being bumped through in its
    /// state.
    fn close_young_page(&self) {
        let cursor = self.young_cursor.get();
        if let Some(PageState::Young { top }) = self.space.borrow_mut().states.get_mut(cursor.page)
        {
            *top = cursor.next;
        }
    }

    /// The write barrier: `value` was written at `slot`, an edge. An old
    /// object's edge to a young one goes into its page's slot set. Both
    /// checks read the table of young pages.
    #[inline]
    pub(crate) fn write_barrier(&self, slot: *const u8, value: u32) {
        if value != 0 && self.is_young(value as usize) {
            let slot = self.offset_of(slot);
            if !self.is_young(slot) {
                self.remember_written(slot);
            }
        }
    }

    /// `remember` for the write barrier: out of the way of its inlined
    /// checks, most writes recording nothing.
    #[cold]
    #[inline(never)]
    fn remember_written(&self, slot: usize) {
        self.remember(slot);
    }

    /// The write barrier of a weak edge: an old object's to a young one goes
    /// into its page's weak slot set.
    #[inline]
    pub(crate) fn weak_barrier(&self, slot: *const u8, value: u32) {
        if value != 0 && self.is_young(value as usize) {
            let slot = self.offset_of(slot);
            if !self.is_young(slot) {
                self.remember_in(&self.remembered_weak, slot);
            }
        }
    }

    /// The write barrier of an ephemeron's key: an old object's to a young
    /// one goes into its page's ephemeron slot set.
    #[inline]
    pub(crate) fn ephemeron_barrier(&self, slot: *const u8, key: u32) {
        if key != 0 && self.is_young(key as usize) {
            let slot = self.offset_of(slot);
            if !self.is_young(slot) {
                self.remember_in(&self.remembered_ephemeron, slot);
            }
        }
    }

    /// Writes `raw` at the ephemeron key `edge`, as a weak edge (no SATB
    /// barrier, a release while marking runs), with its barrier.
    #[inline]
    pub(crate) fn write_ephemeron_key(&self, edge: &std::sync::atomic::AtomicU32, raw: u32) {
        if self.marking_active.get() {
            edge.store(raw, StdOrdering::Release);
        } else {
            edge.store(raw, StdOrdering::Relaxed);
        }
        self.ephemeron_barrier(edge.as_ptr().cast(), raw);
    }

    /// Writes `raw` at the weak edge `edge`: a release while marking runs,
    /// as a strong edge's (it publishes an object allocated since). Its old
    /// value needs no SATB barrier: a weak edge keeps nothing alive.
    #[inline]
    pub(crate) fn write_weak(&self, edge: &std::sync::atomic::AtomicU32, raw: u32) {
        if self.marking_active.get() {
            edge.store(raw, StdOrdering::Release);
        } else {
            edge.store(raw, StdOrdering::Relaxed);
        }
        self.weak_barrier(edge.as_ptr().cast(), raw);
    }

    /// Writes `raw` at `edge`, with the write barriers: while marking runs,
    /// the old value goes through SATB's (`satb`), and the write is a
    /// release, which publishes an object allocated since with its mark
    /// bit; then the generational one.
    #[inline]
    pub(crate) fn write_edge(&self, edge: &std::sync::atomic::AtomicU32, raw: u32) {
        if self.marking_active.get() {
            self.satb(edge.load(StdOrdering::Relaxed));
            edge.store(raw, StdOrdering::Release);
        } else {
            edge.store(raw, StdOrdering::Relaxed);
        }
        self.write_barrier(edge.as_ptr().cast(), raw);
    }

    /// `write_edge` for a runtime type's edge, which may hold an `I31`
    /// (`raw`): neither barrier takes one, as it is no object; an old
    /// value that is one isn't marked.
    #[inline]
    pub(crate) fn write_raw_edge(&self, edge: &std::sync::atomic::AtomicU32, raw: u32) {
        if self.marking_active.get() {
            let old = edge.load(StdOrdering::Relaxed);
            if !crate::raw::is_i31(old) {
                self.satb(old);
            }
            edge.store(raw, StdOrdering::Release);
        } else {
            edge.store(raw, StdOrdering::Relaxed);
        }
        if !crate::raw::is_i31(raw) {
            self.write_barrier(edge.as_ptr().cast(), raw);
        }
    }

    /// Records the old edge at `slot` in its page's (or large object's)
    /// slot set.
    pub(super) fn remember(&self, slot: usize) {
        self.remember_in(&self.remembered, slot);
    }

    /// Records the old edge at `slot` in `table`'s set for its page (or
    /// large object).
    fn remember_in(&self, table: &[SlotSet], slot: usize) {
        let space = self.space.borrow();
        let mut number = page::number_of(slot);
        // A large object's later pages: its slot set is its first's.
        while matches!(space.states[number], PageState::LargeTail) {
            number = number.saturating_sub(1);
        }
        let bytes = match &space.states[number] {
            PageState::Small(_) => PAGE_BYTES,
            PageState::Large(object) => object.bytes,
            _ => unreachable!("an old edge is on an old page"),
        };
        table[number].insert(slot.saturating_sub(page::start_of(number)), bytes);
    }

    /// The bytes of the object (or filler) at `data`, header included,
    /// rounded up to 8.
    pub(super) fn object_bytes(&self, data: usize) -> usize {
        // SAFETY: an object's (or filler's) header, in a committed page.
        let (index, flags) = unsafe { (self.header(data).read(), self.header(data).add(1).read()) };
        // SAFETY: the header just read, of an object or filler of the
        // nursery's.
        unsafe { object_bytes(self.cage.base(), data, index, flags) }
    }

    /// A scavenge, in the trace's `Gc.Scavenge`, on the platform's threads:
    /// every survivor promoted if `promote_all` (a full GC's), else those
    /// that survived one already.
    pub(super) fn scavenge(
        &mut self,
        platform: &dyn Platform,
        promote_all: bool,
    ) -> ScavengeRecord {
        let started = Instant::now();
        let phase_scavenge = self.tracer.phase("Gc.Scavenge");
        self.close_young_page();
        let seen = self.young_used.get();
        let young = self.young.get_mut();
        let from_pages = std::mem::take(&mut young.pages);
        let finalizable = std::mem::take(&mut young.finalizable);
        young.full = false;
        self.young_cursor.set(Cursor::default());
        let space = self.space.get_mut();
        let mut from = vec![false; space.states.len()];
        for &number in &from_pages {
            from[number] = true;
        }
        // The slot sets' pages, each a piece of work: (page, bytes).
        let slot_pages: Vec<(usize, usize)> = space
            .states
            .iter()
            .enumerate()
            .filter(|&(number, _)| self.remembered[number].is_set())
            .map(|(number, state)| match state {
                PageState::Small(_) => (number, PAGE_BYTES),
                PageState::Large(object) => (number, object.bytes),
                _ => unreachable!("a slot set is an old page's"),
            })
            .collect();
        // The root table's entries, then an engine's frames' (none and
        // `I31`s, which `evacuate` skips, among them).
        let mut roots = self.shared.roots.take_entries();
        let table_roots = roots.len();
        roots.extend_from_slice(&self.frame_roots);
        let sites = self.sites.get_mut().len();
        // The slot sets' pages for any thread to take, in whole segments;
        // the rest the joining thread's, with the roots: a worker isn't
        // woken for a little work, as for the copies.
        let mut seed = vec![ROOTS];
        let mut items: Vec<usize> = (0..slot_pages.len())
            .map(|at| at.saturating_mul(4) | SLOTS)
            .collect();
        seed.extend(items.drain(items.len().saturating_sub(items.len() % SEGMENT)..));
        let job = Scavenging {
            base: Base(self.cage.base()),
            tracer: self.tracer,
            black: self.marking_active.get(),
            from: &from,
            promote_all,
            young_pages: &self.young_pages[..],
            remembered: &self.remembered,
            slot_pages: &slot_pages,
            roots: Mutex::new(roots),
            source: Mutex::new(Source {
                cage: &self.cage,
                space,
                marks: &self.mark_table,
                young_pages: &self.young_pages[..],
                to_pages: Vec::new(),
                to: Cursor::default(),
            }),
            worklist: Worklist::with_published(seed, items),
            copiers: Mutex::new(Vec::new()),
            sites,
            scanned_by_workers: AtomicUsize::new(0),
            slot_pages_by_workers: AtomicUsize::new(0),
        };
        platform.run(&job);
        assert!(!job.worklist.poisoned(), "a thread that scavenged panicked");
        let scanned_by_workers = job.scanned_by_workers.load(Ordering::Relaxed);
        let slot_pages_by_workers = job.slot_pages_by_workers.load(Ordering::Relaxed);
        let mut roots = sync::into_inner(job.roots);
        let copiers = sync::into_inner(job.copiers);
        let Source { to_pages, to, .. } = sync::into_inner(job.source);
        self.frame_roots.copy_from_slice(&roots[table_roots..]);
        roots.truncate(table_roots);
        self.shared.roots.put_entries(roots);
        let mut record = ScavengeRecord {
            scanned_by_workers,
            slot_pages_by_workers,
            ..ScavengeRecord::default()
        };
        let mut promoted = Vec::new();
        let mut weak = Vec::new();
        let mut pairs = Vec::new();
        let mut old_bytes = 0usize;
        let mut site_counts = ScavengeCounts::new(sites);
        for copier in copiers {
            site::add_counts(&mut site_counts.found, &copier.sites.found);
            site::add_counts(&mut site_counts.promoted, &copier.sites.promoted);
            let counts = copier.record;
            record.survived_objects = record
                .survived_objects
                .saturating_add(counts.survived_objects);
            record.survived_bytes = record.survived_bytes.saturating_add(counts.survived_bytes);
            record.promoted_objects = record
                .promoted_objects
                .saturating_add(counts.promoted_objects);
            record.promoted_bytes = record.promoted_bytes.saturating_add(counts.promoted_bytes);
            record.copies_abandoned = record
                .copies_abandoned
                .saturating_add(counts.copies_abandoned);
            old_bytes = old_bytes.saturating_add(copier.old_bytes);
            promoted.extend(copier.promoted);
            weak.extend(copier.weak);
            pairs.extend(copier.pairs);
        }
        self.update_weak(&from, weak, pairs);
        self.allocated
            .set(self.allocated.get().saturating_add(old_bytes));
        self.used.set(self.used.get().saturating_add(old_bytes));
        // Promoted while a marking the engine drives runs: black.
        if self.counts_live.get() {
            self.black_bytes
                .set(self.black_bytes.get().saturating_add(old_bytes));
        }
        // The nursery is to-space now, bumped through from where the copies
        // end.
        if let Some(PageState::Young { top }) = self.space.get_mut().states.get_mut(to.page) {
            *top = to.next;
        }
        let young_bytes = to_pages
            .len()
            .saturating_sub(1)
            .saturating_mul(PAGE_BYTES)
            .saturating_add(
                to_pages
                    .last()
                    .map_or(0, |&last| to.next.saturating_sub(page::start_of(last))),
            );
        self.young_used.set(young_bytes);
        self.young_cursor.set(to);
        let young = self.young.get_mut();
        young.pages = to_pages;
        if !promote_all {
            young.scavenged(
                seen,
                record.survived_bytes.saturating_add(record.promoted_bytes),
            );
        }
        let large_nursery = young.capacity_pages
            >= young
                .max_pages
                .min(site::MIN_NURSERY_BYTES_TO_TENURE.div_ceil(PAGE_BYTES));
        self.sites.get_mut().scavenged(&site_counts, large_nursery);
        // The promoted, whole now: their pages' bitmaps (finalizable, and
        // marks in debug builds).
        for (data, index, site) in promoted {
            // SAFETY: a copy's type, which it had.
            let finalizer = unsafe { types::info(index) }.finalizer();
            self.publish(data, index, site, finalizer);
        }
        // The nursery's objects with drop glue: moved, promoted (the old
        // generation's bitmap has them now) or dead.
        let mut dead = Vec::new();
        for data in finalizable {
            match self.forwarded(data) {
                Some(to) if self.is_young(to) => self.young.get_mut().finalizable.push(to),
                Some(_) => {}
                None => dead.push(data),
            }
        }
        // From-space's pages go to the pool, kept committed: a later full
        // GC discards those no one took.
        let since = self.gcs.get().saturating_add(1);
        let space = self.space.get_mut();
        for &number in &from_pages {
            space.states[number] = PageState::Free { since };
            self.young_pages[number].store(false, StdOrdering::Relaxed);
            space.pool.push(number);
        }
        // The heap is whole again before any finalizer runs.
        record.finalized = dead.len();
        for &data in &dead {
            // SAFETY: a dead object with drop glue, whole until now (from-
            // space's pages still hold it), finalized once (it is no longer
            // listed).
            unsafe { self.finalize(data) };
        }
        if cfg!(debug_assertions) {
            for &number in &from_pages {
                // Its bytes past the last object are poisoned to ASan (under
                // fuzzing) from the pool: all of it is free now.
                let start = self.ptr(page::start_of(number));
                crate::asan::unpoison(start, PAGE_BYTES);
                // SAFETY: a free page, committed, that no live object is on.
                unsafe { std::ptr::write_bytes(start, POISON, PAGE_BYTES) };
            }
        }
        if cfg!(fuzzing) {
            self.poison_free();
        }
        phase_scavenge.end();
        record.ns = elapsed_ns(started);
        self.scavenges.set(self.scavenges.get().saturating_add(1));
        self.last_scavenge.set(record);
        record
    }

    /// The weak edges, once all a scavenge keeps is copied: the old
    /// generation's to young objects (the weak slot sets), the copies'
    /// (`copies`: each edge, and whether its copy is old) and the weak
    /// roots and the id table. Each to a from-space object now points to its
    /// copy, or is cleared if it has none: nothing strong kept it. The same
    /// for the
    /// ephemerons' keys (the ephemeron slot sets, and `pairs`, the copies'),
    /// a dead key's value cleared with it.
    fn update_weak(
        &mut self,
        from: &[bool],
        copies: Vec<(usize, bool)>,
        pairs: Vec<(usize, bool)>,
    ) {
        let after = |raw: u32| -> u32 {
            let data = raw as usize;
            if raw == 0 || !from.get(page::number_of(data)).copied().unwrap_or(false) {
                return raw;
            }
            #[expect(
                clippy::cast_possible_truncation,
                reason = "an offset in the cage, under 2^32"
            )]
            self.forwarded(data).map_or(0, |to| to as u32)
        };
        let update = |slot: usize| -> u32 {
            let edge = self.ptr(slot).cast::<u32>();
            // SAFETY: a weak edge of an old object (in its page's weak slot
            // set) or of a copy, 4-aligned, in a committed page; nothing
            // else runs.
            let value = unsafe { edge.read() };
            let moved = after(value);
            if moved != value {
                // SAFETY: as above.
                unsafe { edge.write(moved) };
            }
            moved
        };
        // The pages used so far: the slot sets of the rest are empty (a
        // walk of all 16384 entries of each table cost every scavenge some
        // 30 µs).
        let pages = self.space.borrow().states.len();
        // The slot sets first: a promoted copy's edges go into them below.
        let mut kept = Vec::new();
        for number in 0..pages {
            let Some(bits) = self.remembered_weak[number].take() else {
                continue;
            };
            let start = page::start_of(number);
            for at in bits.offsets() {
                let slot = start.saturating_add(at);
                let moved = update(slot);
                if moved != 0 && self.is_young(moved as usize) {
                    kept.push(slot);
                }
            }
        }
        for (slot, old) in copies {
            let moved = update(slot);
            if old && moved != 0 && self.is_young(moved as usize) {
                kept.push(slot);
            }
        }
        for slot in kept {
            self.remember_in(&self.remembered_weak, slot);
        }
        self.shared.weak_roots.update_entries(&after);
        // The id table's young entries, through its list of them.
        self.ids
            .borrow_mut()
            .update_young(after, |offset| self.is_young(offset as usize));
        // An ephemeron's key, at `pair`: moved, or cleared with its value.
        let update_pair = |pair: usize| -> u32 {
            let moved = update(pair);
            if moved == 0 {
                // SAFETY: the ephemeron's value, after its key; nothing
                // else runs.
                unsafe { self.ptr(pair.saturating_add(4)).cast::<u32>().write(0) };
            }
            moved
        };
        let mut kept = Vec::new();
        for number in 0..pages {
            let Some(bits) = self.remembered_ephemeron[number].take() else {
                continue;
            };
            let start = page::start_of(number);
            for at in bits.offsets() {
                let pair = start.saturating_add(at);
                let moved = update_pair(pair);
                if moved != 0 && self.is_young(moved as usize) {
                    kept.push(pair);
                }
            }
        }
        for (pair, old) in pairs {
            let moved = update_pair(pair);
            if old && moved != 0 && self.is_young(moved as usize) {
                kept.push(pair);
            }
        }
        for pair in kept {
            self.remember_in(&self.remembered_ephemeron, pair);
        }
    }

    /// Where the object at `data` was copied to, if it was.
    fn forwarded(&self, data: usize) -> Option<usize> {
        let header = self.header(data);
        // SAFETY: an object's header, in a committed page.
        let (first, flags) = unsafe { (header.read(), header.add(1).read()) };
        (flags & FORWARDED != 0).then_some(first as usize)
    }
}

/// A scavenge, shared by the threads that do it.
struct Scavenging<'a> {
    base: Base,
    /// The heap's trace: a run on a worker is a phase of it.
    tracer: Tracer,
    /// A concurrent marking runs: the old ranges its threads take are
    /// black, and their rest cleared when given back.
    black: bool,
    /// By page number, whether it is from-space.
    from: &'a [bool],
    promote_all: bool,
    young_pages: &'a [StdBool],
    remembered: &'a [SlotSet],
    slot_pages: &'a [(usize, usize)],
    /// The root table's entries, the joining thread's to update.
    roots: Mutex<Vec<u32>>,
    source: Mutex<Source<'a>>,
    worklist: Worklist,
    /// Each thread's, once it is done.
    copiers: Mutex<Vec<Copier>>,
    /// The heap's sites, by number: the length of a thread's counts.
    sites: usize,
    scanned_by_workers: AtomicUsize,
    slot_pages_by_workers: AtomicUsize,
}

/// What a scavenge's threads take room from, under its lock: the pages,
/// and the to-space being cut into LABs.
struct Source<'a> {
    cage: &'a Cage,
    space: &'a mut Space,
    marks: &'a MarkTable,
    young_pages: &'a [StdBool],
    /// To-space's pages, in the order they were taken.
    to_pages: Vec<usize>,
    /// What is left of the last to-space page.
    to: Cursor,
}

impl Source<'_> {
    /// A LAB of `bytes` (at least) of to-space: its start, and its end.
    fn cut(&mut self, base: *mut u8, bytes: usize) -> (usize, usize) {
        if self.to.limit.saturating_sub(self.to.next) < bytes {
            // The page's end, too short, is a filler; a new page.
            fill(
                base,
                self.to.next,
                self.to.limit.saturating_sub(self.to.next),
            );
            if let Some(PageState::Young { top }) = self.space.states.get_mut(self.to.page) {
                *top = self.to.limit;
            }
            let number = free_page(self.cage, self.space);
            let start = page::start_of(number);
            self.space.states[number] = PageState::Young { top: start };
            self.young_pages[number].store(true, StdOrdering::Relaxed);
            self.to_pages.push(number);
            self.to = Cursor {
                next: start,
                limit: start.saturating_add(PAGE_BYTES),
                page: number,
                ..Cursor::default()
            };
        }
        let start = self.to.next;
        let end = start
            .saturating_add(LAB_BYTES.max(bytes))
            .min(self.to.limit);
        self.to.next = end;
        crate::asan::unpoison(base.wrapping_add(start), end.saturating_sub(start));
        (start, end)
    }
}

/// A thread's own part of a scavenge: where it copies to, and what it
/// counts.
struct Copier {
    /// Its to-space LAB: the next header's offset, and its end.
    lab: (usize, usize),
    /// Its range of each size class, for promoting.
    cursors: Vec<Cursor>,
    record: ScavengeRecord,
    /// What it copied of each site.
    sites: ScavengeCounts,
    /// Bytes it took in the old generation.
    old_bytes: usize,
    /// Its promoted copies, their types and sites, for the heap's thread
    /// to publish in their pages' bitmaps.
    promoted: Vec<(usize, u32, u32)>,
    /// The weak edges of its copies, and whether each copy is old, for the
    /// heap's thread to update once all is copied.
    weak: Vec<(usize, bool)>,
    /// The same for its copies' ephemerons (each one's key).
    pairs: Vec<(usize, bool)>,
}

impl Copier {
    fn new(sites: usize) -> Self {
        Self {
            lab: (0, 0),
            cursors: vec![Cursor::default(); page::CLASSES.len()],
            record: ScavengeRecord::default(),
            sites: ScavengeCounts::new(sites),
            old_bytes: 0,
            promoted: Vec::new(),
            weak: Vec::new(),
            pairs: Vec::new(),
        }
    }
}

impl Job for Scavenging<'_> {
    fn run(&self, delegate: &dyn JobDelegate) {
        self.tracer
            .in_worker_span(delegate, "Gc.ParallelScavenge", || {
                let joining = delegate.is_joining_thread();
                let mut copier = Copier::new(self.sites);
                let mut scanned = 0usize;
                self.worklist.run(
                    joining,
                    &|| delegate.notify_concurrency_increase(),
                    |item, local, worklist| {
                        self.visit(item, &mut copier, local, worklist, joining);
                        scanned = scanned.saturating_add(1);
                    },
                );
                self.finish(&mut copier);
                #[expect(
                    clippy::disallowed_methods,
                    reason = "statistics: it would take 2^64 objects to wrap"
                )]
                if !joining {
                    self.scanned_by_workers
                        .fetch_add(scanned, Ordering::Relaxed);
                }
                sync::lock(&self.copiers).push(copier);
            })
    }

    fn max_concurrency(&self, active: usize) -> usize {
        // A worker for each segment to take, besides those working.
        active.saturating_add(self.worklist.available())
    }
}

impl Scavenging<'_> {
    fn ptr(&self, offset: usize) -> *mut u8 {
        self.base.0.wrapping_add(offset)
    }

    fn in_from(&self, offset: usize) -> bool {
        self.from
            .get(page::number_of(offset))
            .copied()
            .unwrap_or(false)
    }

    fn is_young(&self, offset: usize) -> bool {
        self.young_pages[page::number_of(offset)].load(StdOrdering::Relaxed)
    }

    fn visit(
        &self,
        item: usize,
        copier: &mut Copier,
        local: &mut Local,
        worklist: &Worklist,
        joining: bool,
    ) {
        if item == ROOTS {
            let mut roots = sync::lock(&self.roots);
            for entry in roots.iter_mut() {
                *entry = self.evacuate(copier, local, worklist, *entry).0;
            }
        } else if item & SLOTS != 0 {
            if !joining {
                #[expect(
                    clippy::disallowed_methods,
                    reason = "statistics: it would take 2^64 pages to wrap"
                )]
                self.slot_pages_by_workers.fetch_add(1, Ordering::Relaxed);
            }
            let (number, bytes) = self.slot_pages[item >> 2];
            self.update_slots(number, bytes, copier, local, worklist);
        } else {
            self.scan(item & !OLD, item & OLD != 0, copier, local, worklist);
        }
    }

    /// The object at `value` after this scavenge (copied, once, if it is in
    /// from-space), and whether it is young.
    fn evacuate(
        &self,
        copier: &mut Copier,
        local: &mut Local,
        worklist: &Worklist,
        value: u32,
    ) -> (u32, bool) {
        let data = value as usize;
        // None, or an `I31` (a runtime type's edge may hold one, a slot set
        // may name such an edge written since): no object.
        if value == 0 || crate::raw::is_i31(value) {
            return (value, false);
        }
        if !self.in_from(data) {
            // Old, or a copy in to-space already: a slot set's page may be
            // updated after a promoted copy put its edge to one there.
            return (value, self.is_young(data));
        }
        // SAFETY: a from-space object's header, 8-aligned, in a committed
        // page; the scavenge's threads only read it as this atomic, the
        // heap's thread only after they are done.
        let header =
            unsafe { StdU64::from_ptr(self.ptr(data.saturating_sub(HEADER_BYTES)).cast::<u64>()) };
        let (mut bytes, mut promote, mut old_flags) = (0, false, 0);
        let forwarded = forward(header, |index, flags| {
            // SAFETY: a from-space object's header.
            bytes = unsafe { object_bytes(self.base.0, data, index, flags) };
            promote = self.promote_all || flags & AGED != 0;
            old_flags = flags;
            let to = if promote {
                self.allocate_old(copier, bytes)
            } else {
                self.allocate_young(copier, bytes)
            };
            // SAFETY: the object's data, whole and unchanging while the
            // scavenge runs, and the room for it just taken (at least
            // `bytes`), apart.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.ptr(data),
                    self.ptr(to),
                    bytes.saturating_sub(HEADER_BYTES),
                );
                let copy = self.ptr(to.saturating_sub(HEADER_BYTES)).cast::<u32>();
                copy.write(index);
                copy.add(1)
                    .write(flags & SITE_BITS | if promote { 0 } else { AGED });
            }
            #[expect(
                clippy::cast_possible_truncation,
                reason = "an offset in the cage, under 2^32"
            )]
            let to = to as u32;
            to
        });
        match forwarded {
            Forward::Already(to) => (to, self.is_young(to as usize)),
            Forward::Won(to) => {
                let to = to as usize;
                local.push(worklist, if promote { to | OLD } else { to });
                let site = old_flags >> site::SITE_SHIFT;
                copier.sites.copied(site, old_flags & AGED == 0, promote);
                let record = &mut copier.record;
                if promote {
                    // SAFETY: the copy's header, just written.
                    let index = unsafe {
                        self.ptr(to.saturating_sub(HEADER_BYTES))
                            .cast::<u32>()
                            .read()
                    };
                    copier.promoted.push((to, index, site));
                    record.promoted_objects = record.promoted_objects.saturating_add(1);
                    record.promoted_bytes = record.promoted_bytes.saturating_add(bytes);
                } else {
                    record.survived_objects = record.survived_objects.saturating_add(1);
                    record.survived_bytes = record.survived_bytes.saturating_add(bytes);
                }
                (forwarded.to(), !promote)
            }
            Forward::Lost { winner, ours } => {
                Self::give_back(copier, promote, ours as usize, bytes);
                copier.record.copies_abandoned = copier.record.copies_abandoned.saturating_add(1);
                (winner, self.is_young(winner as usize))
            }
        }
    }

    /// Room in the thread's LAB for `bytes` (a multiple of 8): the data's
    /// offset.
    fn allocate_young(&self, copier: &mut Copier, bytes: usize) -> usize {
        let (next, limit) = copier.lab;
        if limit.saturating_sub(next) < bytes {
            fill(self.base.0, next, limit.saturating_sub(next));
            copier.lab = sync::lock(&self.source).cut(self.base.0, bytes);
        }
        let header = copier.lab.0;
        copier.lab.0 = header.saturating_add(bytes);
        header.saturating_add(HEADER_BYTES)
    }

    /// A slot of the thread's range of the class for `bytes`: the data's
    /// offset.
    fn allocate_old(&self, copier: &mut Copier, bytes: usize) -> usize {
        let class = page::class_for(bytes).expect("a young object fits a slot");
        let mut cursor = copier.cursors[class];
        if cursor.next >= cursor.limit {
            let mut source = sync::lock(&self.source);
            let Source {
                cage, space, marks, ..
            } = &mut *source;
            cursor = refill(cage, space, marks, self.black, class, cursor.page);
        }
        let data = cursor.next;
        let slot = page::CLASSES[class];
        cursor.next = data.saturating_add(slot);
        copier.cursors[class] = cursor;
        copier.old_bytes = copier.old_bytes.saturating_add(slot);
        crate::asan::unpoison(self.ptr(data.saturating_sub(HEADER_BYTES)), slot);
        data
    }

    /// A copy that lost the race for its object: back into the thread's
    /// buffer if it is the last there, else left, a whole object no edge
    /// points to (a free slot to the next sweep, in the old generation).
    fn give_back(copier: &mut Copier, promoted: bool, to: usize, bytes: usize) {
        let header = to.saturating_sub(HEADER_BYTES);
        if promoted {
            let class = page::class_for(bytes).expect("a young object fits a slot");
            let slot = page::CLASSES[class];
            let cursor = &mut copier.cursors[class];
            if cursor.next == to.saturating_add(slot) {
                cursor.next = to;
                copier.old_bytes = copier.old_bytes.saturating_sub(slot);
            }
        } else if copier.lab.0 == header.saturating_add(bytes) {
            copier.lab.0 = header;
        }
    }

    /// The thread is done: its LAB's end goes back to to-space if nothing
    /// was cut after it, else is a filler, and its classes' ranges go back
    /// to their pages.
    fn finish(&self, copier: &mut Copier) {
        let (next, limit) = copier.lab;
        let mut source = sync::lock(&self.source);
        if limit == source.to.next && limit > next {
            source.to.next = next;
        } else {
            fill(self.base.0, next, limit.saturating_sub(next));
        }
        for (class, cursor) in copier.cursors.iter().enumerate() {
            if cursor.next >= cursor.limit {
                continue;
            }
            if let Some(PageState::Small(page)) = source.space.states.get_mut(cursor.page) {
                if self.black {
                    page.marks.set_range(cursor.next, cursor.limit, false);
                }
                page.give_back(cursor.next, cursor.limit);
                if !source.space.available[class].contains(&cursor.page) {
                    source.space.available[class].push(cursor.page);
                }
            }
        }
    }

    /// Copies what the edge at `slot` points to if it is in from-space, and
    /// writes where it went; an old object's edge (`old`) that still points
    /// to a young object goes back into its page's slot set.
    fn update(
        &self,
        slot: usize,
        old: bool,
        copier: &mut Copier,
        local: &mut Local,
        worklist: &Worklist,
    ) {
        // SAFETY: an edge of a live object, which this thread alone updates
        // (each edge is an item's, scanned once).
        let value = unsafe { self.ptr(slot).cast::<u32>().read() };
        let (moved, young) = self.evacuate(copier, local, worklist, value);
        if moved != value {
            // SAFETY: as above.
            unsafe { self.ptr(slot).cast::<u32>().write(moved) };
        }
        if old && young {
            // A promoted copy is on a size-class page.
            let number = page::number_of(slot);
            self.remembered[number].insert(slot.saturating_sub(page::start_of(number)), PAGE_BYTES);
        }
    }

    /// Updates the edges of the copy at `data` (`old` if promoted).
    fn scan(
        &self,
        data: usize,
        old: bool,
        copier: &mut Copier,
        local: &mut Local,
        worklist: &Worklist,
    ) {
        // SAFETY: a copy's header, whole (its thread published it through
        // the worklist).
        let index = unsafe {
            self.ptr(data.saturating_sub(HEADER_BYTES))
                .cast::<u32>()
                .read()
        };
        // SAFETY: a copy's type, which it had.
        let ty = unsafe { types::entry(index) };
        match ty.info.kind() {
            Kind::Fixed => {
                for &at in &ty.edges {
                    self.update(
                        data.saturating_add(at as usize),
                        old,
                        copier,
                        local,
                        worklist,
                    );
                }
                copier.weak.extend(
                    ty.weak
                        .iter()
                        .map(|&at| (data.saturating_add(at as usize), old)),
                );
                // An ephemeron's value is kept as an edge's, whether its
                // key lives (V8's scavenger); its key is weak.
                for &at in &ty.ephemerons {
                    let pair = data.saturating_add(at as usize);
                    self.update(pair.saturating_add(4), old, copier, local, worklist);
                    copier.pairs.push((pair, old));
                }
            }
            Kind::MemberArray => {
                // SAFETY: an array's capacity, then its edges.
                let capacity = unsafe { self.ptr(data).cast::<u32>().read() } as usize;
                for i in 0..capacity {
                    let at = i.saturating_add(1).saturating_mul(4);
                    self.update(data.saturating_add(at), old, copier, local, worklist);
                }
            }
            Kind::Raw => {
                // SAFETY: a copy of an object of the type, whole.
                for at in unsafe { ty.raw_edges(self.ptr(data)) } {
                    self.update(data.saturating_add(at), old, copier, local, worklist);
                }
            }
        }
    }

    /// Updates the old edges page `number`'s slot set holds (taken out;
    /// those still young go back in).
    fn update_slots(
        &self,
        number: usize,
        bytes: usize,
        copier: &mut Copier,
        local: &mut Local,
        worklist: &Worklist,
    ) {
        let Some(bits) = self.remembered[number].take() else {
            return;
        };
        let start = page::start_of(number);
        for at in bits.offsets() {
            let slot = start.saturating_add(at);
            // SAFETY: an old object's edge (I10), which this thread alone
            // updates (the page is one item).
            let value = unsafe { self.ptr(slot).cast::<u32>().read() };
            let (moved, young) = self.evacuate(copier, local, worklist, value);
            if moved != value {
                // SAFETY: as above.
                unsafe { self.ptr(slot).cast::<u32>().write(moved) };
            }
            if young {
                self.remembered[number].insert(at, bytes);
            }
        }
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use loom::sync::Arc;
    use loom::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    impl HeaderWord for AtomicU64 {
        fn load(&self) -> u64 {
            AtomicU64::load(self, Ordering::Acquire)
        }

        fn swap_if(&self, seen: u64, new: u64) -> Result<(), u64> {
            self.compare_exchange(seen, new, Ordering::AcqRel, Ordering::Acquire)
                .map(|_| ())
        }
    }

    /// Three threads copying one object, in every interleaving: one copy
    /// wins, every thread ends with it, and a loser's own copy is told
    /// apart (to give back).
    #[test]
    fn one_copy_wins_and_every_thread_takes_it() {
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = Some(3);
        builder.check(|| {
            let header = Arc::new(AtomicU64::new(join(7, AGED)));
            let copier = |copy: u32| {
                move |index, flags| {
                    assert_eq!((index, flags), (7, AGED), "the header as it was");
                    copy
                }
            };
            let threads: Vec<_> = [101, 102]
                .into_iter()
                .map(|copy| {
                    let header = header.clone();
                    loom::thread::spawn(move || forward(&*header, copier(copy)))
                })
                .collect();
            let mut results = vec![forward(&*header, copier(100))];
            results.extend(threads.into_iter().map(|thread| thread.join().unwrap()));
            let won: Vec<u32> = results
                .iter()
                .filter_map(|result| match result {
                    Forward::Won(to) => Some(*to),
                    _ => None,
                })
                .collect();
            assert_eq!(won.len(), 1, "{results:?}");
            assert!(
                results.iter().all(|result| result.to() == won[0]),
                "{results:?}"
            );
            for result in &results {
                if let Forward::Lost { winner, ours } = result {
                    assert_ne!(winner, ours);
                }
            }
            assert_eq!(
                split(AtomicU64::load(&header, Ordering::Relaxed)),
                (won[0], FORWARDED)
            );
        });
    }
}
