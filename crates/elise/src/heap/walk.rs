//! A walk of every object the heap holds, for a heap snapshot (V8's
//! `HeapObjectIterator` as its snapshot generator walks it, cppgc's
//! `HeapVisitor` as its graph builder does): each object's type, size,
//! edges (by their fields' names) and JS edges. Only for snapshots: it
//! looks at every page, and nothing else calls it.
//!
//! It walks what allocation left, not what the roots reach: the objects an
//! engine's wrappers hold are reached from nowhere in the heap. At a
//! safepoint with the sweep finished (as V8 finishes its own before a
//! snapshot), a slot of a size class's page holds an object unless it is
//! free, in one of the page's ranges or in its class's cursor's; a large
//! object's pages hold it; a young page holds objects and fillers from its
//! start to its top. Those are the objects the last GC kept and the ones
//! allocated since, garbage or not: right after a full GC (V8's snapshot
//! collects first), the live ones.

use std::ffi::CStr;
use std::ptr::NonNull;

use super::{Cursor, HEADER_BYTES, Inner, PageState, large_data, young};
use crate::page::{self, PAGE_BYTES};
use crate::types::{self, Kind, TypeInfo};
use crate::{Erased, Gc, JsEdge};

/// The heap's objects at a safepoint (`Heap::walk`), for one operation.
pub struct Walk<'gc> {
    inner: &'gc Inner,
    /// Their data's offsets.
    objects: Vec<usize>,
}

/// An object of a walk.
#[derive(Clone, Copy)]
pub struct HeapObject<'gc> {
    inner: &'gc Inner,
    data: usize,
    ty: &'static TypeInfo,
}

impl std::fmt::Debug for HeapObject<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{:#x}", self.ty.name(), self.data)
    }
}

/// What an edge of an object is.
#[derive(Clone, Copy)]
pub enum EdgeKind<'gc> {
    /// A `Member`'s (an array's too): it keeps the object alive.
    Strong,
    /// A `WeakMember`'s, or an ephemeron's key.
    Weak,
    /// An ephemeron's value, alive while its key is.
    Ephemeron { key: HeapObject<'gc> },
}

/// An edge of an object to another, by its field's name (none for an
/// array's).
#[derive(Clone, Copy)]
pub struct Edge<'gc> {
    pub name: Option<&'static CStr>,
    pub kind: EdgeKind<'gc>,
    pub to: HeapObject<'gc>,
}

impl<'gc> Walk<'gc> {
    /// The heap's objects; the sweep is done, and the platform's threads
    /// mark at most (they write no object).
    pub(super) fn new(inner: &'gc Inner) -> Self {
        let objects = inner.objects();
        // What the walk hands out may be what a marking would free: what
        // was garbage at its start, what only a weak edge reaches. Marked
        // as a weak read marks (SATB's floating garbage), so that a `Gc`
        // of it written into a live object stays valid.
        for &data in &objects {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "an object's offset in the 4 GiB cage"
            )]
            inner.read_weak(data as u32);
        }
        Self { inner, objects }
    }

    /// Every object, lowest address first on each page.
    pub fn objects(&self) -> impl ExactSizeIterator<Item = HeapObject<'gc>> + '_ {
        let inner = self.inner;
        // SAFETY: the heap's objects (`Inner::objects`), which no GC frees
        // before the operation ends.
        self.objects
            .iter()
            .map(move |&data| unsafe { HeapObject::new(inner, data) })
    }

    pub fn len(&self) -> usize {
        self.objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    /// The objects the heap's roots (`Persistent`s) hold.
    pub fn roots(&self) -> impl Iterator<Item = HeapObject<'gc>> + use<'gc> {
        let inner = self.inner;
        inner
            .shared
            .roots
            .in_use()
            .into_iter()
            // SAFETY: a root's object, alive (and marked, if a marking
            // runs: roots are) until the operation ends.
            .map(move |data| unsafe { HeapObject::new(inner, data) })
    }
}

