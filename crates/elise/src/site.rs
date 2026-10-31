//! Allocation sites (V8's `AllocationSite`): a place that allocates, which
//! the heap watches to pretenure. Its objects carry its number in their
//! header's high 16 bits of flags (V8 puts an `AllocationMemento` after a
//! young object instead: it has no room in its header). Each heap counts,
//! for each site, what it allocated in the nursery and how much of it
//! survived its first scavenge, and allocates straight in the old
//! generation from a site whose objects mostly do, once the nursery is
//! large (V8's `PretenuringHandler`); a full GC that finds the site's old
//! objects mostly dead takes that back.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, PoisonError};

/// A place that allocates, declared where it does (`static SITE: Site =
/// Site::new();`, then `Mutation::alloc_in(&SITE, ..)`), or made at run time
/// and leaked (`Box::leak`) for one that isn't a place in the code (a
/// replayed recording's type). Every heap of the process can use it; each
/// counts its own.
pub struct Site {
    /// Its number, 0 until its first allocation; `EXHAUSTED` if the process
    /// had `MAX_SITES` before it.
    index: AtomicU32,
}

/// How many sites a process may have: their numbers fit 16 bits of a
/// header's flags, 0 for none.
pub const MAX_SITES: u32 = u16::MAX as u32;

const EXHAUSTED: u32 = u32::MAX;

/// Where a header's flags hold its site's number.
pub(crate) const SITE_SHIFT: u32 = 16;

impl Site {
    #[expect(
        clippy::new_without_default,
        reason = "made in statics: a const fn, as TypeSlot's"
    )]
    pub const fn new() -> Self {
        Self {
            index: AtomicU32::new(0),
        }
    }

    /// Its number, numbering it on first use: 0 (no site, never pretenured)
    /// once the process has `MAX_SITES`.
    pub(crate) fn index(&self) -> u32 {
        match self.index.load(Ordering::Acquire) {
            0 => register(self, &NEXT_INDEX),
            EXHAUSTED => 0,
            index => index,
        }
    }
}

/// The next number, under the lock that numbering takes.
static NEXT_INDEX: Mutex<u32> = Mutex::new(1);

/// Numbers `site` from `next` (the process's, but in a test).
fn register(site: &Site, next: &Mutex<u32>) -> u32 {
    let mut next = next.lock().unwrap_or_else(PoisonError::into_inner);
    // Another thread may have numbered it while this one waited.
    match site.index.load(Ordering::Acquire) {
        0 => {}
        EXHAUSTED => return 0,
        index => return index,
    }
    if *next > MAX_SITES {
        site.index.store(EXHAUSTED, Ordering::Release);
        return 0;
    }
    let index = *next;
    site.index.store(index, Ordering::Release);
    #[expect(
        clippy::arithmetic_side_effects,
        reason = "index <= MAX_SITES (checked above), far below u32::MAX"
    )]
    {
        *next = index + 1;
    }
    index
}

/// What a heap decided of a site (V8's `AllocationSite::PretenureDecision`,
/// but its zombie state: a site here is never freed).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PretenureDecision {
    /// Too few of its objects seen yet, or it was taken back.
    #[default]
    Undecided,
    /// Fewer than `TENURE_PERCENT` of its objects survived a scavenge: it
    /// stays so.
    DontTenure,
    /// Enough survived, but the nursery was small: tenured once enough
    /// still do in a large one.
    MaybeTenure,
    /// Its objects are allocated in the old generation.
    Tenure,
}

/// What a heap knows of a site.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SiteStats {
    pub decision: PretenureDecision,
    /// Its objects in the nursery since the last scavenge (V8's mementos
    /// created).
    pub created: usize,
    /// Its objects that went to the old generation since the last full GC:
    /// allocated there or promoted.
    pub entered_old: usize,
    /// Its objects the last full GC marked.
    pub marked: usize,
}

/// A site is decided once a scavenge saw this many of its objects (V8's
/// `kMinMementoCount`).
const MIN_CREATED: usize = 100;
/// A site is tenured once this share of its objects survives their first
/// scavenge (V8's `kScavengerPretenureRatio`).
const TENURE_PERCENT: usize = 80;
/// ... with a nursery at least this large, or its largest (V8's
/// `kDefaultMinNewSpaceCapacityForPretenuring`): a small one scavenges so
/// often that objects can't die first.
pub(crate) const MIN_NURSERY_BYTES_TO_TENURE: usize = 8 << 20;
/// A tenured site is taken back once a full GC marks fewer than this share
/// of its old objects (V8's `kOldSurvivalRateLowThreshold`, which V8 checks
/// of the whole old generation, its mementos gone with promotion).
const KEPT_PERCENT: usize = 10;

