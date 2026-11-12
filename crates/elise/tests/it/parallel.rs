//! Marking on a platform's workers: it keeps and frees what marking on one
//! thread does, workers help with a wide graph, and a narrow one stays on
//! the heap's thread; a worker's runs are phases of the platform's trace.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::thread::ThreadId;

use elise::{
    Gc, GcObject, Heap, HeapVector, InlinePlatform, LocalExternal, Member, Mutation, Persistent,
    Platform, ThreadPool,
};

type Log = Rc<RefCell<Vec<u32>>>;

/// Logs its node's id when finalized.
struct Finalized(u32, Log);

impl Drop for Finalized {
    fn drop(&mut self) {
        self.1.borrow_mut().push(self.0);
    }
}

#[derive(GcObject)]
struct Branch {
    kids: Member<HeapVector<Branch>>,
    id: Cell<u32>,
    finalized: LocalExternal<Finalized>,
}

#[derive(GcObject)]
struct Link {
    next: Member<Link>,
}

/// A tree of `count` branches, 8 children each, built breadth first; every
/// third child of the root is cut off (garbage) unless `keep`. Returns the
/// root and the ids that are garbage.
fn tree<'gc>(cx: &Mutation<'gc>, count: u32, log: &Log) -> (Gc<'gc, Branch>, Vec<u32>) {
    let branch = |id: u32| {
        cx.alloc(BranchInit {
            kids: Some(HeapVector::new(cx, 8)),
            id: Cell::new(id),
            finalized: LocalExternal::new(Finalized(id, log.clone())),
        })
    };
    let mut all = vec![branch(0)];
    for id in 1..count {
        let parent = all[((id - 1) / 8) as usize];
        let child = branch(id);
        parent.kids.get(cx).unwrap().push(cx, child);
        all.push(child);
    }
    // Cut every third child of the root: it and its subtree are garbage.
    let root = all[0];
    let kids = root.kids.get(cx).unwrap();
    let mut garbage = Vec::new();
    let mut i = 0;
    while i < kids.len() {
        if kids.get(cx, i).unwrap().id.get() % 3 == 0 {
            let mut stack = vec![kids.remove(cx, i)];
            while let Some(branch) = stack.pop() {
                garbage.push(branch.id.get());
                let kids = branch.kids.get(cx).unwrap();
                stack.extend((0..kids.len()).map(|i| kids.get(cx, i).unwrap()));
            }
        } else {
            i += 1;
        }
    }
    garbage.sort_unstable();
    (root, garbage)
}

fn count<'gc>(cx: &Mutation<'gc>, root: Gc<'gc, Branch>) -> usize {
    let mut stack = vec![root];
    let mut count = 0;
    while let Some(branch) = stack.pop() {
        count += 1;
        let kids = branch.kids.get(cx).unwrap();
        stack.extend((0..kids.len()).map(|i| kids.get(cx, i).unwrap()));
    }
    count
}

const BRANCHES: u32 = if cfg!(miri) { 200 } else { 50_000 };

/// A GC on `platform` of a tree with garbage: what it finalized, the live
/// branches it kept, and the objects its workers scanned.
fn collect_tree(platform: Arc<dyn Platform>) -> (Vec<u32>, Vec<u32>, usize, usize) {
    let log = Log::default();
    let mut heap = Heap::with_platform(crate::verified(), platform);
    let (root, garbage) = heap.mutate(|cx| {
        let (root, garbage) = tree(cx, BRANCHES, &log);
        (Persistent::new(cx, root), garbage)
    });
    heap.collect();
    let mut finalized = log.borrow().clone();
    finalized.sort_unstable();
    let live = heap.mutate(|cx| count(cx, root.get(cx)));
    (
        finalized,
        garbage,
        live,
        heap.stats().last_gc.scanned_by_workers,
    )
}

#[test]
fn parallel_marking_keeps_what_one_thread_keeps() {
    let (inline, garbage, live, by_workers) = collect_tree(Arc::new(InlinePlatform));
    assert_eq!(inline, garbage);
    assert_eq!(live + garbage.len(), BRANCHES as usize);
    assert_eq!(by_workers, 0);
    let (pooled, _, pooled_live, _) = collect_tree(Arc::new(ThreadPool::new(3)));
    assert_eq!(pooled, inline);
    assert_eq!(pooled_live, live);
}

/// Workers take segments of a wide graph. Whether they get to one in a
/// given GC is up to the scheduler: a few GCs make it all but certain
/// (and Miri's scheduler is too slow to count on).
#[test]
fn workers_help_with_a_wide_graph() {
    let pool: Arc<dyn Platform> = Arc::new(ThreadPool::new(3));
    let helped = (0..5).any(|_| collect_tree(pool.clone()).3 > 0);
    assert!(
        helped || cfg!(miri),
        "no worker scanned an object in five GCs"
    );
}

