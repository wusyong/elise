//! Pages and size classes. The cage is cut into 256 KiB pages (V8's; the
//! first is never used, so that offset 0 is null). A page holds slots of
//! one size class: every 16 bytes up to 128, then about 12.5% apart
//! (mimalloc's spacing), up to 32 KiB, where a page's tail wastes at most
//! 8.6% of it. A slot is an object's 8-byte header (its type's index and 4
//! bytes of flags) and its data. Larger objects take pages of their own
//! (`LargeObject`).
//!
//! A page keeps two bitmaps next to it, a bit per slot. `marks` is the
//! live objects: a GC clears it and marks them. Allocation doesn't use it
//! (it bumps through the ranges the sweep found), so a new object sets its
//! bit only in debug builds, for the next GC to poison it if it dies.
//! `finalizable` is the objects with drop glue, set when they are whole. A
//! sweep reads only these: the free slots are the runs of zeros in
//! `marks`, kept as ranges to bump through, and a dead object is read only
//! to run its finalizer. Both are atomic words, in `Marks`: the sweep's
//! worker reads and clears the bits of the pages handed to it.
//!
//! The threads that mark set `marks` bits with an atomic or (relaxed: the
//! heap doesn't change while they mark, and the platform's handoff orders
//! their start and end with the rest); the rest of the GC runs on the
//! heap's thread. They find a page's `marks` (`Marks`, in a box of the
//! page's that doesn't move with it) through a table of the heap's by page
//! number (`MarkTable`), not the pages' list, which the heap's thread
//! changes as it takes pages: a marker reads the table, a page in use
//! never leaves it, and a page leaves it only at a safepoint, when it is
//! freed.
//!
//! An old page (and a large object) may also have a slot set (`SlotSet`,
//! kept in a table of the heap's by page number, apart from the page so
//! that a scavenge's threads can record in it while others take pages): a
//! bit for each 4 bytes of it, set where one of its objects has an edge to
//! a young object (V8's `OLD_TO_NEW`), made the first time one is recorded.

use std::ops::Deref;
use std::ptr::{self, NonNull};
use std::sync::atomic::AtomicPtr;

use crate::bits::free_ranges;
use crate::cage::CAGE_BYTES;
use crate::sync::{AtomicU64, Ordering};
use crate::types::Finalizer;

pub(crate) const PAGE_BYTES: usize = 256 << 10;
pub(crate) const HEADER_BYTES: usize = 8;
/// Slots are 16-byte aligned, so data (after the header) is 8-aligned.
const SLOT_ALIGN: usize = 16;
/// The largest slot: larger objects are `LargeObject`s.
pub(crate) const MAX_SLOT_BYTES: usize = 32 << 10;
pub(crate) const PAGES: usize = CAGE_BYTES / PAGE_BYTES;

/// The size classes, smallest first.
pub(crate) const CLASSES: &[usize] = &classes();

/// For each class, `ceil(2^MAGIC_SHIFT / size)`: an offset in a page (under
/// 2^18) times it, shifted right by `MAGIC_SHIFT`, is the offset divided by
/// the size, exactly (the error, under offset / 2^MAGIC_SHIFT < 2^-22, is
/// less than 1 / size), without a division on marking's path.
const MAGIC: &[u64] = &magic();
const MAGIC_SHIFT: u32 = 40;

#[expect(
    clippy::arithmetic_side_effects,
    reason = "evaluated at compile time, where an overflow fails the build"
)]
const fn class_count() -> usize {
    let mut count = 0;
    let mut size = 0;
    while size < MAX_SLOT_BYTES {
        size = next_class(size);
        count += 1;
    }
    count
}

const CLASS_COUNT: usize = class_count();

#[expect(
    clippy::arithmetic_side_effects,
    reason = "evaluated at compile time, where an overflow fails the build"
)]
const fn next_class(size: usize) -> usize {
    let next = if size < 128 {
        size + SLOT_ALIGN
    } else {
        (size + size / 8).next_multiple_of(SLOT_ALIGN)
    };
    if next > MAX_SLOT_BYTES {
        MAX_SLOT_BYTES
    } else {
        next
    }
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "evaluated at compile time, where an overflow fails the build"
)]
const fn classes() -> [usize; CLASS_COUNT] {
    let mut classes = [0; CLASS_COUNT];
    let mut size = 0;
    let mut at = 0;
    while at < CLASS_COUNT {
        size = next_class(size);
        classes[at] = size;
        at += 1;
    }
    classes
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "evaluated at compile time, where an overflow fails the build"
)]
const fn magic() -> [u64; CLASS_COUNT] {
    let mut magic = [0; CLASS_COUNT];
    let mut at = 0;
    while at < CLASS_COUNT {
        magic[at] = (1u64 << MAGIC_SHIFT).div_ceil(CLASSES[at] as u64);
        at += 1;
    }
    magic
}

/// The class for a slot of at least `bytes` (header included), `None` for
/// a large object.
pub(crate) fn class_for(bytes: usize) -> Option<usize> {
    let class = CLASSES.partition_point(|&size| size < bytes);
    (class < CLASSES.len()).then_some(class)
}