/// A heap's sites, by number.
#[derive(Default)]
pub(crate) struct Sites {
    stats: Vec<SiteStats>,
}

/// What a scavenge's thread counted of the sites, by number.
#[derive(Default)]
pub(crate) struct ScavengeCounts {
    /// Objects that survived their first scavenge (V8's mementos found).
    pub(crate) found: Vec<usize>,
    pub(crate) promoted: Vec<usize>,
}

impl ScavengeCounts {
    pub(crate) fn new(sites: usize) -> Self {
        Self {
            found: vec![0; sites],
            promoted: vec![0; sites],
        }
    }

    /// An object of `site` copied: for the first time if `first`, to the
    /// old generation if `promoted`.
    pub(crate) fn copied(&mut self, site: u32, first: bool, promoted: bool) {
        let site = site as usize;
        if first && let Some(count) = self.found.get_mut(site) {
            *count = count.saturating_add(1);
        }
        if promoted && let Some(count) = self.promoted.get_mut(site) {
            *count = count.saturating_add(1);
        }
    }
}

/// Adds `counts` into `into`, site by site.
pub(crate) fn add_counts(into: &mut [usize], counts: &[usize]) {
    for (into, count) in into.iter_mut().zip(counts) {
        *into = into.saturating_add(*count);
    }
}

impl Sites {
    /// The number past the highest site the heap allocated from: the
    /// length of a thread's counts.
    pub(crate) fn len(&self) -> usize {
        self.stats.len()
    }

    pub(crate) fn stats(&self, site: u32) -> SiteStats {
        self.stats.get(site as usize).copied().unwrap_or_default()
    }

    fn stats_mut(&mut self, site: u32) -> &mut SiteStats {
        let at = site as usize;
        if self.stats.len() <= at {
            self.stats
                .resize(at.saturating_add(1), SiteStats::default());
        }
        &mut self.stats[at]
    }

    pub(crate) fn is_tenured(&self, site: u32) -> bool {
        self.stats(site).decision == PretenureDecision::Tenure
    }

    /// An object of `site` allocated in the nursery.
    pub(crate) fn created(&mut self, site: u32) {
        let stats = self.stats_mut(site);
        stats.created = stats.created.saturating_add(1);
    }

    /// An object of `site` allocated in the old generation.
    pub(crate) fn entered_old(&mut self, site: u32) {
        let stats = self.stats_mut(site);
        stats.entered_old = stats.entered_old.saturating_add(1);
    }

    /// A scavenge is done, its threads' counts added up (V8's
    /// `ProcessPretenuringFeedback`): each site with enough objects seen
    /// is decided, if it may still be, and the counts start over.
    /// `large_nursery`: the nursery is at least `MIN_NURSERY_BYTES_TO_TENURE`
    /// or its largest.
    pub(crate) fn scavenged(&mut self, counts: &ScavengeCounts, large_nursery: bool) {
        for (at, stats) in self.stats.iter_mut().enumerate() {
            let found = counts.found.get(at).copied().unwrap_or(0);
            let promoted = counts.promoted.get(at).copied().unwrap_or(0);
            stats.entered_old = stats.entered_old.saturating_add(promoted);
            let open = matches!(
                stats.decision,
                PretenureDecision::Undecided | PretenureDecision::MaybeTenure
            );
            if stats.created >= MIN_CREATED && open {
                stats.decision =
                    if found.saturating_mul(100) < stats.created.saturating_mul(TENURE_PERCENT) {
                        PretenureDecision::DontTenure
                    } else if large_nursery {
                        PretenureDecision::Tenure
                    } else {
                        PretenureDecision::MaybeTenure
                    };
            }
            stats.created = 0;
        }
    }

