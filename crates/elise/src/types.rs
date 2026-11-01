//! What the GC knows of each type, and the process's table of them: an
//! object's header holds its type's index there (cppgc's `GCInfoTable`;
//! an index rather than a pointer, so that the heap holds no native
//! pointer of its own). A type's index is assigned the first time it is
//! allocated and kept in a static next to its layout (`TypeSlot`), as
//! cppgc's `GCInfoTrait<T>::Index()`: a type's layout is code, the same in
//! every renderer.
//!
//! Above the static types' indices, the table holds the types an embedder
//! registers as it runs (`raw`: a wasm engine's), at indices it chooses in
//! a range it reserved. Such a type is retired once nothing allocates it
//! and no live object has it; its entry is freed, and the index may take
//! another type, only after every heap then alive finished a full GC that
//! started after: until then a GC may still read a dead object's header
//! (a marking's snapshot, a scavenge through a dead old object's
//! remembered edge), and that GC's sweep leaves none.

use std::mem::{align_of, needs_drop, size_of};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// The largest object, header included: a quarter of the cage (one larger
/// than a slot takes pages of its own).
pub(crate) const MAX_OBJECT_BYTES: usize = crate::cage::CAGE_BYTES / 4;

/// A type's layout: its size, where its `Member`s are (the GC walks those
/// without calling the type's code) and how to finalize it.
pub struct TypeInfo {
    name: &'static str,
    /// The object's bytes after its header; an array's are in the array.
    size: u32,
    fields: &'static [FieldInfo],
    kind: Kind,
    /// `drop_in_place`, if the type has drop glue (an `External`'s).
    drop: Option<unsafe fn(*mut u8)>,
    finalizer: Finalizer,
}

/// Where an object's finalizer runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Finalizer {
    /// It has no drop glue.
    None,
    /// On the sweep's worker: its drop glue is `External`s', of `Send`
    /// values (or the heap's thread, if its page waits for it).
    Sweeper,
    /// On the heap's thread: it has a `LocalExternal`'s.
    Main,
}

/// A field of an object: its offset, its kind and whether its drop glue
/// must run on the heap's thread.
#[doc(hidden)]
pub struct FieldInfo {
    pub offset: usize,
    pub kind: FieldKind,
    pub main_thread_drop: bool,
}

/// What a field is to the GC.
#[doc(hidden)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FieldKind {
    /// Plain data or an `External`: never read.
    Data,
    /// An edge: marked through.
    Member,
    /// That many edges in a row (`[Member<T>; N]`).
    Members(u32),
    /// A weak edge (`WeakMember`): never marked through, cleared once its
    /// object dies.
    Weak,
    /// That many weak edges in a row (`[WeakMember<T>; N]`).
    Weaks(u32),
    /// An ephemeron (`EphemeronPair`): a weak edge, its key, then an edge,
    /// its value, alive while the key is.
    Ephemeron,
    /// That many ephemerons in a row (`[EphemeronPair<K, V>; N]`).
    Ephemerons(u32),
    /// A reference to a JS engine's object (`JsEdge`): the engine's own,
    /// whose address is handed to it when the object is scanned.
    JsEdge,
}

impl FieldKind {
    /// The edges the field holds, in a row from its offset.
    pub(crate) fn edges(self) -> usize {
        match self {
            FieldKind::Member => 1,
            FieldKind::Members(count) => count as usize,
            _ => 0,
        }
    }

    /// The weak edges the field holds, in a row from its offset.
    pub(crate) fn weak_edges(self) -> usize {
        match self {
            FieldKind::Weak => 1,
            FieldKind::Weaks(count) => count as usize,
            _ => 0,
        }
    }

    /// The ephemerons the field holds, in a row from its offset, 8 bytes
    /// each.
    pub(crate) fn ephemerons(self) -> usize {
        match self {
            FieldKind::Ephemeron => 1,
            FieldKind::Ephemerons(count) => count as usize,
            _ => 0,
        }
    }

