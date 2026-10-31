//! What the heap measures of the old generation's fragmentation once a
//! sweep is done (where `HeapOptions` asks), and its allocation's free
//! ranges: whether compaction would pay is decided from them.

use std::cell::Cell;

use elise::{Fragmentation, Gc, GcObject, Heap, HeapOptions, Member, Persistent};

const PAGE_BYTES: usize = 256 << 10;

/// About 1 KiB: a page holds a couple of hundred.
#[derive(GcObject)]
struct Mid {
    next: Member<Mid>,
    words: [Cell<u64>; 120],
}

fn mid<'gc>(cx: &elise::Mutation<'gc>, next: Option<Gc<'gc, Mid>>) -> Gc<'gc, Mid> {
    cx.alloc(MidInit {
        next,
        words: std::array::from_fn(|_| Cell::new(0)),
    })
}

/// The old generation alone (no nursery), measured.
fn measured_heap() -> Heap {
    Heap::new(HeapOptions {
        nursery_bytes: 0,
        measure_fragmentation: true,
        ..crate::verified()
    })
}

/// The bytes of `Mid`'s slots, and how many a page holds.
fn mid_class(heap: &mut Heap) -> (usize, usize) {
    let before = heap.stats().used_bytes;
    heap.mutate(|cx| {
        mid(cx, None);
    });
    let slot = heap.stats().used_bytes - before;
    (slot, PAGE_BYTES / slot)
}

/// `count` objects, one in `kept_one_in` kept (the first of each run), the
/// rest garbage between them; then a full GC, its sweep done in its pause.
fn spaced(heap: &mut Heap, count: usize, kept_one_in: usize) -> Persistent<Mid> {
    let kept = heap.mutate(|cx| {
        let mut kept = None;
        for i in 0..count {
            if i % kept_one_in == 0 {
                kept = Some(mid(cx, kept));
            } else {
                mid(cx, None);
            }
        }
        Persistent::new(cx, kept.unwrap())
    });
    heap.collect();
    kept
}

/// The pages' slots, their free ones and what is live add up: the slots in
/// use are the heap's used bytes, with no large object.
fn check_accounts(heap: &Heap, pages: usize, area: usize) -> Fragmentation {
    let stats = heap.stats();
    let measured = stats.fragmentation;
    assert_eq!(measured.pages, pages);
    assert_eq!(measured.slot_bytes, pages * area);
    assert_eq!(measured.slot_bytes - measured.free_bytes, stats.used_bytes);
    assert!(measured.old_committed_bytes >= pages * PAGE_BYTES);
    measured
}

/// Four pages with a tenth of their slots live: V8 would evacuate them all,
/// its objects fitting on one, three pages freed.
#[test]
fn mostly_free_pages_would_be_evacuated() {
    let mut heap = measured_heap();
    let (slot, slots) = mid_class(&mut heap);
    heap.collect();
    let _kept = spaced(&mut heap, 4 * slots, 10);
    let measured = check_accounts(&heap, 4, slots * slot);
    assert_eq!(measured.candidates, 4);
    assert_eq!(measured.evacuated_bytes, heap.stats().used_bytes);
    assert_eq!(measured.released_pages, 3);
    assert_eq!(measured.released_bytes, 3 * PAGE_BYTES);
}

/// Pages half live are too full for V8 to take; pages all live, too.
#[test]
fn fuller_pages_would_stay() {
    for kept_one_in in [2, 1] {
        let mut heap = measured_heap();
        let (slot, slots) = mid_class(&mut heap);
        heap.collect();
        let _kept = spaced(&mut heap, 4 * slots, kept_one_in);
        let measured = check_accounts(&heap, 4, slots * slot);
        assert_eq!(
            (
                measured.candidates,
                measured.evacuated_bytes,
                measured.released_pages
            ),
            (0, 0, 0)
        );
    }
}

/// Each sweep done is measured once; the heap measures nothing unless
/// asked.
#[test]
fn each_sweep_is_measured_where_asked() {
    let mut heap = measured_heap();
    assert_eq!(heap.stats().fragmentation, Fragmentation::default());
    heap.collect();
    heap.collect();
    assert_eq!(heap.stats().fragmentation.count, 2);
    assert_eq!(heap.stats().fragmentation_at_marking.count, 2);
    let mut unmeasured = Heap::new(HeapOptions {
        nursery_bytes: 0,
        ..crate::verified()
    });
    let _kept = spaced(&mut unmeasured, 1000, 10);
    assert_eq!(unmeasured.stats().fragmentation, Fragmentation::default());
    assert_eq!(
        unmeasured.stats().fragmentation_at_marking,
        Fragmentation::default()
    );
}

/// When a marking starts (where V8 chooses), the slots allocated since the
/// sweep are in use: pages the sweep left mostly free, filled since, are
/// none to evacuate; the fill garbage, the next sweep finds them as free.
#[test]
fn a_marking_measures_the_slots_filled_since() {
    let mut heap = measured_heap();
    let (slot, slots) = mid_class(&mut heap);
    heap.collect();
    let _kept = spaced(&mut heap, 4 * slots, 10);
    assert_eq!(heap.stats().fragmentation.released_pages, 3);
    let free = heap.stats().fragmentation.free_bytes / slot;
    heap.mutate(|cx| {
        for _ in 0..free {
            mid(cx, None);
        }
    });
    let used = heap.stats().used_bytes;
    heap.collect();
    let at_marking = heap.stats().fragmentation_at_marking;
    assert_eq!(at_marking.count, 3);
    assert_eq!(at_marking.pages, 4);
    assert_eq!(at_marking.free_bytes, 0);
    assert_eq!(at_marking.slot_bytes, used);
    assert_eq!(at_marking.candidates, 0);
    assert_eq!(heap.stats().fragmentation.released_pages, 3);
}

/// On pages where every other slot is live, allocation takes a range of
/// one slot each time; on a new page, one range of all its slots.
#[test]
fn allocation_counts_its_ranges() {
    let mut heap = measured_heap();
    let (_, slots) = mid_class(&mut heap);
    heap.collect();
    let _kept = spaced(&mut heap, 4 * slots, 2);
    let before = heap.stats();
    heap.mutate(|cx| {
        for _ in 0..100 {
            mid(cx, None);
        }
    });
    let after = heap.stats();
    assert_eq!(after.old_ranges - before.old_ranges, 100);
    assert_eq!(after.old_range_slots - before.old_range_slots, 100);

    let mut fresh = measured_heap();
    let before = fresh.stats();
    fresh.mutate(|cx| {
        for _ in 0..100 {
            mid(cx, None);
        }
    });
    let after = fresh.stats();
    assert_eq!(after.old_ranges - before.old_ranges, 1);
    assert_eq!(
        (after.old_range_slots - before.old_range_slots) as usize,
        slots
    );
}