/// `class_for`, at compile time: a type's (`TypeInfo::of`), so that its
/// allocation doesn't search the classes (cppgc knows an object's space
/// from its size, a constant in `MakeGarbageCollected`). `NO_CLASS` for a
/// large object.
#[expect(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    reason = "evaluated at compile time, where an overflow fails the build; CLASS_COUNT fits a u8"
)]
pub(crate) const fn const_class_for(bytes: usize) -> u8 {
    let mut class = 0;
    while class < CLASS_COUNT {
        if CLASSES[class] >= bytes {
            return class as u8;
        }
        class += 1;
    }
    NO_CLASS
}

/// `const_class_for`'s large object.
pub(crate) const NO_CLASS: u8 = u8::MAX;

/// What marking reads of a page in use: where its slots are, and their
/// mark bits. A large object is one slot.
pub(crate) struct Marks {
    start: usize,
    slot_bytes: usize,
    slots: usize,
    /// The slot of an offset from the start, by multiplication (0 for a
    /// large object: one slot).
    magic: u64,
    bits: Box<[AtomicU64]>,
    /// The objects with drop glue, a bit per slot.
    finalizable: Box<[AtomicU64]>,
}

impl Marks {
    fn new(start: usize, slot_bytes: usize, slots: usize, magic: u64) -> Box<Self> {
        Box::new(Self {
            start,
            slot_bytes,
            slots,
            magic,
            bits: (0..slots.div_ceil(64)).map(|_| AtomicU64::new(0)).collect(),
            finalizable: (0..slots.div_ceil(64)).map(|_| AtomicU64::new(0)).collect(),
        })
    }

    /// The slot of an object of the page, by its data's offset.
    fn slot_of(&self, data: usize) -> usize {
        let from = data
            .checked_sub(self.start)
            .and_then(|from| from.checked_sub(HEADER_BYTES))
            .filter(|&from| from < PAGE_BYTES)
            .expect("an object of the page");
        #[expect(
            clippy::arithmetic_side_effects,
            reason = "from < 2^18 and magic <= 2^40 / 16, so the product is under 2^54"
        )]
        let slot = ((from as u64 * self.magic) >> MAGIC_SHIFT) as usize;
        debug_assert_eq!(
            data_at(self.start, self.slot_bytes, slot),
            data,
            "an object's start"
        );
        slot
    }

    /// Marks the slot: whether it wasn't marked. A plain read first spares
    /// the atomic write for what is marked already (V8 reads before its
    /// compare-and-swap).
    fn mark(&self, slot: usize) -> bool {
        let (word, bit) = (&self.bits[slot / 64], 1u64 << (slot % 64));
        word.load(Ordering::Relaxed) & bit == 0 && word.fetch_or(bit, Ordering::Relaxed) & bit == 0
    }

    fn is_marked(&self, slot: usize) -> bool {
        self.bits[slot / 64].load(Ordering::Relaxed) & (1u64 << (slot % 64)) != 0
    }

    fn set(&self, slot: usize, marked: bool) {
        let bit = 1u64 << (slot % 64);
        if marked {
            self.bits[slot / 64].fetch_or(bit, Ordering::Relaxed);
        } else {
            self.bits[slot / 64].fetch_and(!bit, Ordering::Relaxed);
        }
    }

    fn clear(&self) {
        for word in &self.bits {
            word.store(0, Ordering::Relaxed);
        }
    }

    /// Sets (or clears) the bits of the slots from the one whose data is at
    /// `next` up to the one at `limit`: a range allocation bumps through,
    /// black while marking runs (V8's black area), cleared of what it left.
    /// A word at a time.
    pub(crate) fn set_range(&self, next: usize, limit: usize, marked: bool) {
        let slot = |data: usize| {
            data.saturating_sub(self.start)
                .saturating_sub(HEADER_BYTES)
                .checked_div(self.slot_bytes)
                .unwrap_or(0)
        };
        let (mut first, end) = (slot(next), slot(limit));
        while first < end {
            let word = first / 64;
            let to = end.min(word.saturating_add(1).saturating_mul(64));
            // The bits first..to of the word.
            let width = to.saturating_sub(first);
            let ones = if width >= 64 {
                u64::MAX
            } else {
                (1u64 << width).wrapping_sub(1)
            };
            let mask = ones << (first % 64);
            if marked {
                self.bits[word].fetch_or(mask, Ordering::Relaxed);
            } else {
                self.bits[word].fetch_and(!mask, Ordering::Relaxed);
            }
            first = to;
        }
    }

    fn words(&self) -> Vec<u64> {
        self.bits
            .iter()
            .map(|word| word.load(Ordering::Relaxed))
            .collect()
    }

    /// A sweep of the page by its marks alone: its free ranges, the lowest
    /// last (as `Page::ranges`), and its marked slots' count.
    fn sweep(&self) -> Swept {
        let words = self.words();
        let mut ranges = free_ranges(&words, self.slots);
        ranges.reverse();
        Swept {
            ranges,
            live: ones(&words).count(),
        }
    }

    /// Slot `slot`'s finalizable bit set by the page's only writer: a plain
    /// load and store, not an atomic or (`publish`'s, at every allocation
    /// with drop glue). The heap's thread writes a page's bits while the
    /// page is in use; a page handed to the sweep's worker is the worker's
    /// alone until its result is applied, and no slot of it is handed out
    /// before (soundness's I6), so no other thread writes the word between
    /// the load and the store.
    fn set_finalizable_alone(&self, slot: usize) {
        let word = &self.finalizable[slot / 64];
        word.store(
            word.load(Ordering::Relaxed) | 1u64 << (slot % 64),
            Ordering::Relaxed,
        );
    }

    fn set_finalizable(&self, slot: usize, finalizable: bool) {
        let bit = 1u64 << (slot % 64);
        if finalizable {
            self.finalizable[slot / 64].fetch_or(bit, Ordering::Relaxed);
        } else {
            self.finalizable[slot / 64].fetch_and(!bit, Ordering::Relaxed);
        }
    }

    fn finalizable_words(&self) -> Vec<u64> {
        self.finalizable
            .iter()
            .map(|word| word.load(Ordering::Relaxed))
            .collect()
    }

    /// The dead objects with drop glue (unmarked, finalizable), by slot:
    /// no longer finalizable.
    fn take_dead_finalizable(&self) -> Vec<usize> {
        let mut dead = Vec::new();
        for (word, (marks, finalizable)) in self.bits.iter().zip(&self.finalizable).enumerate() {
            let gone = finalizable.load(Ordering::Relaxed) & !marks.load(Ordering::Relaxed);
            if gone == 0 {
                continue;
            }
            finalizable.fetch_and(!gone, Ordering::Relaxed);
            #[expect(
                clippy::arithmetic_side_effects,
                reason = "word < slots / 64 + 1 and bit < 64: a slot of the page, at most 2^14"
            )]
            dead.extend(ones(&[gone]).map(|bit| word * 64 + bit));
        }
        dead
    }
}

