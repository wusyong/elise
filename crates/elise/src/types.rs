//! What the GC knows of each type, and the process's table of them: an
//! object's header holds its type's index there (cppgc's `GCInfoTable`;
//! an index rather than a pointer, so that the heap holds no native
//! pointer of its own). A type's index is assigned the first time it is
//! allocated and kept in a static next to its layout (`TypeSlot`), as
//! cppgc's `GCInfoTrait<T>::Index()`: a type's layout is code, the same in
//! every renderer.

use std::mem::{align_of, needs_drop, size_of};
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};
use std::sync::{Mutex, PoisonError};

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
}

/// How many types a process may have (cppgc's table grows; a fixed one is
/// enough for the DOM's types): every `type_index` is below it.
pub const MAX_TYPES: usize = 1 << 12;

/// A registered type: its layout, and the offsets of its edges, of its
/// weak edges, of its ephemerons and of its JS edges, each in a row, what
/// marking reads
/// (cppgc's `GCInfo` holds the type's `Trace`).
pub(crate) struct Entry {
    pub(crate) info: &'static TypeInfo,
    pub(crate) edges: Box<[u32]>,
    pub(crate) weak: Box<[u32]>,
    /// Each ephemeron's: its key's, its value 4 bytes after.
    pub(crate) ephemerons: Box<[u32]>,
    pub(crate) js_edges: Box<[u32]>,
}

/// The table: index to a registered type, made once and never freed, 0
/// never used (an empty slot's header).
static TABLE: [AtomicPtr<Entry>; MAX_TYPES] =
    [const { AtomicPtr::new(ptr::null_mut()) }; MAX_TYPES];

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
        info: slot.info,
        edges: edge_offsets(slot.info, FieldKind::edges, 4),
        weak: edge_offsets(slot.info, FieldKind::weak_edges, 4),
        ephemerons: edge_offsets(slot.info, FieldKind::ephemerons, 8),
        js_edges: edge_offsets(slot.info, FieldKind::js_edges, 4),
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
    TABLE
        .get(index as usize)
        .is_some_and(|entry| !entry.load(Ordering::Acquire).is_null())
}

/// The layout of the type with `index`.
///
/// # Panics
///
/// If no type has it (a header that isn't one).
pub(crate) fn info(index: u32) -> &'static TypeInfo {
    entry(index).info
}

/// The type with `index`, as registered.
///
/// # Panics
///
/// If no type has it (a header that isn't one).
pub(crate) fn entry(index: u32) -> &'static Entry {
    let entry = TABLE
        .get(index as usize)
        .map_or(ptr::null_mut(), |entry| entry.load(Ordering::Acquire));
    assert!(!entry.is_null(), "no GC type has index {index}");
    // SAFETY: a registered entry, leaked so never freed, and never
    // changed after.
    unsafe { &*entry }
}