    /// The JS edges the field holds.
    pub(crate) fn js_edges(self) -> usize {
        usize::from(self == FieldKind::JsEdge)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Fixed,
    /// `vector::MemberArray`: a capacity, then that many edges.
    MemberArray,
    /// A type registered as the process runs (`raw`): its edges are its
    /// entry's, and it may be an array (`Entry::array`).
    Raw,
}

impl TypeInfo {
    #[doc(hidden)]
    pub const fn of<T>(name: &'static str, fields: &'static [FieldInfo]) -> Self {
        assert!(
            align_of::<T>() <= 8,
            "a GcObject is aligned to 8 bytes at most"
        );
        assert!(
            size_of::<T>() <= MAX_OBJECT_BYTES,
            "a GcObject fits a quarter of the cage"
        );
        #[expect(
            clippy::cast_possible_truncation,
            reason = "asserted above: at most MAX_OBJECT_BYTES, a quarter of 2^32"
        )]
        let size = size_of::<T>() as u32;
        let mut main = false;
        let mut at = 0;
        while at < fields.len() {
            main = main || fields[at].main_thread_drop;
            at = at.saturating_add(1);
        }
        Self {
            name,
            size,
            fields,
            kind: Kind::Fixed,
            drop: if needs_drop::<T>() {
                Some(drop_object::<T>)
            } else {
                None
            },
            finalizer: if !needs_drop::<T>() {
                Finalizer::None
            } else if main {
                Finalizer::Main
            } else {
                Finalizer::Sweeper
            },
        }
    }

    /// A runtime type's: `size` bytes (an array's fixed part), no drop
    /// glue.
    const fn raw(size: u32) -> Self {
        Self {
            name: "raw",
            size,
            fields: &[],
            kind: Kind::Raw,
            drop: None,
            finalizer: Finalizer::None,
        }
    }

    pub(crate) const fn member_array(name: &'static str) -> Self {
        Self {
            name,
            size: 0,
            fields: &[],
            kind: Kind::MemberArray,
            drop: None,
            finalizer: Finalizer::None,
        }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub(crate) fn size(&self) -> u32 {
        self.size
    }

    pub(crate) fn fields(&self) -> &'static [FieldInfo] {
        self.fields
    }

    pub(crate) fn kind(&self) -> Kind {
        self.kind
    }

    pub(crate) fn drop_fn(&self) -> Option<unsafe fn(*mut u8)> {
        self.drop
    }

    pub(crate) fn finalizer(&self) -> Finalizer {
        self.finalizer
    }
}

/// # Safety
///
/// `data` is a live `T`, dropped once.
unsafe fn drop_object<T>(data: *mut u8) {
    // SAFETY: as the caller promises.
    unsafe { ptr::drop_in_place(data.cast::<T>()) }
}

/// A type's layout and its index in the table, 0 until its first
/// allocation. `derive(GcObject)` makes one, in a static, for each type.
#[doc(hidden)]
pub struct TypeSlot {
    info: &'static TypeInfo,
    index: AtomicU32,
}

impl TypeSlot {
    pub const fn new(info: &'static TypeInfo) -> Self {
        Self {
            info,
            index: AtomicU32::new(0),
        }
    }

    pub(crate) fn info(&self) -> &'static TypeInfo {
        self.info
    }

    /// The type's index in the table, registering it on first use. Inlined:
    /// an engine's binding reads it at every call (`type_index`).
    #[inline]
    pub(crate) fn index(&'static self) -> u32 {
        match self.index.load(Ordering::Acquire) {
            0 => register(self),
            index => index,
        }
    }

    /// The type, as registered.
    pub(crate) fn entry(&'static self) -> &'static Entry {
        let entry = TABLE[self.index() as usize].load(Ordering::Acquire);
        // SAFETY: a static type's entry, registered by `index`, leaked so
        // never freed, and never changed after.
        unsafe { &*entry }
    }
}