impl<'gc> HeapObject<'gc> {
    /// The object at `data`, of `inner`'s heap.
    ///
    /// # Safety
    ///
    /// `data` is a whole object of the heap (its type registered), which
    /// no GC frees before `'gc` ends.
    unsafe fn new(inner: &'gc Inner, data: usize) -> Self {
        Self {
            inner,
            data,
            // SAFETY: as the caller promises: its header, in a committed
            // page; an object the heap holds, live or not yet freed, whose
            // type `entry` may read: a runtime type is freed only by a full
            // GC that started after it retired, whose start left the
            // nursery none of the type's objects and whose sweep (which
            // `Heap::walk` finishes first) freed its old ones.
            ty: unsafe { types::info(inner.header(data).read()) },
        }
    }

    /// The object, read back as its type with `Gc::downcast`.
    pub fn get(self) -> Gc<'gc, Erased> {
        // SAFETY: an object of the walk: whole, in the cage (so not null),
        // and no GC runs before the walk's operation ends.
        unsafe { Gc::from_raw(NonNull::new_unchecked(self.inner.ptr(self.data).cast())) }
    }

    /// Its address: what `Mutation::engine_address` gives of it, which an
    /// engine's wrapper of it holds.
    pub fn address(self) -> NonNull<u8> {
        self.get().as_non_null().cast()
    }

    /// Its type's name (the struct's).
    pub fn type_name(self) -> &'static str {
        self.ty.name()
    }

    /// Its type's number (`type_index`).
    pub fn type_index(self) -> u32 {
        // SAFETY: as `Walk::objects` read it.
        unsafe { self.inner.header(self.data).read() }
    }

    /// Its bytes, header included.
    pub fn size(self) -> usize {
        self.inner.object_bytes(self.data)
    }

    /// Its edges to other objects, in its fields' order; empty ones left
    /// out.
    pub fn edges(self) -> impl Iterator<Item = Edge<'gc>> {
        let inner = self.inner;
        let data = self.data;
        // SAFETY: a `u32` of the object, at `at` (an edge's, by its type's
        // layout).
        let read = move |at: usize| unsafe { inner.ptr(data).add(at).cast::<u32>().read() };
        let target = move |raw: u32| {
            // SAFETY: an edge of an object of the walk: 0, or an object
            // the heap holds (a strong edge's keeps it; a weak edge's
            // target that died was cleared as it was freed), so one of
            // the walk's.
            (raw != 0).then(|| unsafe { HeapObject::new(inner, raw as usize) })
        };
        let mut edges = Vec::new();
        match self.ty.kind() {
            Kind::Fixed => {
                for field in self.ty.fields() {
                    let name = Some(field.name);
                    let kind = field.kind;
                    for i in 0..kind.edges() {
                        if let Some(to) =
                            target(read(field.offset.saturating_add(i.saturating_mul(4))))
                        {
                            edges.push(Edge {
                                name,
                                kind: EdgeKind::Strong,
                                to,
                            });
                        }
                    }
                    for i in 0..kind.weak_edges() {
                        if let Some(to) =
                            target(read(field.offset.saturating_add(i.saturating_mul(4))))
                        {
                            edges.push(Edge {
                                name,
                                kind: EdgeKind::Weak,
                                to,
                            });
                        }
                    }
                    for i in 0..kind.ephemerons() {
                        let at = field.offset.saturating_add(i.saturating_mul(8));
                        let key = target(read(at));
                        if let Some(key) = key {
                            edges.push(Edge {
                                name,
                                kind: EdgeKind::Weak,
                                to: key,
                            });
                        }
                        if let Some(to) = target(read(at.saturating_add(4))) {
                            edges.push(Edge {
                                name,
                                kind: key.map_or(EdgeKind::Weak, |key| EdgeKind::Ephemeron { key }),
                                to,
                            });
                        }
                    }
                }
            }
            Kind::Raw => {
                // SAFETY: an object of the walk, its type's as `new` read
                // it: whole, its length (an array's) written before it was
                // published.
                let entry = unsafe { types::entry(self.type_index()) };
                // SAFETY: as above.
                for at in unsafe { entry.raw_edges(self.inner.ptr(data)) } {
                    let edge = read(at);
                    // An `I31` is no object.
                    if crate::raw::is_i31(edge) {
                        continue;
                    }
                    if let Some(to) = target(edge) {
                        edges.push(Edge {
                            name: None,
                            kind: EdgeKind::Strong,
                            to,
                        });
                    }
                }
            }
            Kind::MemberArray => {
                let capacity = read(0) as usize;
                for i in 0..capacity {
                    if let Some(to) = target(read(i.saturating_add(1).saturating_mul(4))) {
                        edges.push(Edge {
                            name: None,
                            kind: EdgeKind::Strong,
                            to,
                        });
                    }
                }
            }
        }
        edges.into_iter()
    }

    /// Its JS edges (what an engine's references are: its wrapper's, its
    /// listeners'), by their fields' names.
    pub fn js_edges(self) -> impl Iterator<Item = (&'static CStr, &'gc JsEdge)> {
        let inner = self.inner;
        let data = self.data;
        self.ty
            .fields()
            .iter()
            .filter(|field| field.kind == types::FieldKind::JsEdge)
            .map(move |field| {
                // SAFETY: a `JsEdge` of an object of the walk, by its type's
                // layout, alive until the operation ends.
                let edge = unsafe { &*inner.ptr(data).add(field.offset).cast::<JsEdge>() };
                (field.name, edge)
            })
    }
}

