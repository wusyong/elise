//! The old generation's fragmentation (V8's `--trace-fragmentation`),
//! measured where `HeapOptions` asks, once a sweep is done (the most
//! fragmented it gets) and when a marking starts (where V8 chooses the
//! pages to evacuate, allocation having filled some free slots since):
//! the free slots on the size-class pages with objects, and what V8's
//! choice of pages to evacuate would move and free there. Whether
//! compaction would pay is decided from it; nothing here moves an object.

use std::cell::Cell;

use super::{Inner, PageState};
use crate::page::{self, PAGE_BYTES};

/// V8's choice of pages to evacuate in a regular full GC
/// (`MarkCompactCollector::CollectEvacuationCandidates`, before it has
/// measured its compaction's speed): a page at least this much free
/// (`kTargetFragmentationPercent`)...
const TARGET_FREE_PERCENT: usize = 70;
/// ...the least live first, as long as the live bytes moved stay within
/// this (`compaction_max_evacuated_bytes_mb`).
const MAX_EVACUATED_BYTES: usize = 4 << 20;

/// The old generation's size-class pages at a point a GC measures them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fragmentation {
    /// How many times the heap measured at this point, this one included.
    pub count: u64,
    /// The old generation's committed bytes (the nursery's pages aside):
    /// pages in use, in the pool, and large objects'.
    pub old_committed_bytes: usize,
    /// The size-class pages with objects, the bytes of their slots (a
    /// page's tail too short for a slot aside), and of those, the free
    /// slots' (allocated in since the GC or not).
    pub pages: usize,
    pub slot_bytes: usize,
    pub free_bytes: usize,
    /// V8's choice of those pages: the ones it would evacuate, their
    /// objects' bytes, and the pages freed once the objects are packed on
    /// as few pages of their classes as fit them, and those pages' bytes.
    pub candidates: usize,
    pub evacuated_bytes: usize,
    pub released_pages: usize,
    pub released_bytes: usize,
}

/// A size class's slots on a page: its area, in V8's words.
fn area(class: usize) -> usize {
    let slot = page::CLASSES[class];
    PAGE_BYTES
        .checked_div(slot)
        .map_or(0, |slots| slots.saturating_mul(slot))
}

/// V8's choice of `pages` (each one's live bytes and class) to evacuate:
/// those at least `TARGET_FREE_PERCENT` free, the least live first while
/// the bytes moved stay within `MAX_EVACUATED_BYTES`; none if moving them
/// frees no page (V8 avoids compacting to grow again). Their count, the
/// bytes they hold, and the pages freed.
fn choose(pages: &[(usize, usize)]) -> (usize, usize, usize) {
    let mut candidates: Vec<(usize, usize)> = pages
        .iter()
        .copied()
        .filter(|&(live, class)| {
            let area = area(class);
            area.saturating_sub(live).saturating_mul(100)
                >= area.saturating_mul(TARGET_FREE_PERCENT)
        })
        .collect();
    candidates.sort_unstable_by_key(|&(live, _)| live);
    let mut by_class = vec![0usize; page::CLASSES.len()];
    let (mut count, mut evacuated) = (0usize, 0usize);
    for &(live, class) in &candidates {
        let total = evacuated.saturating_add(live);
        if total > MAX_EVACUATED_BYTES {
            break;
        }
        evacuated = total;
        count = count.saturating_add(1);
        by_class[class] = by_class[class].saturating_add(live);
    }
    let needed = by_class
        .iter()
        .enumerate()
        .map(|(class, &live)| live.div_ceil(area(class).max(1)))
        .fold(0usize, usize::saturating_add);
    let released = count.saturating_sub(needed);
    if released == 0 {
        return (0, 0, 0);
    }
    (count, evacuated, released)
}

