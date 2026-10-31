use std::cell::UnsafeCell;
use std::fmt;
use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::member::compress;
use crate::{Gc, HeapType, Mutation};

/// A root (cppgc's `Persistent`, its node in Blink's `PersistentRegion`):
/// an entry of its heap's root table, which keeps the object alive. It
/// may outlive an operation, not the heap: it holds the table by address,
/// with no count (a heap dropped with roots left panics in a debug build,
/// and leaks the table in a release one, so that their drops still write
/// into it).
pub struct Persistent<T: HeapType> {
    table: NonNull<RootTable>,
    index: u32,
    _target: PhantomData<fn() -> T>,
}

impl<T: HeapType> Persistent<T> {
    pub fn new<'gc>(cx: &Mutation<'gc>, object: Gc<'gc, T>) -> Self {
        Self::of(
            NonNull::from(&cx.inner.shared.roots),
            compress(cx, Some(object)),
        )
    }

    fn of(table: NonNull<RootTable>, offset: u32) -> Self {
        // SAFETY: the heap's table, which lives as long as its roots do
        // (`Heap`'s drop leaks it while any is left).
        let index = unsafe { table.as_ref() }.add(offset);
        Self {
            table,
            index,
            _target: PhantomData,
        }
    }

    /// Its entry in the root table.
    pub fn index(&self) -> u32 {
        self.index
    }

    /// # Panics
    ///
    /// If `cx` is another heap's: the check is always on, as the entry
    /// means nothing there.
    pub fn get<'gc>(&self, cx: &Mutation<'gc>) -> Gc<'gc, T> {
        assert!(
            std::ptr::eq(self.table.as_ptr(), &raw const cx.inner.shared.roots),
            "a Persistent of another heap"
        );
        let offset = cx.inner.shared.roots.get(self.index);
        // SAFETY: a root is a live `T` of its heap, which `cx` is.
        unsafe { Gc::from_raw(NonNull::new_unchecked(cx.inner.ptr(offset as usize).cast())) }
    }
}

impl<T: HeapType> Clone for Persistent<T> {
    fn clone(&self) -> Self {
        // SAFETY: as `of`'s.
        let offset = unsafe { self.table.as_ref() }.get(self.index);
        Self::of(self.table, offset)
    }
}

impl<T: HeapType> Drop for Persistent<T> {
    fn drop(&mut self) {
        // SAFETY: as `of`'s.
        unsafe { self.table.as_ref() }.remove(self.index);
    }
}

impl<T: HeapType> fmt::Debug for Persistent<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Persistent({})", self.index)
    }
}

/// A weak root (cppgc's `WeakPersistent`): an entry of its heap's table of
/// weak roots, which doesn't keep the object alive. A GC clears the entry
/// once the object dies; read while a concurrent marking runs, the object
/// is marked, as a `WeakMember`'s. Its removal needs no barrier: it cuts
/// no path that keeps anything alive. It holds its table as `Persistent`
/// does.
pub struct WeakPersistent<T: HeapType> {
    table: NonNull<RootTable>,
    index: u32,
    _target: PhantomData<fn() -> T>,
}

impl<T: HeapType> WeakPersistent<T> {
    pub fn new<'gc>(cx: &Mutation<'gc>, object: Gc<'gc, T>) -> Self {
        let table = &cx.inner.shared.weak_roots;
        let index = table.add(compress(cx, Some(object)));
        Self {
            table: NonNull::from(table),
            index,
            _target: PhantomData,
        }
    }

    /// Its object, if it is alive.
    ///
    /// # Panics
    ///
    /// If `cx` is another heap's.
    pub fn get<'gc>(&self, cx: &Mutation<'gc>) -> Option<Gc<'gc, T>> {
        assert!(
            std::ptr::eq(self.table.as_ptr(), &raw const cx.inner.shared.weak_roots),
            "a WeakPersistent of another heap"
        );
        let offset = cx.inner.shared.weak_roots.get(self.index);
        cx.inner.read_weak(offset);
        // SAFETY: a weak root's entry is a live `T` of its heap, which `cx`
        // is, or 0 once it died (marked just now if a marking runs).
        (offset != 0).then(|| unsafe {
            Gc::from_raw(NonNull::new_unchecked(cx.inner.ptr(offset as usize).cast()))
        })
    }
}

