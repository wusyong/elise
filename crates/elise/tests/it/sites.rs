//! Allocation sites: a site whose objects survive their first scavenge is
//! tenured once the nursery is large, one whose objects die isn't, each
//! site on its own; a tenured site allocates in the old generation, until a
//! full GC finds its old objects mostly dead; `pretenured` comes first; an
//! object keeps its site through scavenges and promotion. The heaps scavenge
//! and mark on the process's pool, so the threads' counts add up; every
//! heap runs the verifier, which checks each object's site.

use std::cell::Cell;

use elise::{
    GcObject, Heap, HeapOptions, HeapVector, Mutation, Persistent, PretenureDecision, Site,
};

/// 64 bytes with its header.
#[derive(GcObject)]
struct Leaf {
    words: [Cell<u64>; 7],
}

const PAGE_BYTES: usize = 256 << 10;

/// A heap whose nursery starts at `pages` and grows up to `max_pages`.
fn heap(pages: usize, max_pages: usize) -> Heap {
    Heap::new(HeapOptions {
        nursery_bytes: pages * PAGE_BYTES,
        max_nursery_bytes: max_pages * PAGE_BYTES,
        ..crate::verified()
    })
}

fn leaf<'gc>(cx: &Mutation<'gc>, site: &Site) -> elise::Gc<'gc, Leaf> {
    cx.alloc_in(
        site,
        LeafInit {
            words: std::array::from_fn(|_| Cell::new(0)),
        },
    )
}

/// `count` objects of `site`, kept.
fn keep(heap: &mut Heap, site: &Site, count: usize) -> Persistent<HeapVector<Leaf>> {
    heap.mutate(|cx| {
        let list = HeapVector::new(cx, count);
        for _ in 0..count {
            list.push(cx, leaf(cx, site));
        }
        Persistent::new(cx, list)
    })
}

/// `count` objects of `site`, garbage at once.
fn drop_new(heap: &mut Heap, site: &Site, count: usize) {
    heap.mutate(|cx| {
        for _ in 0..count {
            leaf(cx, site);
        }
    });
}

/// Whether an object of `site` allocated now goes to the nursery.
fn goes_young(heap: &mut Heap, site: &Site) -> bool {
    let before = heap.stats().young_bytes;
    drop_new(heap, site, 1);
    heap.stats().young_bytes > before
}

#[test]
fn a_site_is_tenured_if_its_objects_survive() {
    static KEEP: Site = Site::new();
    static DROP: Site = Site::new();
    // A nursery at its largest: a site is decided at once.
    let mut heap = heap(1, 1);
    let kept = keep(&mut heap, &KEEP, 200);
    drop_new(&mut heap, &DROP, 200);
    assert_eq!(heap.site(&KEEP).created, 200);
    assert_eq!(heap.site(&DROP).created, 200);
    heap.scavenge();
    assert_eq!(heap.site(&KEEP).decision, PretenureDecision::Tenure);
    assert_eq!(heap.site(&DROP).decision, PretenureDecision::DontTenure);
    assert_eq!(heap.site(&KEEP).created, 0, "the counts start over");
    assert!(!goes_young(&mut heap, &KEEP));
    assert_eq!(heap.site(&KEEP).entered_old, 1);
    assert_eq!(heap.stats().pretenured_objects, 1);
    assert!(goes_young(&mut heap, &DROP));
    assert_eq!(heap.stats().pretenured_objects, 1);
    drop(kept);
}

#[test]
fn a_small_nursery_waits_to_tenure() {
    static SITE: Site = Site::new();
    let mut heap = heap(1, 2);
    // 3000 objects, 192 KiB: they survive, but the nursery is a page.
    let first = keep(&mut heap, &SITE, 3000);
    heap.scavenge();
    assert_eq!(heap.site(&SITE).decision, PretenureDecision::MaybeTenure);
    assert_eq!(heap.stats().nursery_bytes, PAGE_BYTES);
    assert!(goes_young(&mut heap, &SITE));
    // As many again: with the first, promoted, more than the nursery
    // survived, so it doubles to its largest, and the site is tenured.
    let second = keep(&mut heap, &SITE, 3000);
    heap.scavenge();
    assert_eq!(heap.stats().nursery_bytes, 2 * PAGE_BYTES);
    assert_eq!(heap.site(&SITE).decision, PretenureDecision::Tenure);
    assert!(!goes_young(&mut heap, &SITE));
    drop((first, second));
}