impl Inner {
    /// Measures the old generation's pages, the sweep done, into `last`.
    pub(super) fn measure_fragmentation(&self, last: &Cell<Fragmentation>) {
        let space = self.space.borrow();
        let mut measured = Fragmentation {
            count: last.get().count.saturating_add(1),
            ..Fragmentation::default()
        };
        let mut pages = Vec::new();
        for (number, state) in space.states.iter().enumerate() {
            if !matches!(state, PageState::Young { .. }) {
                measured.old_committed_bytes = measured
                    .old_committed_bytes
                    .saturating_add(space.committed[number]);
            }
            let PageState::Small(page) = state else {
                continue;
            };
            let mut free = page
                .ranges
                .iter()
                .map(|&(first, end)| end.saturating_sub(first) as usize)
                .fold(0usize, usize::saturating_add);
            let cursor = self.cursors[page.class].get();
            if cursor.page == number {
                free = free.saturating_add(
                    cursor
                        .limit
                        .saturating_sub(cursor.next)
                        .checked_div(page.slot_bytes)
                        .unwrap_or(0),
                );
            }
            let area = page.slots.saturating_mul(page.slot_bytes);
            let free = free.saturating_mul(page.slot_bytes);
            measured.pages = measured.pages.saturating_add(1);
            measured.slot_bytes = measured.slot_bytes.saturating_add(area);
            measured.free_bytes = measured.free_bytes.saturating_add(free);
            pages.push((area.saturating_sub(free), page.class));
        }
        (
            measured.candidates,
            measured.evacuated_bytes,
            measured.released_pages,
        ) = choose(&pages);
        measured.released_bytes = measured.released_pages.saturating_mul(PAGE_BYTES);
        last.set(measured);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The class of 64-byte slots: 4096 to a page.
    fn class_64() -> usize {
        page::CLASSES
            .iter()
            .position(|&bytes| bytes == 64)
            .expect("a class of 64 bytes")
    }

    #[test]
    fn v8_takes_the_pages_mostly_free_the_least_live_first() {
        let class = class_64();
        let area = area(class);
        assert_eq!(area, PAGE_BYTES);
        // 10% live, 30% live (70% free: just in), 31% live (out), 5% live.
        let pages = [
            (area / 10, class),
            (area * 3 / 10, class),
            (area * 31 / 100, class),
            (area / 20, class),
        ];
        let live = area / 10 + area * 3 / 10 + area / 20;
        // Three pages' objects fit on one: two freed.
        assert_eq!(choose(&pages), (3, live, 2));
    }

    #[test]
    fn v8_moves_4_mib_at_most() {
        let class = class_64();
        let area = area(class);
        // 80 pages, each a quarter live (64 KiB): 4 MiB is 64 of them.
        let pages = vec![(area / 4, class); 80];
        let (count, evacuated, released) = choose(&pages);
        assert_eq!(count, 64);
        assert_eq!(evacuated, MAX_EVACUATED_BYTES);
        assert_eq!(released, 64 - 16);
    }

    #[test]
    fn the_least_live_go_first_under_the_limit() {
        let class = class_64();
        let area = area(class);
        // 70 pages a quarter live, 10 pages a hundredth live: the ten
        // first, then as many of the others as fit in 4 MiB (63).
        let mut pages = vec![(area / 4, class); 70];
        pages.extend(vec![(area / 100, class); 10]);
        let (count, evacuated, _) = choose(&pages);
        assert_eq!(count, 73);
        assert_eq!(evacuated, 10 * (area / 100) + 63 * (area / 4));
    }

    #[test]
    fn v8_moves_nothing_when_no_page_is_freed() {
        let class = class_64();
        // One page a fifth live: its objects need a page of their own.
        assert_eq!(choose(&[(area(class) / 5, class)]), (0, 0, 0));
        // None free enough.
        assert_eq!(choose(&[(area(class) / 2, class); 4]), (0, 0, 0));
    }

    #[test]
    fn each_class_packs_on_its_own_pages() {
        let class = class_64();
        let other = class + 1;
        // Two pages of each class, each 20% live: per class, two into one.
        let pages = [
            (area(class) / 5, class),
            (area(class) / 5, class),
            (area(other) / 5, other),
            (area(other) / 5, other),
        ];
        assert_eq!(choose(&pages).2, 2);
    }
}