/// What a sweep found of a page: its free ranges, the lowest last, and its
/// live objects' count.
pub(crate) struct Swept {
    pub(crate) ranges: Vec<(u32, u32)>,
    pub(crate) live: usize,
}

/// A page's `Marks`, owned without `Box`'s claim that no other pointer
/// reaches them: `MarkTable` holds one more, which every move of a `Box`
/// would invalidate (a unique retag, under Stacked Borrows), and a page
/// moves as its state changes. The `aliasable` crate's `AliasableBox`.
pub(crate) struct OwnedMarks(NonNull<Marks>);

impl OwnedMarks {
    fn new(marks: Box<Marks>) -> Self {
        Self(NonNull::new(Box::into_raw(marks)).expect("a box isn't null"))
    }
}

impl Deref for OwnedMarks {
    type Target = Marks;

    fn deref(&self) -> &Marks {
        // SAFETY: a `Box`'s allocation, freed only when this drops; only
        // shared references are made of it.
        unsafe { self.0.as_ref() }
    }
}

impl Drop for OwnedMarks {
    fn drop(&mut self) {
        // SAFETY: made by `Box::into_raw` in `new`, and freed once, here;
        // `MarkTable` lets go of it before (a page leaves the table before
        // it is freed).
        drop(unsafe { Box::from_raw(self.0.as_ptr()) });
    }
}

// SAFETY: as a `Box<Marks>`: `Marks` is `Send` and `Sync` (its bits are
// atomics, the rest is read only).
unsafe impl Send for OwnedMarks {}
// SAFETY: as above.
unsafe impl Sync for OwnedMarks {}

/// By page number, the `Marks` of each page in use (a size class's, or a
/// large object's first): what the threads that mark read to find a mark
/// bit, and the sweep's to sweep a page. The heap's thread puts a page's in
/// before it allocates on it, and takes it out only at a safepoint, when
/// it frees the page: a page handed to the sweep only once its result is
/// applied.
pub(crate) struct MarkTable(Box<[AtomicPtr<Marks>]>);

impl MarkTable {
    pub(crate) fn new() -> Self {
        Self(
            (0..PAGES)
                .map(|_| AtomicPtr::new(ptr::null_mut()))
                .collect(),
        )
    }

    /// Page `number` is in use, with `marks`, which its page owns.
    pub(crate) fn insert(&self, number: usize, marks: &Marks) {
        self.0[number].store(
            ptr::from_ref(marks).cast_mut(),
            std::sync::atomic::Ordering::Release,
        );
    }

    /// Page `number` is freed (at a safepoint).
    pub(crate) fn remove(&self, number: usize) {
        self.0[number].store(ptr::null_mut(), std::sync::atomic::Ordering::Release);
    }

    /// Page `number`'s `Marks`. A relaxed read: a marker in a pause is
    /// ordered after the page went in by the platform's handoff, and one
    /// alongside the mutator by the fence after the edges it read (written
    /// with release after the page went in); an acquire read here, for each
    /// edge, marked the DOM's nodes an eighth slower.
    fn get(&self, number: usize) -> Option<&Marks> {
        let marks = self
            .0
            .get(number)?
            .load(std::sync::atomic::Ordering::Relaxed);
        // SAFETY: a page's `Marks`, in an allocation of the page's that
        // doesn't move (`OwnedMarks`, not a `Box`, whose moves would claim
        // it alone): it stays in the table while the page is in use, and leaves
        // it before the page is freed, only at a safepoint, while no thread
        // reads that page's entry: no one marks then, and the sweep reads
        // only the pages handed to it, which are freed only once their
        // results are applied (or the sweep's job is cancelled).
        (!marks.is_null()).then(|| unsafe { &*marks })
    }