/// How many static types a process may have (cppgc's table grows; a fixed
/// one is enough for the DOM's types): every `type_index` is below it.
pub const MAX_TYPES: usize = 1 << 12;

/// The table's length: every index, a runtime type's too, is below it
/// (cppgc's `GCInfoTable::kMaxIndex` is 2^14). 2 MiB of zeroed pointers,
/// whose pages no one touches until a type is registered there.
pub(crate) const TABLE_LEN: usize = 1 << 18;

/// A registered type: its layout, and the offsets of its edges, of its
/// weak edges, of its ephemerons and of its JS edges, each in a row, what
/// marking reads
/// (cppgc's `GCInfo` holds the type's `Trace`).
pub(crate) struct Entry {
    pub(crate) info: TypeInfo,
    pub(crate) edges: Box<[u32]>,
    pub(crate) weak: Box<[u32]>,
    /// Each ephemeron's: its key's, its value 4 bytes after.
    pub(crate) ephemerons: Box<[u32]>,
    pub(crate) js_edges: Box<[u32]>,
    /// A runtime array's elements, after `info.size()` bytes.
    pub(crate) array: Option<crate::raw::ArrayLayout>,
    /// A runtime type, retired: nothing allocates it any more.
    retired: AtomicBool,
    /// What its embedder keeps alive with it, dropped with the entry.
    _keepalive: Option<Box<dyn Send + Sync>>,
}

impl Entry {
    /// A runtime type's bytes after the header: an array's from its length
    /// (its fixed part's, then its elements').
    ///
    /// # Safety
    ///
    /// `data` is an object of the type, whose length was written as it was
    /// allocated.
    pub(crate) unsafe fn raw_size(&self, data: *const u8) -> usize {
        let fixed = self.info.size() as usize;
        match self.array {
            None => fixed,
            Some(array) => {
                // SAFETY: as the caller promises.
                let length = unsafe { array.length(data) };
                fixed.saturating_add((length as usize).saturating_mul(array.element_bytes as usize))
            }
        }
    }

    /// A runtime type's edges' offsets: its fixed part's, then its
    /// elements' if they are edges.
    ///
    /// # Safety
    ///
    /// As `raw_size`.
    pub(crate) unsafe fn raw_edges(&self, data: *const u8) -> impl Iterator<Item = usize> + '_ {
        let fixed = self.info.size() as usize;
        let elements = match self.array {
            // SAFETY: as the caller promises.
            Some(array) if array.element_edges => (unsafe { array.length(data) }) as usize,
            _ => 0,
        };
        self.edges
            .iter()
            .map(|&at| at as usize)
            .chain((0..elements).map(move |i| fixed.saturating_add(i.saturating_mul(4))))
    }
}

/// The table: index to a registered type, 0 never used (an empty slot's
/// header). A static type's entry is made once and never freed; a runtime
/// type's is freed once retired and every heap finished a full GC after.
static TABLE: [AtomicPtr<Entry>; TABLE_LEN] =
    [const { AtomicPtr::new(ptr::null_mut()) }; TABLE_LEN];

/// The offsets of `info`'s edges (`count` gives a field's: its edges, its
/// weak edges or its ephemerons, `stride` bytes apart), field by field,
/// each field's in a row.
fn edge_offsets(info: &TypeInfo, count: fn(FieldKind) -> usize, stride: usize) -> Box<[u32]> {
    let mut edges = Vec::new();
    for field in info.fields() {
        for i in 0..count(field.kind) {
            let at = field.offset.saturating_add(i.saturating_mul(stride));
            edges.push(u32::try_from(at).expect("an object's size fits 32 bits"));
        }
    }
    edges.into_boxed_slice()
}

/// The next index, under the lock that registering takes.
static NEXT_INDEX: Mutex<u32> = Mutex::new(1);