impl<T: HeapType> Drop for WeakPersistent<T> {
    fn drop(&mut self) {
        // SAFETY: as `Persistent::of`'s.
        unsafe { self.table.as_ref() }.remove(self.index);
    }
}

impl<T: HeapType> fmt::Debug for WeakPersistent<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WeakPersistent({})", self.index)
    }
}

/// The root table: each entry an object's offset (0 when free), and the
/// free entries. Every method borrows its entries for its own body alone
/// and runs no code of anyone else's meanwhile (no callback, no drop of a
/// root), so no two borrows overlap and none is checked at run time
/// (Blink's `PersistentRegion`); `UnsafeCell` keeps it to the heap's
/// thread (`!Sync`).
#[derive(Default)]
pub(crate) struct RootTable {
    entries: UnsafeCell<Entries>,
}

#[derive(Default)]
struct Entries {
    slots: Vec<u32>,
    free: Vec<u32>,
    /// While a concurrent marking runs, the offsets of the roots removed,
    /// for the heap to mark at its next safepoint (SATB's barrier for
    /// roots: a `Persistent` is dropped anywhere, without the heap).
    removed: Option<Vec<u32>>,
}

impl RootTable {
    /// `f` on the entries: none of the callers' `f`s touches the table or
    /// runs code that could (each is one of this type's methods' bodies).
    fn with<R>(&self, f: impl FnOnce(&mut Entries) -> R) -> R {
        // SAFETY: the only borrow of the entries while `f` runs: `self` is
        // the heap's thread's alone (`!Sync`), and `f` doesn't re-enter.
        f(unsafe { &mut *self.entries.get() })
    }

    fn add(&self, offset: u32) -> u32 {
        debug_assert_ne!(offset, 0);
        self.with(|entries| {
            if let Some(index) = entries.free.pop() {
                entries.slots[index as usize] = offset;
                return index;
            }
            let index = u32::try_from(entries.slots.len()).expect("fewer than 2^32 roots");
            entries.slots.push(offset);
            index
        })
    }

    fn get(&self, index: u32) -> u32 {
        self.with(|entries| entries.slots[index as usize])
    }

    fn remove(&self, index: u32) {
        self.with(|entries| {
            let offset = std::mem::take(&mut entries.slots[index as usize]);
            if let Some(removed) = &mut entries.removed {
                removed.push(offset);
            }
            entries.free.push(index);
        });
    }

    /// Starts (or stops) keeping the offsets of the roots removed.
    pub(crate) fn watch_removals(&self, watch: bool) {
        self.with(|entries| entries.removed = watch.then(Vec::new));
    }

    /// The offsets of the roots removed since last asked.
    pub(crate) fn take_removed(&self) -> Vec<u32> {
        self.with(|entries| {
            entries
                .removed
                .as_mut()
                .map(std::mem::take)
                .unwrap_or_default()
        })
    }

    /// Whether no root was removed since last asked.
    pub(crate) fn removed_none(&self) -> bool {
        self.with(|entries| entries.removed.as_ref().is_none_or(Vec::is_empty))
    }

    /// The offsets of the entries in use, now.
    pub(crate) fn in_use(&self) -> Vec<usize> {
        self.with(|entries| {
            entries
                .slots
                .iter()
                .filter(|&&offset| offset != 0)
                .map(|&offset| offset as usize)
                .collect()
        })
    }

    /// The entries, free ones (0) too, for a scavenge to update: put back
    /// with `put_entries`.
    pub(crate) fn take_entries(&self) -> Vec<u32> {
        self.with(|entries| std::mem::take(&mut entries.slots))
    }

    pub(crate) fn put_entries(&self, slots: Vec<u32>) {
        self.with(|entries| entries.slots = slots);
    }

    /// Each entry, free ones (0) too, replaced by what `update` makes of
    /// it (a GC clears a weak root's whose object died), the entries out
    /// of the table meanwhile.
    pub(crate) fn update_entries(&self, mut update: impl FnMut(u32) -> u32) {
        let mut slots = self.take_entries();
        for slot in &mut slots {
            *slot = update(*slot);
        }
        self.put_entries(slots);
    }

    /// Entries in use.
    pub(crate) fn len(&self) -> usize {
        self.with(|entries| entries.slots.len().saturating_sub(entries.free.len()))
    }
}