    /// Records the new object at `data`, whole now, in its page's bitmaps
    /// as `Page::publish` does, through the table (allocation's fast path
    /// doesn't borrow the space for its page): marked if `mark` (debug
    /// builds, no marking running), finalizable if it has drop glue. The
    /// page's counts are the caller's to add.
    ///
    /// # Panics
    ///
    /// If no page in use holds it.
    pub(crate) fn publish(&self, data: usize, finalizable: bool, mark: bool) {
        let marks = self
            .get(number_of(data))
            .expect("an object is allocated on a page in use");
        let slot = marks.slot_of(data);
        if mark {
            marks.set(slot, true);
        }
        if finalizable {
            marks.set_finalizable_alone(slot);
        }
    }

    /// Marks the object at `data`: whether it wasn't marked.
    ///
    /// # Panics
    ///
    /// If no page in use holds it.
    pub(crate) fn mark(&self, data: usize) -> bool {
        let marks = self
            .get(number_of(data))
            .expect("an edge points into a page in use");
        marks.mark(marks.slot_of(data))
    }

    /// Whether the object at `data` is marked (not, on no page in use).
    pub(crate) fn is_marked(&self, data: usize) -> bool {
        self.get(number_of(data))
            .is_some_and(|marks| marks.is_marked(marks.slot_of(data)))
    }

    /// The bytes the object at `data` takes: its slot's, or the large
    /// object's.
    ///
    /// # Panics
    ///
    /// If no page in use holds it.
    pub(crate) fn slot_bytes(&self, data: usize) -> usize {
        self.get(number_of(data))
            .expect("an object of a page in use")
            .slot_bytes
    }

    /// Sweeps size-class page `number` by its marks: on a sweeper's thread,
    /// while the heap's thread neither marks nor allocates on it.
    ///
    /// # Panics
    ///
    /// If the page isn't in use.
    pub(crate) fn sweep(&self, number: usize) -> Swept {
        self.get(number).expect("a page in use").sweep()
    }

    /// The dead objects with drop glue of page `number` (a size class's, or
    /// a large object's first), by data offset, no longer finalizable: for
    /// a sweeper's thread to finalize, as for `sweep`.
    ///
    /// # Panics
    ///
    /// If the page isn't in use.
    pub(crate) fn take_dead_finalizable(&self, number: usize) -> Vec<usize> {
        let marks = self.get(number).expect("a page in use");
        let dead = marks.take_dead_finalizable();
        if marks.magic == 0 {
            // A large object: one slot, its data past its page's header.
            return dead
                .iter()
                .map(|_| marks.start.saturating_add(HEADER_BYTES))
                .collect();
        }
        dead.into_iter()
            .map(|slot| data_at(marks.start, marks.slot_bytes, slot))
            .collect()
    }

    /// Whether the object at `data` has drop glue not run yet (not, on no
    /// page in use).
    pub(crate) fn is_finalizable(&self, data: usize) -> bool {
        self.get(number_of(data)).is_some_and(|marks| {
            let slot = if marks.magic == 0 {
                0
            } else {
                marks.slot_of(data)
            };
            marks.finalizable[slot / 64].load(Ordering::Relaxed) & (1u64 << (slot % 64)) != 0
        })
    }
}

/// A page of one size class.
pub(crate) struct Page {
    /// Its offset in the cage.
    pub(crate) start: usize,
    pub(crate) class: usize,
    pub(crate) slot_bytes: usize,
    pub(crate) slots: usize,
    magic: u64,
    pub(crate) marks: OwnedMarks,
    /// Its free slots not handed out yet, as runs `(first, end)`, the
    /// lowest last (taken with `pop`).
    pub(crate) ranges: Vec<(u32, u32)>,
    /// Its free slots, found by the last sweep (or all, when new).
    pub(crate) free: usize,
    /// Handed to the sweep after a GC's marking, its result not applied
    /// yet: no free range (`ranges` empty, `free` 0), its marks the GC's.
    pub(crate) unswept: bool,
    /// Its objects with drop glue (its bits in `marks`' `finalizable`), and
    /// of those, the ones to finalize on the heap's thread (`Main`).
    pub(crate) finalizable_objects: usize,
    pub(crate) main_objects: usize,
}