/// What survives a scavenge counts for its site the first time only: a
/// site whose old objects live on but whose new ones die isn't tenured.
#[test]
fn only_a_first_survival_counts() {
    static SITE: Site = Site::new();
    let mut heap = heap(1, 2);
    let kept = keep(&mut heap, &SITE, 200);
    heap.scavenge();
    assert_eq!(heap.site(&SITE).decision, PretenureDecision::MaybeTenure);
    // 200 new ones die; the first 200, aged, are promoted.
    drop_new(&mut heap, &SITE, 200);
    heap.scavenge();
    assert_eq!(heap.site(&SITE).decision, PretenureDecision::DontTenure);
    drop(kept);
}

#[test]
fn a_full_gc_takes_back_a_site_whose_old_objects_died() {
    static SITE: Site = Site::new();
    let mut heap = heap(1, 1);
    let kept = keep(&mut heap, &SITE, 200);
    heap.scavenge();
    assert_eq!(heap.site(&SITE).decision, PretenureDecision::Tenure);
    // 5000 more, old, dead: the full GC marks the 200 (promoted by it) of
    // 5200, under a tenth.
    drop_new(&mut heap, &SITE, 5000);
    assert_eq!(heap.site(&SITE).entered_old, 5000);
    heap.collect();
    assert_eq!(heap.site(&SITE).marked, 200);
    assert_eq!(heap.site(&SITE).entered_old, 0);
    assert_eq!(heap.site(&SITE).decision, PretenureDecision::Undecided);
    assert!(goes_young(&mut heap, &SITE));
    drop(kept);
}

#[test]
fn a_full_gc_keeps_a_site_whose_old_objects_live() {
    static SITE: Site = Site::new();
    let mut heap = heap(1, 1);
    let first = keep(&mut heap, &SITE, 200);
    heap.scavenge();
    // 4800 more, old, dead, and 400 kept: 600 of 5400 marked.
    drop_new(&mut heap, &SITE, 4800);
    let second = keep(&mut heap, &SITE, 400);
    heap.collect();
    assert_eq!(heap.site(&SITE).marked, 600);
    assert_eq!(heap.site(&SITE).decision, PretenureDecision::Tenure);
    // The next counts from what it marked: 600 and 6000 dead, under a tenth.
    drop((first, second));
    drop_new(&mut heap, &SITE, 6000);
    heap.collect();
    assert_eq!(heap.site(&SITE).marked, 0);
    assert_eq!(heap.site(&SITE).decision, PretenureDecision::Undecided);
}

#[test]
fn pretenured_comes_before_a_site() {
    static SITE: Site = Site::new();
    let mut heap = heap(1, 1);
    let before = heap.stats().young_bytes;
    heap.mutate(|cx| {
        cx.pretenured(|cx| leaf(cx, &SITE));
    });
    assert_eq!(heap.stats().young_bytes, before);
    assert_eq!(heap.site(&SITE).entered_old, 1);
    assert_eq!(heap.site(&SITE).created, 0);
    // Not for its site, which was never tenured.
    assert_eq!(heap.stats().pretenured_objects, 0);
}

#[test]
fn an_object_keeps_its_site_through_scavenges_and_promotion() {
    static SITE: Site = Site::new();
    static OTHER: Site = Site::new();
    let mut heap = Heap::new(crate::verified());
    let kept = keep(&mut heap, &SITE, 150);
    let other = keep(&mut heap, &OTHER, 30);
    // Copied in the nursery, then promoted, then marked: each counted for
    // its own site.
    heap.scavenge();
    heap.scavenge();
    assert_eq!(heap.site(&SITE).entered_old, 150);
    assert_eq!(heap.site(&OTHER).entered_old, 30);
    heap.collect();
    assert_eq!(heap.site(&SITE).marked, 150);
    assert_eq!(heap.site(&OTHER).marked, 30);
    assert_eq!(heap.stats().last_gc.marked_objects, 150 + 30 + 4);
    drop((kept, other));
}