/// A chain never fills a segment: no worker is woken, and all of it is
/// marked on the heap's thread.
#[test]
fn a_narrow_graph_stays_on_the_heaps_thread() {
    let mut heap = Heap::with_platform(crate::verified(), Arc::new(ThreadPool::new(3)));
    let head = heap.mutate(|cx| {
        let mut next = None;
        for _ in 0..1000 {
            next = Some(cx.alloc(LinkInit { next }));
        }
        Persistent::new(cx, next.unwrap())
    });
    heap.collect();
    assert_eq!(heap.stats().last_gc.scanned_by_workers, 0);
    heap.mutate(|cx| {
        let mut length = 0;
        let mut link = Some(head.get(cx));
        while let Some(at) = link {
            length += 1;
            link = at.next.get(cx);
        }
        assert_eq!(length, 1000);
    });
}

/// The spans of a worker's runs, as cppgc's concurrent ones (a worker
/// discards pages only a GC after they are freed: `heap`'s
/// `a_worker_discards_the_pooled_pages` looks for `Gc.Discard`).
const WORKER_SPANS: [&str; 4] = [
    "Gc.ConcurrentMark",
    "Gc.ConcurrentSweep",
    "Gc.ParallelScavenge",
    "Gc.Discard",
];

/// A worker's run of a job is a span of the platform's trace, on the
/// worker, named for the job; the heap's thread runs its part of a job in
/// the GC's phase, with no span of its own. Whether a worker gets to a job
/// in a given GC is up to the scheduler: each kind is looked for once the
/// heap says a worker did it, in a few GCs (Miri's scheduler is too slow to
/// count on).
#[test]
fn a_workers_runs_are_spans_of_the_trace() {
    static TRACE: crate::Recorder = crate::Recorder::new(true);
    let platform = crate::with_trace(Arc::new(ThreadPool::new(3)), &TRACE);
    let main = std::thread::current().id();
    let (mut marked, mut scavenged) = (false, false);
    let mut swept = 0;
    for _ in 0..5 {
        let log = Log::default();
        let mut heap = Heap::with_platform(crate::verified(), platform.clone());
        let _root = heap.mutate(|cx| Persistent::new(cx, tree(cx, BRANCHES, &log).0));
        heap.collect();
        let stats = heap.stats();
        marked |= stats.last_gc.scanned_by_workers > 0;
        scavenged |= stats.last_scavenge.scanned_by_workers > 0;
        swept += stats.background_sweep_ns;
        if marked && scavenged && swept > 0 {
            break;
        }
    }
    assert!(marked || cfg!(miri), "no worker marked in five GCs");
    let events = TRACE.by_thread();
    let mut open: std::collections::HashMap<ThreadId, Vec<&str>> = Default::default();
    let mut found = std::collections::HashSet::new();
    for (thread, event) in events.iter() {
        let stack = open.entry(*thread).or_default();
        match event.strip_prefix("begin ") {
            Some(name) if WORKER_SPANS.contains(&name) => {
                assert_ne!(*thread, main, "the heap's thread's {name}");
                assert!(stack.is_empty(), "{name} in {stack:?}");
                found.insert(name);
                stack.push(name);
            }
            Some(name) => {
                assert_eq!(*thread, main, "a worker's {name}");
                stack.push(name);
            }
            None => assert!(stack.pop().is_some(), "an end with nothing begun"),
        }
    }
    assert!(
        open.values().all(Vec::is_empty),
        "spans left open: {open:?}"
    );
    assert_eq!(found.contains("Gc.ConcurrentMark"), marked, "{found:?}");
    assert_eq!(
        found.contains("Gc.ParallelScavenge"),
        scavenged,
        "{found:?}"
    );
    assert_eq!(found.contains("Gc.ConcurrentSweep"), swept > 0, "{found:?}");
}

/// A pool whose trace doesn't record isn't told of its workers' runs.
#[test]
fn a_trace_that_doesnt_record_isnt_told_of_workers_runs() {
    static TRACE: crate::Recorder = crate::Recorder::new(false);
    let platform = crate::with_trace(Arc::new(ThreadPool::new(3)), &TRACE);
    let mut by_workers = 0;
    for _ in 0..5 {
        let log = Log::default();
        let mut heap = Heap::with_platform(crate::verified(), platform.clone());
        let _root = heap.mutate(|cx| Persistent::new(cx, tree(cx, BRANCHES, &log).0));
        heap.collect();
        by_workers += heap.stats().last_gc.scanned_by_workers;
        if by_workers > 0 {
            break;
        }
    }
    assert!(by_workers > 0 || cfg!(miri), "no worker marked in five GCs");
    assert!(TRACE.names().is_empty());
}