impl Page {
    /// A page with every slot free.
    pub(crate) fn new(number: usize, class: usize) -> Self {
        let slot_bytes = CLASSES[class];
        #[expect(
            clippy::arithmetic_side_effects,
            reason = "a class is at least 16 bytes"
        )]
        let slots = PAGE_BYTES / slot_bytes;
        let start = start_of(number);
        Self {
            start,
            class,
            slot_bytes,
            slots,
            magic: MAGIC[class],
            marks: OwnedMarks::new(Marks::new(start, slot_bytes, slots, MAGIC[class])),
            ranges: vec![(0, u32::try_from(slots).expect("at most 2^14 slots"))],
            free: slots,
            unswept: false,
            finalizable_objects: 0,
            main_objects: 0,
        }
    }

    /// The offset of slot `slot`'s data.
    pub(crate) fn data_of(&self, slot: usize) -> usize {
        debug_assert!(slot < self.slots);
        data_at(self.start, self.slot_bytes, slot)
    }

    /// The slot of an object of the page, by its data's offset.
    pub(crate) fn slot_of(&self, data: usize) -> usize {
        let from = data
            .checked_sub(self.start)
            .and_then(|from| from.checked_sub(HEADER_BYTES))
            .filter(|&from| from < PAGE_BYTES)
            .expect("an object of the page");
        #[expect(
            clippy::arithmetic_side_effects,
            reason = "from < 2^18 and magic <= 2^40 / 16, so the product is under 2^54"
        )]
        let slot = ((from as u64 * self.magic) >> MAGIC_SHIFT) as usize;
        debug_assert_eq!(self.data_of(slot), data, "an object's start");
        slot
    }

    /// Marks the slot: whether it wasn't marked.
    #[cfg(test)]
    pub(crate) fn mark(&self, slot: usize) -> bool {
        self.marks.mark(slot)
    }

    /// Its free ranges' slots, as bytes from their headers: what ASan is
    /// told not to touch under fuzzing.
    pub(crate) fn free_spans(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.ranges.iter().map(|&(first, end)| {
            let start =
                data_at(self.start, self.slot_bytes, first as usize).saturating_sub(HEADER_BYTES);
            let bytes = (end.saturating_sub(first) as usize).saturating_mul(self.slot_bytes);
            (start, bytes)
        })
    }

    /// Gives back the rest of a range taken (`next` up to `limit`, data
    /// offsets as `take_range` gave): a scavenge's thread's, unused.
    pub(crate) fn give_back(&mut self, next: usize, limit: usize) {
        let slot = |data: usize| {
            data.saturating_sub(self.start)
                .saturating_sub(HEADER_BYTES)
                .checked_div(self.slot_bytes)
                .and_then(|slot| u32::try_from(slot).ok())
                .expect("a slot of the page")
        };
        if next < limit {
            self.ranges.push((slot(next), slot(limit)));
        }
    }

    /// Takes its next free range: the data offsets of its first slot and
    /// of the slot past its end.
    pub(crate) fn take_range(&mut self) -> Option<(usize, usize)> {
        let (first, end) = self.ranges.pop()?;
        Some((
            data_at(self.start, self.slot_bytes, first as usize),
            data_at(self.start, self.slot_bytes, end as usize),
        ))
    }

    /// Clears a slot's mark: a heap verifier's test breaks a heap so.
    #[cfg(test)]
    pub(crate) fn unmark(&self, slot: usize) {
        self.marks.set(slot, false);
    }

    pub(crate) fn is_marked(&self, slot: usize) -> bool {
        self.marks.is_marked(slot)
    }

    /// Records a new object in the slot, whole now: finalizable if it has
    /// drop glue, and in `marks` if `mark` (debug builds).
    pub(crate) fn publish(&mut self, slot: usize, finalizer: Finalizer, mark: bool) {
        if mark {
            self.marks.set(slot, true);
        }
        if finalizer != Finalizer::None {
            self.marks.set_finalizable_alone(slot);
            self.finalizable_objects = self.finalizable_objects.saturating_add(1);
        }
        if finalizer == Finalizer::Main {
            self.main_objects = self.main_objects.saturating_add(1);
        }
    }

    pub(crate) fn clear_marks(&mut self) {
        self.marks.clear();
    }

    fn mark_words(&self) -> Vec<u64> {
        self.marks.words()
    }

    /// The marked slots: the live objects of the last GC (and, in debug
    /// builds, the ones allocated since).
    pub(crate) fn objects(&self) -> Vec<usize> {
        ones(&self.mark_words()).collect()
    }

    /// The objects with drop glue, by slot.
    pub(crate) fn finalizable(&self) -> Vec<usize> {
        ones(&self.marks.finalizable_words()).collect()
    }

    /// After marking: pushes the dead objects with drop glue onto `dead`
    /// (by data offset; they are no longer finalizable), finds the free
    /// ranges, and returns the live objects' count.
    pub(crate) fn sweep(&mut self, dead: &mut Vec<usize>) -> usize {
        let gone = self.marks.take_dead_finalizable();
        self.finalizable_objects = self.finalizable_objects.saturating_sub(gone.len());
        dead.extend(gone.into_iter().map(|slot| self.data_of(slot)));
        let swept = self.marks.sweep();
        let live = swept.live;
        self.apply(swept);
        live
    }

    /// Hands the page to the sweep: no range until its result is applied.
    pub(crate) fn hand(&mut self) {
        self.ranges.clear();
        self.free = 0;
        self.unswept = true;
    }

    /// Applies a sweep's result.
    pub(crate) fn apply(&mut self, swept: Swept) {
        // The sweep cleared its dead objects' finalizable bits (all of
        // them, though a finalizer panicked and left some unrun).
        if self.finalizable_objects > 0 {
            self.finalizable_objects = ones(&self.marks.finalizable_words()).count();
        }
        self.free = self.slots.saturating_sub(swept.live);
        self.ranges = swept.ranges;
        self.unswept = false;
    }
}