#[cold]
fn register(slot: &'static TypeSlot) -> u32 {
    let mut next = NEXT_INDEX.lock().unwrap_or_else(PoisonError::into_inner);
    // Another thread may have registered it while this one waited.
    let index = slot.index.load(Ordering::Acquire);
    if index != 0 {
        return index;
    }
    let index = *next;
    let at = index as usize;
    assert!(at < MAX_TYPES, "more than {MAX_TYPES} GC types");
    let entry = Box::leak(Box::new(Entry {
        info: TypeInfo { ..*slot.info },
        edges: edge_offsets(slot.info, FieldKind::edges, 4),
        weak: edge_offsets(slot.info, FieldKind::weak_edges, 4),
        ephemerons: edge_offsets(slot.info, FieldKind::ephemerons, 8),
        js_edges: edge_offsets(slot.info, FieldKind::js_edges, 4),
        array: None,
        retired: AtomicBool::new(false),
        _keepalive: None,
    }));
    TABLE[at].store(ptr::from_mut(entry), Ordering::Release);
    slot.index.store(index, Ordering::Release);
    #[expect(
        clippy::arithmetic_side_effects,
        reason = "index < MAX_TYPES (asserted above), far below u32::MAX"
    )]
    {
        *next = index + 1;
    }
    index
}

/// Whether a type has `index`: the heap verifier's check of a header.
pub(crate) fn is_registered(index: u32) -> bool {
    !load(index).is_null()
}

/// The entry at `index`, null past the table. Indexed rather than through
/// `TABLE.get`: borrowing the whole 2 MiB table on each lookup costs a
/// checker that tracks borrows by the byte (Miri) that much per object a GC
/// visits.
fn load(index: u32) -> *mut Entry {
    let index = index as usize;
    if index < TABLE_LEN {
        TABLE[index].load(Ordering::Acquire)
    } else {
        ptr::null_mut()
    }
}

/// The layout of the type with `index`.
///
/// # Safety
///
/// As `entry`.
///
/// # Panics
///
/// As `entry`.
pub(crate) unsafe fn info<'a>(index: u32) -> &'a TypeInfo {
    // SAFETY: as the caller promises.
    unsafe { &entry(index).info }
}

/// The type with `index`, as registered.
///
/// # Safety
///
/// `index` is a static type's, or the header's of an object a GC may still
/// read (one of a heap's, or a dead one a GC of its heap may reach before
/// its sweep frees it): a runtime type's entry outlives every such object,
/// freed only once each heap finished a full GC that started after the
/// type was retired.
///
/// # Panics
///
/// If no type has it (a header that isn't one).
pub(crate) unsafe fn entry<'a>(index: u32) -> &'a Entry {
    let entry = load(index);
    assert!(!entry.is_null(), "no GC type has index {index}");
    // SAFETY: a registered entry, never changed after but its `retired`
    // flag; as the caller promises, not freed while it is read.
    unsafe { &*entry }
}

/// The runtime types' bookkeeping, under one lock, off every hot path:
/// the free indices, the live heaps' full GCs, and the retired types
/// waiting for them. What it frees it hands back, for the caller to drop
/// once the lock is let go (an embedder's keepalive may call back in).
struct Runtime {
    /// The free indices from `next` on, and below it the runs given back,
    /// each `[start, end)`, sorted, none touching the next.
    next: u32,
    given_back: Vec<(u32, u32)>,
    heaps: Vec<HeapEpochs>,
    next_heap: u64,
    retiring: Vec<Retiring>,
}

/// A live heap's full GCs: started, and finished (their sweep done).
struct HeapEpochs {
    id: u64,
    started: u64,
    finished: u64,
}

/// A retired type: freed once each heap listed finished that many full
/// GCs (or is gone), its index then free to its range again, or to every
/// range if its own was released.
struct Retiring {
    index: u32,
    released: bool,
    waits: Vec<(u64, u64)>,
}

static RUNTIME: Mutex<Runtime> = Mutex::new(Runtime::new());