    /// A full GC marked `marked` of each site's objects: a tenured site
    /// whose old objects mostly died is undecided again, and the counts
    /// start over.
    pub(crate) fn collected(&mut self, marked: &[usize]) {
        for (at, stats) in self.stats.iter_mut().enumerate() {
            let now = marked.get(at).copied().unwrap_or(0);
            let old = stats.marked.saturating_add(stats.entered_old);
            if stats.decision == PretenureDecision::Tenure
                && now.saturating_mul(100) < old.saturating_mul(KEPT_PERCENT)
            {
                stats.decision = PretenureDecision::Undecided;
            }
            stats.marked = now;
            stats.entered_old = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A heap's sites with one, number 1, that saw `created` objects.
    fn one_site(created: usize) -> Sites {
        let mut sites = Sites::default();
        for _ in 0..created {
            sites.created(1);
        }
        sites
    }

    fn found(at_1: usize) -> ScavengeCounts {
        let mut counts = ScavengeCounts::new(2);
        counts.found[1] = at_1;
        counts
    }

    #[test]
    fn a_site_is_decided_by_what_survives_its_first_scavenge() {
        // 80 of 100: tenured, in a large nursery.
        let mut sites = one_site(100);
        sites.scavenged(&found(80), true);
        assert_eq!(sites.stats(1).decision, PretenureDecision::Tenure);
        assert_eq!(sites.stats(1).created, 0, "the counts start over");
        // 79 of 100: not, and it stays so.
        let mut sites = one_site(100);
        sites.scavenged(&found(79), true);
        assert_eq!(sites.stats(1).decision, PretenureDecision::DontTenure);
        for _ in 0..100 {
            sites.created(1);
        }
        sites.scavenged(&found(100), true);
        assert_eq!(sites.stats(1).decision, PretenureDecision::DontTenure);
    }

    #[test]
    fn too_few_objects_decide_nothing() {
        let mut sites = one_site(99);
        sites.scavenged(&found(99), true);
        assert_eq!(sites.stats(1).decision, PretenureDecision::Undecided);
        // The 99 don't count towards the next.
        sites.created(1);
        sites.scavenged(&found(1), true);
        assert_eq!(sites.stats(1).decision, PretenureDecision::Undecided);
    }

    #[test]
    fn a_small_nursery_waits_to_tenure() {
        let mut sites = one_site(100);
        sites.scavenged(&found(100), false);
        assert_eq!(sites.stats(1).decision, PretenureDecision::MaybeTenure);
        for _ in 0..100 {
            sites.created(1);
        }
        sites.scavenged(&found(90), true);
        assert_eq!(sites.stats(1).decision, PretenureDecision::Tenure);
        // A maybe whose objects then die is not tenured.
        let mut sites = one_site(100);
        sites.scavenged(&found(100), false);
        for _ in 0..100 {
            sites.created(1);
        }
        sites.scavenged(&found(10), true);
        assert_eq!(sites.stats(1).decision, PretenureDecision::DontTenure);
    }

    #[test]
    fn a_full_gc_takes_back_a_site_whose_old_objects_died() {
        let mut sites = one_site(100);
        sites.scavenged(&found(100), true);
        // 1000 entered the old generation; 100 marked: kept (10%).
        for _ in 0..1000 {
            sites.entered_old(1);
        }
        sites.collected(&[0, 100]);
        assert_eq!(sites.stats(1).decision, PretenureDecision::Tenure);
        assert_eq!(sites.stats(1).marked, 100);
        assert_eq!(sites.stats(1).entered_old, 0);
        // The 100 and 900 more; 99 marked: under 10% of 1000.
        for _ in 0..900 {
            sites.entered_old(1);
        }
        sites.collected(&[0, 99]);
        assert_eq!(sites.stats(1).decision, PretenureDecision::Undecided);
    }

    #[test]
    fn promoted_objects_entered_the_old_generation() {
        let mut sites = one_site(1);
        let mut counts = ScavengeCounts::new(2);
        counts.copied(1, false, true);
        counts.copied(1, true, false);
        sites.scavenged(&counts, false);
        assert_eq!(sites.stats(1).entered_old, 1);
    }

    #[test]
    fn sites_are_numbered_once() {
        static SITE: Site = Site::new();
        let index = SITE.index();
        assert!((1..=MAX_SITES).contains(&index));
        assert_eq!(SITE.index(), index);
        assert_ne!(Site::new().index(), index);
    }

    #[test]
    fn past_the_last_number_a_site_has_none() {
        let next = Mutex::new(MAX_SITES);
        let (last, past) = (Site::new(), Site::new());
        assert_eq!(register(&last, &next), MAX_SITES);
        assert_eq!(register(&past, &next), 0);
        assert_eq!(past.index(), 0, "and keeps none");
        assert_eq!(last.index(), MAX_SITES);
    }
}