impl Page {
    /// The heap verifier's check of the page, right after a sweep: its free
    /// ranges are exactly its unmarked slots, lowest last and apart; its
    /// count of free slots agrees; every finalizable slot is marked; its
    /// count of finalizable objects agrees. A page handed to the sweep has
    /// no free range (its dead objects with drop glue wait). The live
    /// objects' count, or what is wrong.
    pub(crate) fn check(&self) -> Result<usize, String> {
        let marks = self.mark_words();
        let live = ones(&marks).filter(|&slot| slot < self.slots).count();
        if ones(&marks).any(|slot| slot >= self.slots) {
            return Err("a mark past the last slot".into());
        }
        if self.main_objects > self.finalizable_objects {
            return Err(format!(
                "{} objects to finalize on the heap's thread, of {} finalizable",
                self.main_objects, self.finalizable_objects
            ));
        }
        if self.unswept {
            // Its dead objects with drop glue wait for the heap's thread.
            if !self.ranges.is_empty() || self.free != 0 {
                return Err("not swept yet, but it has free ranges".into());
            }
            return Ok(live);
        }
        // Not while it waits for the sweep, whose worker clears its dead
        // objects' bits as it finalizes them.
        let finalizable = self.marks.finalizable_words();
        if ones(&finalizable).count() != self.finalizable_objects {
            return Err(format!(
                "{} finalizable objects counted, {} set",
                self.finalizable_objects,
                ones(&finalizable).count()
            ));
        }
        if self.free != self.slots.saturating_sub(live) {
            return Err(format!(
                "{} free slots counted, {} unmarked",
                self.free,
                self.slots.saturating_sub(live)
            ));
        }
        let mut in_ranges = 0usize;
        let mut below = u32::MAX;
        for &(first, end) in &self.ranges {
            if first >= end || end as usize > self.slots || end > below {
                return Err(format!(
                    "free range {first}..{end} is empty, past the end, or not below the last"
                ));
            }
            below = first;
            for slot in first as usize..end as usize {
                if marks[slot / 64] & (1 << (slot % 64)) != 0 {
                    return Err(format!(
                        "free range {first}..{end} holds the marked slot {slot}"
                    ));
                }
            }
            in_ranges = in_ranges.saturating_add(end.saturating_sub(first) as usize);
        }
        if in_ranges != self.free {
            return Err(format!(
                "the free ranges hold {in_ranges} slots, not the {} free",
                self.free
            ));
        }
        if let Some(slot) =
            ones(&finalizable).find(|&slot| marks[slot / 64] & (1 << (slot % 64)) == 0)
        {
            return Err(format!("slot {slot} is finalizable but unmarked"));
        }
        Ok(live)
    }
}

/// The data's offset of slot `slot` of a page at `start`.
fn data_at(start: usize, slot_bytes: usize, slot: usize) -> usize {
    #[expect(
        clippy::arithmetic_side_effects,
        reason = "slot <= the page's slots, so start + slot * slot_bytes + 8 is at most the next page's start + 8, inside the cage"
    )]
    let data = start + slot * slot_bytes + HEADER_BYTES;
    data
}

/// The set bits of a bitmap, lowest first.
fn ones(bits: &[u64]) -> impl Iterator<Item = usize> + '_ {
    bits.iter().enumerate().flat_map(|(word, &bits)| {
        std::iter::successors(Some(bits), |&rest| Some(rest & rest.wrapping_sub(1)))
            .take_while(|&rest| rest != 0)
            .map(move |rest| {
                #[expect(
                    clippy::arithmetic_side_effects,
                    reason = "a bitmap covers a page's slots or an object's 4-byte slots, an object at most a quarter of the cage: under 2^30 bits"
                )]
                let slot = word * 64 + rest.trailing_zeros() as usize;
                slot
            })
    })
}

/// An object larger than a slot, on pages of its own: the first holds its
/// header, at the page's start.
pub(crate) struct LargeObject {
    pub(crate) pages: usize,
    /// Its header and data.
    pub(crate) bytes: usize,
    /// Its mark bit and whether it has drop glue not run yet, as a page's
    /// of one slot.
    pub(crate) marks: OwnedMarks,
    /// Its finalizer runs on the heap's thread (`Finalizer::Main`).
    pub(crate) main: bool,
}

impl LargeObject {
    /// A large object from page `first` on, `bytes` long, unmarked.
    pub(crate) fn new(first: usize, pages: usize, bytes: usize) -> Self {
        Self {
            pages,
            bytes,
            marks: OwnedMarks::new(Marks::new(start_of(first), bytes, 1, 0)),
            main: false,
        }
    }

    pub(crate) fn is_marked(&self) -> bool {
        self.marks.is_marked(0)
    }

    pub(crate) fn set_marked(&self, marked: bool) {
        self.marks.set(0, marked);
    }

    pub(crate) fn is_finalizable(&self) -> bool {
        self.marks.finalizable[0].load(Ordering::Relaxed) & 1 != 0
    }

    pub(crate) fn set_finalizable(&self, finalizable: bool) {
        self.marks.set_finalizable(0, finalizable);
    }
}