fn runtime() -> MutexGuard<'static, Runtime> {
    RUNTIME.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Runtime {
    const fn new() -> Self {
        #[expect(clippy::cast_possible_truncation, reason = "MAX_TYPES is 2^12")]
        Self {
            next: MAX_TYPES as u32,
            given_back: Vec::new(),
            heaps: Vec::new(),
            next_heap: 0,
            retiring: Vec::new(),
        }
    }

    /// `len` free indices in a row: the first of them.
    fn reserve(&mut self, len: u32) -> Option<u32> {
        if let Some(at) = self
            .given_back
            .iter()
            .position(|&(start, end)| end.saturating_sub(start) >= len)
        {
            let (start, end) = self.given_back[at];
            if end.saturating_sub(start) == len {
                self.given_back.remove(at);
            } else {
                self.given_back[at].0 = start.saturating_add(len);
            }
            return Some(start);
        }
        #[expect(clippy::cast_possible_truncation, reason = "TABLE_LEN is 2^18")]
        let end = self
            .next
            .checked_add(len)
            .filter(|&end| end <= TABLE_LEN as u32)?;
        let start = self.next;
        self.next = end;
        Some(start)
    }

    /// Frees the indices `[start, end)`.
    fn give_back(&mut self, start: u32, end: u32) {
        if start >= end {
            return;
        }
        let mut at = self.given_back.partition_point(|&(s, _)| s < start);
        self.given_back.insert(at, (start, end));
        // Merged with the next run, then with the one before.
        let next = at.saturating_add(1);
        if self.given_back.get(next).is_some_and(|&(s, _)| s == end) {
            self.given_back[at].1 = self.given_back[next].1;
            self.given_back.remove(next);
        }
        if let Some(before) = at.checked_sub(1)
            && self.given_back[before].1 == start
        {
            self.given_back[before].1 = self.given_back[at].1;
            self.given_back.remove(at);
            at = before;
        }
        // The last run, reaching `next`, lowers it.
        if at.saturating_add(1) == self.given_back.len() && self.given_back[at].1 == self.next {
            self.next = self.given_back[at].0;
            self.given_back.pop();
        }
    }

    /// Frees the indices `[start, start + len)`, which `reserve` gave: the
    /// empty ones now, the retired ones once their entries are freed. A
    /// registered one stays taken for the process's life.
    fn release(&mut self, start: u32, len: u32) -> Vec<Entry> {
        let end = start.saturating_add(len);
        let mut run = start;
        for index in start..end {
            if TABLE[index as usize].load(Ordering::Acquire).is_null() {
                continue;
            }
            self.give_back(run, index);
            run = index.saturating_add(1);
            if let Some(retiring) = self
                .retiring
                .iter_mut()
                .find(|retiring| retiring.index == index)
            {
                retiring.released = true;
            }
        }
        self.give_back(run, end);
        self.reclaim()
    }

    fn add_heap(&mut self) -> u64 {
        let id = self.next_heap;
        self.next_heap = id.saturating_add(1);
        self.heaps.push(HeapEpochs {
            id,
            started: 0,
            finished: 0,
        });
        id
    }

    fn remove_heap(&mut self, id: u64) -> Vec<Entry> {
        self.heaps.retain(|heap| heap.id != id);
        self.reclaim()
    }

    fn heap(&mut self, id: u64) -> &mut HeapEpochs {
        self.heaps
            .iter_mut()
            .find(|heap| heap.id == id)
            .expect("a live heap is listed")
    }

    fn gc_started(&mut self, id: u64) {
        let heap = self.heap(id);
        heap.started = heap.started.saturating_add(1);
    }

    fn gc_finished(&mut self, id: u64) -> Vec<Entry> {
        let heap = self.heap(id);
        heap.finished = heap.finished.saturating_add(1);
        self.reclaim()
    }

    /// Retires the runtime type at `index`: it waits for each heap alive
    /// now to finish a full GC that starts after.
    ///
    /// # Safety
    ///
    /// As `retire`.
    unsafe fn retire(&mut self, index: u32) -> Vec<Entry> {
        let entry = TABLE[index as usize].load(Ordering::Acquire);
        assert!(!entry.is_null(), "no GC type has index {index}");
        // SAFETY: a registered entry, freed only by `reclaim`, which the
        // lock this is under (or the caller's `&mut`) keeps from running.
        let entry = unsafe { &*entry };
        assert!(
            entry.info.kind() == Kind::Raw,
            "a static GC type is never retired"
        );
        assert!(
            !entry.retired.swap(true, Ordering::Relaxed),
            "the GC type with index {index} is retired already"
        );
        // A full GC under way started before: the one after it is waited
        // for.
        let waits = self
            .heaps
            .iter()
            .map(|heap| (heap.id, heap.started.saturating_add(1)))
            .collect();
        self.retiring.push(Retiring {
            index,
            released: false,
            waits,
        });
        self.reclaim()
    }

    /// The retired types every heap waited for, taken out of the table.
    fn reclaim(&mut self) -> Vec<Entry> {
        let mut freed = Vec::new();
        let mut at = 0;
        while at < self.retiring.len() {
            let done = self.retiring[at].waits.iter().all(|&(id, needed)| {
                self.heaps
                    .iter()
                    .find(|heap| heap.id == id)
                    .is_none_or(|heap| heap.finished >= needed)
            });
            if !done {
                at = at.saturating_add(1);
                continue;
            }
            let retiring = self.retiring.swap_remove(at);
            let entry = TABLE[retiring.index as usize].swap(ptr::null_mut(), Ordering::AcqRel);
            // SAFETY: a runtime type's entry, made by `Box::into_raw` in
            // `register_raw`, taken out of the table just now under the lock
            // (which registering takes too), so freed once; no GC reads it
            // any more: every heap that may hold one of its objects finished
            // a full GC that started after it was retired, whose sweep freed
            // them all.
            freed.push(*unsafe { Box::from_raw(entry) });
            if retiring.released {
                self.give_back(retiring.index, retiring.index.saturating_add(1));
            }
        }
        freed
    }
}