impl Inner {
    /// The data offsets of every object the heap holds, at a safepoint with
    /// no sweep running: a size class's page's slots but its free ranges
    /// and its class's cursor's, each large object, each young page's
    /// objects up to its top.
    fn objects(&self) -> Vec<usize> {
        let space = self.space.borrow();
        let young = self.young.borrow();
        let young_cursor = self.young_cursor.get();
        let mut objects = Vec::new();
        for (number, state) in space.states.iter().enumerate() {
            match state {
                PageState::Small(page) => {
                    assert!(!page.unswept, "the walk is after the sweep");
                    let mut free = vec![false; page.slots];
                    let mut set = |first: usize, end: usize| {
                        for slot in free.iter_mut().take(end).skip(first) {
                            *slot = true;
                        }
                    };
                    for &(first, end) in &page.ranges {
                        set(first as usize, end as usize);
                    }
                    let cursor: Cursor = self.cursors[page.class].get();
                    if cursor.page == number && cursor.next < cursor.limit {
                        // Data offsets of slots: from the page's start,
                        // header first.
                        let slot = |data: usize| {
                            data.saturating_sub(page.start)
                                .saturating_sub(HEADER_BYTES)
                                .checked_div(page.slot_bytes)
                                .unwrap_or(0)
                        };
                        set(slot(cursor.next), slot(cursor.limit));
                    }
                    for (slot, &free) in free.iter().enumerate() {
                        if !free {
                            self.push_object(&mut objects, page.data_of(slot));
                        }
                    }
                }
                PageState::Large(_) => self.push_object(&mut objects, large_data(number)),
                _ => {}
            }
        }
        for &number in &young.pages {
            let top = match space.states.get(number) {
                Some(PageState::Young { .. }) if young_cursor.page == number => young_cursor.next,
                Some(PageState::Young { top }) => *top,
                _ => continue,
            };
            let mut at = page::start_of(number);
            let end = top.min(at.saturating_add(PAGE_BYTES));
            while at < end {
                let data = at.saturating_add(HEADER_BYTES);
                // SAFETY: a header the walk reached, on a young page: its
                // objects and fillers lie one after another up to its top.
                if unsafe { self.header(data).read() } != young::FILLER {
                    self.push_object(&mut objects, data);
                }
                at = at.saturating_add(self.object_bytes(data).max(HEADER_BYTES));
            }
        }
        objects
    }

    /// Records the object at `data`, if its header has a type: a slot an
    /// allocation took is published before its operation goes on (its
    /// fields' writers don't panic), so only a slot never written (a new
    /// page's zeros, index 0) or poisoned has none.
    fn push_object(&self, objects: &mut Vec<usize>, data: usize) {
        // SAFETY: a slot's header, in a committed page.
        let index = unsafe { self.header(data).read() };
        if types::is_registered(index) {
            objects.push(data);
        }
    }
}