/// The edges of an old page's (or large object's) objects to young
/// objects: a bit for each 4 bytes of it, from its start, made the first
/// time one is recorded. Bits are set with an atomic or, and the bitmap put
/// in with a compare-and-swap, so that a scavenge's threads can record in
/// it together (V8's `SlotSet`, atomic buckets); std's atomics, as the
/// pages' bitmaps aren't the protocol loom and shuttle check.
#[derive(Default)]
pub(crate) struct SlotSet {
    bits: std::sync::atomic::AtomicPtr<Bits>,
}

/// A slot set's bitmap.
pub(crate) struct Bits(Box<[std::sync::atomic::AtomicU64]>);

impl Bits {
    /// The recorded edges, as bytes from the start, lowest first.
    pub(crate) fn offsets(&self) -> Vec<usize> {
        let words: Vec<u64> = self
            .0
            .iter()
            .map(|word| word.load(std::sync::atomic::Ordering::Relaxed))
            .collect();
        ones(&words).map(|slot| slot.saturating_mul(4)).collect()
    }
}

impl SlotSet {
    /// Records the edge `at` bytes from the start of `bytes` (the page's or
    /// the large object's).
    pub(crate) fn insert(&self, at: usize, bytes: usize) {
        use std::sync::atomic::Ordering;
        let mut bits = self.bits.load(Ordering::Acquire);
        if bits.is_null() {
            let words = bytes.div_ceil(4).div_ceil(64);
            let new = Box::into_raw(Box::new(Bits(
                (0..words)
                    .map(|_| std::sync::atomic::AtomicU64::new(0))
                    .collect(),
            )));
            bits = match self.bits.compare_exchange(
                ptr::null_mut(),
                new,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => new,
                Err(winner) => {
                    // SAFETY: made just above, never shared.
                    drop(unsafe { Box::from_raw(new) });
                    winner
                }
            };
        }
        let slot = at / 4;
        // SAFETY: a bitmap put in by `insert`, freed only by `take` or
        // `clear` (with no `insert` alongside: at a safepoint).
        let bits = unsafe { &*bits };
        bits.0[slot / 64].fetch_or(1 << (slot % 64), Ordering::Relaxed);
    }

    pub(crate) fn contains(&self, at: usize) -> bool {
        use std::sync::atomic::Ordering;
        let bits = self.bits.load(Ordering::Acquire);
        let slot = at / 4;
        // SAFETY: as in `insert`.
        !bits.is_null()
            && unsafe { &*bits }.0[slot / 64].load(Ordering::Relaxed) & (1 << (slot % 64)) != 0
    }

    /// The recorded edges, as bytes from the start (the heap verifier's).
    pub(crate) fn offsets(&self) -> Vec<usize> {
        let bits = self.bits.load(std::sync::atomic::Ordering::Acquire);
        if bits.is_null() {
            return Vec::new();
        }
        // SAFETY: as in `insert`.
        unsafe { &*bits }.offsets()
    }

    /// Takes the bitmap out: the set is empty after.
    pub(crate) fn take(&self) -> Option<Box<Bits>> {
        let bits = self
            .bits
            .swap(ptr::null_mut(), std::sync::atomic::Ordering::AcqRel);
        // SAFETY: a bitmap put in by `insert`, taken out once (the swap).
        (!bits.is_null()).then(|| unsafe { Box::from_raw(bits) })
    }

    pub(crate) fn clear(&self) {
        drop(self.take());
    }

    /// Whether it has a bitmap (perhaps with no bit set).
    pub(crate) fn is_set(&self) -> bool {
        !self
            .bits
            .load(std::sync::atomic::Ordering::Acquire)
            .is_null()
    }
}

impl Drop for SlotSet {
    fn drop(&mut self) {
        self.clear();
    }
}

/// The offset of page `number`.
pub(crate) fn start_of(number: usize) -> usize {
    debug_assert!(number < PAGES);
    #[expect(
        clippy::arithmetic_side_effects,
        reason = "number < PAGES = CAGE_BYTES / PAGE_BYTES, so the product is under CAGE_BYTES"
    )]
    let start = number * PAGE_BYTES;
    start
}

/// The page an offset is in.
pub(crate) fn number_of(offset: usize) -> usize {
    offset / PAGE_BYTES
}

#[cfg(all(test, not(any(loom, shuttle))))]
mod tests {
    use super::*;

    #[test]
    fn classes_are_sixteen_apart_then_about_an_eighth() {
        assert_eq!(&CLASSES[..8], [16, 32, 48, 64, 80, 96, 112, 128]);
        assert_eq!(&CLASSES[8..12], [144, 176, 208, 240]);
        assert_eq!(*CLASSES.last().unwrap(), MAX_SLOT_BYTES);
        assert_eq!(CLASSES.len(), 52);
        for pair in CLASSES.windows(2) {
            assert!(pair[1] > pair[0] && pair[1] % SLOT_ALIGN == 0);
            assert!(pair[1] - pair[0] <= (pair[0] / 8).max(SLOT_ALIGN) + SLOT_ALIGN);
        }
        // A page's tail wastes at most 8.6% of it.
        for &size in CLASSES {
            assert!((PAGE_BYTES % size) * 1000 <= PAGE_BYTES * 86, "{size}");
        }
    }