/// A heap's place in the list the retired types wait on: listed as it is
/// made, gone with it.
pub(crate) struct HeapRecord(u64);

impl HeapRecord {
    pub(crate) fn new() -> Self {
        Self(runtime().add_heap())
    }

    /// A full GC of the heap starts, the last one's sweep finished.
    pub(crate) fn gc_started(&self) {
        runtime().gc_started(self.0);
    }

    /// The heap's full GC finished: its sweep is done.
    pub(crate) fn gc_finished(&self) {
        let freed = runtime().gc_finished(self.0);
        drop(freed);
    }
}

impl Drop for HeapRecord {
    fn drop(&mut self) {
        let freed = runtime().remove_heap(self.0);
        drop(freed);
    }
}

/// `len` free indices in a row, for an embedder's runtime types: the
/// first, or `None` if the table has no such run.
pub(crate) fn reserve(len: u32) -> Option<u32> {
    runtime().reserve(len)
}

/// Frees the indices `[start, start + len)`, which `reserve` gave
/// (`Runtime::release`).
pub(crate) fn release(start: u32, len: u32) {
    let freed = runtime().release(start, len);
    drop(freed);
}

/// A type has the index already, registered or retired and not yet freed:
/// what `register_raw` refuses.
pub(crate) struct Occupied;

/// Registers a runtime type at `index` (reserved), its layout checked.
pub(crate) fn register_raw(
    index: u32,
    size: u32,
    edges: Box<[u32]>,
    array: Option<crate::raw::ArrayLayout>,
    keepalive: Box<dyn Send + Sync>,
) -> Result<(), Occupied> {
    let entry = Box::new(Entry {
        info: TypeInfo::raw(size),
        edges,
        weak: Box::new([]),
        ephemerons: Box::new([]),
        js_edges: Box::new([]),
        array,
        retired: AtomicBool::new(false),
        _keepalive: Some(keepalive),
    });
    {
        // Under the lock: `reclaim` empties a slot under it.
        let _runtime = runtime();
        let slot = &TABLE[index as usize];
        if slot.load(Ordering::Acquire).is_null() {
            slot.store(Box::into_raw(entry), Ordering::Release);
            return Ok(());
        }
    }
    // The keepalive dropped outside the lock.
    drop(entry);
    Err(Occupied)
}

/// Retires the runtime type at `index`: its entry is freed once every heap
/// alive now finished a full GC that starts after.
///
/// # Safety
///
/// Nothing allocates the type from now on (nor races with this), and no
/// object of it is reachable in any heap (from a root, an edge, a weak
/// edge or an id).
///
/// # Panics
///
/// If no runtime type that isn't retired has `index`.
pub(crate) unsafe fn retire(index: u32) {
    // SAFETY: as the caller promises.
    let freed = unsafe { runtime().retire(index) };
    drop(freed);
}

/// The runtime type with `index`, to allocate in this operation.
///
/// # Panics
///
/// If no runtime type has it, or it is retired.
pub(crate) fn raw_entry<'a>(index: u32) -> &'a Entry {
    let entry = load(index);
    assert!(!entry.is_null(), "no GC type has index {index}");
    // SAFETY: a registered entry. Allocating it now, the caller isn't past
    // its retirement (whose promise is that nothing allocates it after, nor
    // races with it: the check below only catches a mistake made in order),
    // so it isn't freed before every heap alive then finished a full GC
    // after now, which none does during this operation.
    let entry = unsafe { &*entry };
    assert!(
        entry.info.kind() == Kind::Raw,
        "the GC type with index {index} is a static one"
    );
    assert!(
        !entry.retired.load(Ordering::Relaxed),
        "the GC type with index {index} is retired"
    );
    entry
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// Counts its drops.
    struct Drops(Arc<AtomicUsize>);

    impl Drop for Drops {
        fn drop(&mut self) {
            #[expect(clippy::disallowed_methods, reason = "a test's count")]
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A runtime type registered at an index the process's runtime reserved
    /// (no other test takes it), for a runtime of the test's to retire; the
    /// count of its keepalive's drops.
    fn registered() -> (u32, Arc<AtomicUsize>) {
        let index = reserve(1).expect("a free index");
        let drops = Arc::new(AtomicUsize::new(0));
        assert!(
            register_raw(
                index,
                8,
                Box::new([0]),
                None,
                Box::new(Drops(drops.clone()))
            )
            .is_ok()
        );
        (index, drops)
    }

    fn freed(drops: &Arc<AtomicUsize>, entries: Vec<Entry>) -> usize {
        drop(entries);
        drops.load(Ordering::Relaxed)
    }

    #[test]
    fn a_retired_type_waits_for_every_heaps_next_full_gc() {
        let mut runtime = Runtime::new();
        let (a, b) = (runtime.add_heap(), runtime.add_heap());
        let (index, drops) = registered();
        // SAFETY: nothing allocates the type, of which no object exists.
        let entries = unsafe { runtime.retire(index) };
        assert_eq!(freed(&drops, entries), 0);
        runtime.gc_started(a);
        let entries = runtime.gc_finished(a);
        assert_eq!(freed(&drops, entries), 0, "b hasn't collected");
        runtime.gc_started(b);
        let entries = runtime.gc_finished(b);
        assert_eq!(freed(&drops, entries), 1);
        assert!(!is_registered(index));
    }

    #[test]
    fn a_full_gc_under_way_as_a_type_retires_isnt_enough() {
        let mut runtime = Runtime::new();
        let heap = runtime.add_heap();
        let (index, drops) = registered();
        runtime.gc_started(heap);
        // SAFETY: as above.
        let entries = unsafe { runtime.retire(index) };
        assert_eq!(freed(&drops, entries), 0);
        let entries = runtime.gc_finished(heap);
        assert_eq!(freed(&drops, entries), 0, "its marking began before");
        runtime.gc_started(heap);
        let entries = runtime.gc_finished(heap);
        assert_eq!(freed(&drops, entries), 1);
    }

    #[test]
    fn a_heap_gone_or_made_after_doesnt_hold_a_retired_type() {
        let mut runtime = Runtime::new();
        let old = runtime.add_heap();
        let (index, drops) = registered();
        // SAFETY: as above.
        let entries = unsafe { runtime.retire(index) };
        assert_eq!(freed(&drops, entries), 0);
        let young = runtime.add_heap();
        runtime.gc_started(young);
        let entries = runtime.gc_finished(young);
        assert_eq!(freed(&drops, entries), 0);
        let entries = runtime.remove_heap(old);
        assert_eq!(freed(&drops, entries), 1);
    }

    #[test]
    fn with_no_heap_a_retired_type_is_freed_at_once() {
        let mut runtime = Runtime::new();
        let (index, drops) = registered();
        // SAFETY: as above.
        let entries = unsafe { runtime.retire(index) };
        assert_eq!(freed(&drops, entries), 1);
        assert!(register_raw(index, 8, Box::new([]), None, Box::new(())).is_ok());
    }

    #[test]
    fn a_retired_index_is_taken_until_freed() {
        let mut runtime = Runtime::new();
        let heap = runtime.add_heap();
        let (index, drops) = registered();
        // SAFETY: as above.
        drop(unsafe { runtime.retire(index) });
        assert!(register_raw(index, 8, Box::new([]), None, Box::new(())).is_err());
        runtime.gc_started(heap);
        assert_eq!(freed(&drops, runtime.gc_finished(heap)), 1);
        assert!(register_raw(index, 8, Box::new([]), None, Box::new(())).is_ok());
    }

    #[test]
    fn ranges_given_back_are_merged_and_reused() {
        let mut runtime = Runtime::new();
        let a = runtime.reserve(10).unwrap();
        let b = runtime.reserve(10).unwrap();
        let c = runtime.reserve(10).unwrap();
        assert_eq!((a, b, c), (4096, 4106, 4116));
        runtime.give_back(a, a + 10);
        // The first run that fits, split.
        assert_eq!(runtime.reserve(4), Some(a));
        assert_eq!(runtime.reserve(7), Some(c + 10));
        runtime.give_back(b, b + 10);
        // Merged with what is left of `a`'s.
        assert_eq!(runtime.given_back, [(a + 4, b + 10)]);
        assert_eq!(runtime.reserve(16), Some(a + 4));
        // The last run lowers `next`.
        runtime.give_back(c, c + 17);
        assert_eq!((runtime.next, runtime.given_back.len()), (c, 0));
        assert_eq!(runtime.reserve(TABLE_LEN as u32), None);
        assert_eq!(runtime.reserve(TABLE_LEN as u32 - c), Some(c));
    }

    #[test]
    fn a_released_range_gives_back_its_empty_and_freed_indices() {
        let mut runtime = Runtime::new();
        let heap = runtime.add_heap();
        // Indices the process's runtime reserved, for no other test to
        // touch, which the test's runtime never gave: its free ones are
        // past them, none of them given back.
        runtime.next = TABLE_LEN as u32;
        let global = reserve(4).expect("free indices");
        let drops = Arc::new(AtomicUsize::new(0));
        for at in [1, 2] {
            assert!(
                register_raw(
                    global + at,
                    8,
                    Box::new([]),
                    None,
                    Box::new(Drops(drops.clone()))
                )
                .is_ok()
            );
        }
        // SAFETY: as above.
        drop(unsafe { runtime.retire(global + 2) });
        drop(runtime.release(global, 4));
        let given: Vec<_> = runtime.given_back.clone();
        assert_eq!(
            given,
            [(global, global + 1), (global + 3, global + 4)],
            "the registered and the retired stay taken"
        );
        runtime.gc_started(heap);
        assert_eq!(freed(&drops, runtime.gc_finished(heap)), 1);
        assert_eq!(
            runtime.given_back,
            [(global, global + 1), (global + 2, global + 4)]
        );
    }
}