    #[test]
    fn a_size_gets_the_smallest_class_it_fits() {
        assert_eq!(class_for(1), Some(0));
        assert_eq!(class_for(16), Some(0));
        assert_eq!(class_for(17), Some(1));
        assert_eq!(CLASSES[class_for(337).unwrap()], 368);
        assert_eq!(class_for(MAX_SLOT_BYTES), Some(CLASSES.len() - 1));
        assert_eq!(class_for(MAX_SLOT_BYTES + 1), None);
    }

    /// A type's class, worked out at compile time, is the one allocation
    /// would search for (every size under Miri's every 61st).
    #[test]
    fn the_compile_time_class_is_the_searched_one() {
        let step = if cfg!(miri) { 61 } else { 1 };
        for bytes in (1..=MAX_SLOT_BYTES + 64).step_by(step) {
            let class = const_class_for(bytes);
            assert_eq!(
                (class != NO_CLASS).then_some(class as usize),
                class_for(bytes),
                "{bytes} bytes"
            );
        }
    }

    /// The multiplication is the division for every offset in a page (every
    /// slot's start under Miri, which is too slow for all).
    #[test]
    fn the_magic_divides_exactly() {
        for (class, &size) in CLASSES.iter().enumerate() {
            let step = if cfg!(miri) { size } else { 1 };
            for from in (0..PAGE_BYTES).step_by(step) {
                let quotient = ((from as u64 * MAGIC[class]) >> MAGIC_SHIFT) as usize;
                assert_eq!(quotient, from / size, "{from} / {size}");
            }
        }
    }

    #[test]
    fn a_slot_is_found_from_its_data() {
        for class in [0, 7, 20, CLASSES.len() - 1] {
            let page = Page::new(3, class);
            for slot in [0, 1, page.slots / 2, page.slots - 1] {
                assert_eq!(page.slot_of(page.data_of(slot)), slot);
            }
        }
    }

    fn bitmap(set: &[usize], slots: usize) -> Vec<u64> {
        let mut bits = vec![0; slots.div_ceil(64)];
        for &slot in set {
            bits[slot / 64] |= 1 << (slot % 64);
        }
        bits
    }

    #[test]
    fn free_ranges_are_the_runs_of_zeros() {
        assert_eq!(free_ranges(&bitmap(&[], 100), 100), [(0, 100)]);
        assert_eq!(free_ranges(&bitmap(&[0, 1, 99], 100), 100), [(2, 99)]);
        // Across words, and the last word's bits past `slots` aren't slots.
        assert_eq!(
            free_ranges(&bitmap(&[5, 63, 64, 70, 127], 130), 130),
            [(0, 5), (6, 63), (65, 70), (71, 127), (128, 130)]
        );
        let full: Vec<usize> = (0..70).collect();
        assert_eq!(free_ranges(&bitmap(&full, 70), 70), []);
        let alternate: Vec<usize> = (0..10).step_by(2).collect();
        assert_eq!(
            free_ranges(&bitmap(&alternate, 10), 10),
            [(1, 2), (3, 4), (5, 6), (7, 8), (9, 10)]
        );
    }

    #[test]
    fn a_sweep_finds_the_dead_finalizable_and_the_free() {
        let mut page = Page::new(2, 3);
        for slot in [0, 1, 2, 5] {
            let finalizer = if slot == 1 {
                Finalizer::None
            } else {
                Finalizer::Sweeper
            };
            page.publish(slot, finalizer, true);
        }
        page.clear_marks();
        page.mark(1);
        page.mark(2);
        let mut dead = Vec::new();
        assert_eq!(page.sweep(&mut dead), 2);
        assert_eq!(dead, [page.data_of(0), page.data_of(5)]);
        assert_eq!(page.finalizable(), [2]);
        assert_eq!(page.objects(), [1, 2]);
        assert_eq!(page.free, page.slots - 2);
        assert_eq!(page.ranges.last(), Some(&(0, 1)));
        assert_eq!(page.ranges.first(), Some(&(3, page.slots as u32)));
    }

    /// A range's bits set and cleared whole, across words, and only its own.
    #[test]
    fn a_range_is_set_and_cleared_whole() {
        let page = Page::new(1, 0);
        let data = |slot: usize| page.data_of(slot);
        // Slots 60 up to 130: the end of a word, a whole one, a start.
        page.marks.set_range(data(60), data(130), true);
        let marked: Vec<usize> = page.objects();
        assert_eq!(marked, (60..130).collect::<Vec<_>>());
        // Cleared from 64 up to 129: 60 to 63 and 129 stay.
        page.marks.set_range(data(64), data(129), false);
        assert_eq!(page.objects(), [60, 61, 62, 63, 129]);
        // An empty range does nothing.
        page.marks.set_range(data(5), data(5), true);
        assert_eq!(page.objects().len(), 5);
        // Up to the page's end.
        let end = data_at(page.start, page.slot_bytes, page.slots);
        page.marks.set_range(data(page.slots - 3), end, true);
        assert_eq!(page.objects().len(), 8);
    }
}
