//! Fuzzing's operations on a heap, and the reference model they are
//! checked against: a graph that collects nothing. A run decodes bytes
//! into operations until the bytes run out, carries them out on a heap
//! with the verifier on, and checks, at the start of every operation, that
//! what the roots reach is what the model says, field for field, and that
//! no object the model reaches was finalized; at the end, that every object
//! with a finalizer was finalized exactly once. A weak edge or root read
//! gives what the model says, or nothing once that object may be gone (the
//! roots didn't reach it at a GC since); what it gives the operation holds,
//! and may link (alive again, with what it reaches). An ephemeron's read
//! likewise, its value with its key, and an id's lookup (of the wrong type,
//! nothing).
//!
//! The fuzz target (`crates/elise/fuzz`) includes this file as it is;
//! `fuzz.rs` replays the regressions the fuzzer found with it, and its
//! tests make their inputs with `encode`.
//!
//! The encoding: a byte of options (bit 0: stress; bit 1: a pool of two
//! workers rather than marking inline; bit 2: a GC once a few KiB are
//! allocated; bit 3: a nursery that doesn't grow; bit 4: marking alongside
//! the mutator, its job (and the discarding of free pages) run only by
//! `MarkStep`s, between operations, so that a run is the same each time),
//! then operations, each a byte (its
//! kind, modulo the kinds) and its arguments, as `Op` lists them; the weak
//! operations are kinds of others' with a spare bit (`SetEdge`'s top bit,
//! or an odd multiple of the kinds past `Root`, `Unroot`, `Collect` and
//! `Scavenge`'s), as `MarkStep` is a `Safepoint`'s, and so are the
//! ephemeron ones (`Push`'s and `Remove`'s) and the id table's
//! (`SetBlob`'s and `SetBack`'s, with the top bit clear), and `AllocSent`
//! (`Alloc`'s last byte, 252, which no regression has). A handle
//! or a root is an index, modulo how many there are: any bytes decode to
//! something to do. A pool runs no posted job: it would make a run depend
//! on its threads' timing (shuttle's tests run marking alongside on
//! threads).
//!
//! Bit 5 of the options: a heap a fake JS engine drives (`HeapOptions::
//! driven`), as V8 does: it starts the marking (a `MarkStep` when none
//! runs), advances it with each `MarkStep` (its own worklist of wrappers,
//! handing their objects over, then the heap's step), and ends it in its
//! pause (a `Collect`), rescanning its roots; the wrappers it didn't mark
//! die, and a live object's never does. Its operations are `Scavenge`'s
//! bytes but the first and the last (still scavenges), in a driven run
//! only, five of them the first: script getting an object's wrapper (made if it has
//! none: black while the engine marks), dropping it, passing it back
//! (reading its object, marked while the heap marks), and a property of
//! one wrapper's referring to another's (the engine's Dijkstra barrier).
//! A wrapper script holds is a root; an object refers to its wrapper (its
//! `JsEdge`, which the heap's marker gives the engine).

// The fake engine's side of a driven heap is unsafe: it hands over and
// reads back raw addresses, and writes its references into the objects.
#![allow(unsafe_code, reason = "the fake engine's side of a driven heap")]

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use std::sync::Mutex;

use elise::{
    EphemeronPair, External, Gc, GcObject, Heap, HeapOptions, HeapVector, Id, InlinePlatform, Job,
    JobDelegate, JobHandle, JsEdge, JsEdgeLocal, JsEdgeSink, LocalExternal, Member, Mutation,
    Persistent, Platform, Site, ThreadPool, WeakMember, WeakPersistent, WrapperLocal,
    WrapperMarker,
};

/// An object: four edges, a weak one, an ephemeron, a list, an edge to a
/// blob, maybe a finalizer, and its reference to its wrapper in a driven
/// heap (where it is always old).
#[derive(GcObject)]
pub struct Obj {
    id: Cell<u32>,
    e0: Member<Obj>,
    e1: Member<Obj>,
    e2: Member<Obj>,
    e3: Member<Obj>,
    weak: WeakMember<Obj>,
    pair: EphemeronPair<Obj, Obj>,
    list: Member<HeapVector<Obj>>,
    blob: Member<Blob>,
    probe: LocalExternal<Option<Probe>>,
    /// Its tag, if it has a finalizer to run on the sweep's worker: only it
    /// points to it, so the tag lives as long.
    tag: Member<Tag>,
    /// The fake engine's reference to its wrapper: the wrapper's index plus
    /// one, 0 for none.
    wrapper: JsEdge,
}

/// An object's finalizer for the sweep's worker (an `External`'s, `Send`),
/// in an object of its own: `Obj`'s finalizer runs on the heap's thread.
#[derive(GcObject)]
pub struct Tag {
    probe: External<SentProbe>,
}

/// Logs its object's id when finalized, from any thread.
pub struct SentProbe {
    id: u32,
    log: Arc<Mutex<Vec<u32>>>,
}

impl Drop for SentProbe {
    fn drop(&mut self) {
        self.log.lock().unwrap().push(self.id);
    }
}

/// A large object (40 KiB): its words are its id and their index.
#[derive(GcObject)]
pub struct Blob {
    id: Cell<u32>,
    words: [Cell<u64>; 5000],
    back: Member<Obj>,
}

/// Logs its object's id when finalized.
pub struct Probe {
    id: u32,
    log: Rc<RefCell<Vec<u32>>>,
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.log.borrow_mut().push(self.id);
    }
}

/// An operation. `Handle`s pick among the objects of the operation: those
/// the roots reached at its start and those it made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// An object, from `SITES[site - 1]`, or no site for 0.
    Alloc {
        probe: bool,
        site: u8,
    },
    AllocBlob,
    /// An object with no site whose finalizer is the sweep's (`Tag`).
    AllocSent,
    SetEdge {
        from: u16,
        field: u8,
        to: Option<u16>,
    },
    SetBlob {
        from: u16,
        to: Option<u16>,
    },
    SetBack {
        blob: u16,
        to: Option<u16>,
    },
    Push {
        list: u16,
        to: u16,
    },
    Remove {
        list: u16,
        at: u16,
    },
    Root {
        object: u16,
    },
    Unroot {
        root: u16,
    },
    SetWeak {
        from: u16,
        to: Option<u16>,
    },
    /// Reads an object's weak edge: what it gives, the operation holds.
    ReadWeak {
        from: u16,
    },
    WeakRoot {
        object: u16,
    },
    WeakUnroot {
        root: u16,
    },
    ReadWeakRoot {
        root: u16,
    },
    /// Sets an object's ephemeron: a key (or none, and no value) and a
    /// value.
    SetPair {
        from: u16,
        key: Option<u16>,
        value: Option<u16>,
    },
    /// Reads an object's ephemeron: what it gives, the operation holds.
    ReadPair {
        from: u16,
    },
    /// Gives an object (or blob) an id.
    NewId {
        object: u16,
    },
    /// Looks an id up: what it finds, the operation holds.
    ById {
        id: u16,
    },
    /// Ends the operation: the next starts at a safepoint.
    Safepoint,
    /// Ends the operation, and runs a marking's job, if one is posted,
    /// until it has been asked whether to yield `budget` times (every 64
    /// objects): marking alongside the mutator, between its operations;
    /// the job discarding free pages too (asked before each). A safepoint
    /// where nothing marks alongside.
    MarkStep {
        budget: u8,
    },
    /// Ends the operation and collects.
    Collect,
    /// Ends the operation and scavenges.
    Scavenge,
    /// Script gets an object's wrapper (a driven run's), made if it has
    /// none, and holds it.
    Wrapper {
        object: u16,
    },
    /// Script drops a wrapper it holds.
    Release {
        wrapper: u16,
    },
    /// Script passes a wrapper it holds back: its object, which the
    /// operation holds.
    Unwrap {
        wrapper: u16,
    },
    /// A held wrapper's property refers to another held wrapper.
    Link {
        from: u16,
        to: u16,
    },
    /// A held wrapper's property refers to nothing.
    Unlink {
        from: u16,
    },
}

const KINDS: u8 = 12;

/// `AllocSent`'s byte: `Alloc`'s last.
const ALLOC_SENT: u8 = 252;

/// The sites an `Alloc` may name: a heap may tenure them (with a nursery
/// that doesn't grow, at once) and take them back.
static SITES: [Site; 2] = [const { Site::new() }; 2];

/// At most this many operations, objects and blobs a run: bytes long
/// enough to make more are cut off, so a run stays quick.
const MAX_OPS: usize = 4000;
const MAX_OBJECTS: u32 = 3000;
const MAX_BLOBS: u32 = 60;

pub struct Options {
    pub stress: bool,
    pub pool: bool,
    pub small_limit: bool,
    /// The nursery is its largest from the start: a site whose objects
    /// survive is tenured at its first scavenge.
    pub fixed_nursery: bool,
    /// Marking runs alongside the mutator, by `MarkStep`s.
    pub concurrent: bool,
    /// A fake engine drives the heap's markings.
    pub driven: bool,
}

/// Decodes `bytes`: the options and the operations.
pub fn decode(bytes: &[u8]) -> (Options, Vec<Op>) {
    let mut at = 0;
    let mut byte = || {
        let value = bytes.get(at).copied();
        at += 1;
        value
    };
    let flags = byte().unwrap_or(0);
    let options = Options {
        stress: flags & 1 != 0,
        pool: flags & 2 != 0,
        small_limit: flags & 4 != 0,
        fixed_nursery: flags & 8 != 0,
        concurrent: flags & 16 != 0,
        driven: flags & 32 != 0,
    };
    let driven = options.driven;
    let mut ops = Vec::new();
    while ops.len() < MAX_OPS {
        let Some(kind) = byte() else { break };
        let odd = (kind / KINDS) % 2 == 1;
        let mut u16 = || Some(u16::from_le_bytes([byte()?, byte()?]));
        let op = match kind % KINDS {
            0 if kind == ALLOC_SENT => Op::AllocSent,
            0 => Op::Alloc {
                probe: kind & 0x80 != 0,
                site: (kind / KINDS) % 3,
            },
            1 => Op::AllocBlob,
            2 => {
                let (from, to) = (u16(), u16());
                let field = (kind >> 4) & 3;
                let to = to.map(|to| (kind & 0x40 == 0).then_some(to));
                match (from, to) {
                    (Some(from), Some(to)) if kind & 0x80 != 0 => Op::SetWeak { from, to },
                    (Some(from), Some(to)) => Op::SetEdge { from, field, to },
                    _ => break,
                }
            }
            // An odd multiple with the top bit clear (the regressions use
            // the others).
            3 if odd && kind & 0x80 == 0 => match u16() {
                Some(object) => Op::NewId { object },
                None => break,
            },
            4 if odd && kind & 0x80 == 0 => match u16() {
                Some(id) => Op::ById { id },
                None => break,
            },
            3 => match (u16(), u16()) {
                (Some(from), Some(to)) => Op::SetBlob {
                    from,
                    to: (kind & 0x80 == 0).then_some(to),
                },
                _ => break,
            },
            4 => match (u16(), u16()) {
                (Some(blob), Some(to)) => Op::SetBack {
                    blob,
                    to: (kind & 0x80 == 0).then_some(to),
                },
                _ => break,
            },
            // An odd multiple: an ephemeron's key, unless the top bit is
            // set; its value, unless the next multiple's bit is.
            5 if odd => match (u16(), u16(), u16()) {
                (Some(from), Some(key), Some(value)) => {
                    let key = (kind & 0x80 == 0).then_some(key);
                    Op::SetPair {
                        from,
                        key,
                        value: (key.is_some() && (kind / KINDS / 2).is_multiple_of(2))
                            .then_some(value),
                    }
                }
                _ => break,
            },
            5 => match (u16(), u16()) {
                (Some(list), Some(to)) => Op::Push { list, to },
                _ => break,
            },
            6 if odd => match u16() {
                Some(from) => Op::ReadPair { from },
                None => break,
            },
            6 => match (u16(), u16()) {
                (Some(list), Some(at)) => Op::Remove { list, at },
                _ => break,
            },
            7 => match u16() {
                Some(object) if odd => Op::WeakRoot { object },
                Some(object) => Op::Root { object },
                None => break,
            },
            8 => match u16() {
                Some(root) if odd => Op::WeakUnroot { root },
                Some(root) => Op::Unroot { root },
                None => break,
            },
            // An odd multiple of the kinds past 9: a step.
            9 if odd => Op::MarkStep {
                budget: kind / (2 * KINDS),
            },
            9 => Op::Safepoint,
            10 if odd => match u16() {
                Some(from) => Op::ReadWeak { from },
                None => break,
            },
            10 => Op::Collect,
            _ if odd => match u16() {
                Some(root) => Op::ReadWeakRoot { root },
                None => break,
            },
            // The engine's, by the multiple: 35 to 131, then 155, 179, 203,
            // 227.
            _ if driven && kind != 11 && kind != 251 => {
                let first = u16();
                match (kind, first) {
                    (..=131, Some(object)) => Op::Wrapper { object },
                    (155, Some(wrapper)) => Op::Release { wrapper },
                    (179, Some(wrapper)) => Op::Unwrap { wrapper },
                    (203, Some(from)) => match u16() {
                        Some(to) => Op::Link { from, to },
                        None => break,
                    },
                    (_, Some(from)) => Op::Unlink { from },
                    (_, None) => break,
                }
            }
            _ => Op::Scavenge,
        };
        ops.push(op);
    }
    (options, ops)
}

/// Encodes `options` and `ops` so that `decode` gives them back.
pub fn encode(options: &Options, ops: &[Op]) -> Vec<u8> {
    let mut bytes = vec![
        u8::from(options.stress)
            | u8::from(options.pool) << 1
            | u8::from(options.small_limit) << 2
            | u8::from(options.fixed_nursery) << 3
            | u8::from(options.concurrent) << 4
            | u8::from(options.driven) << 5,
    ];
    let odd = |byte: u8| (byte / KINDS) % 2 == 1;
    // The byte of an operation's kind that decodes to its flags too.
    let kind = |tag: u8, flags: &dyn Fn(u8) -> bool| {
        (0..=u8::MAX)
            .find(|&byte| byte % KINDS == tag && flags(byte))
            .expect("a byte for every operation")
    };
    for op in ops {
        let (byte, args): (u8, Vec<u16>) = match *op {
            Op::Alloc { probe, site } => (
                kind(0, &|byte| {
                    (byte & 0x80 != 0) == probe && (byte / KINDS) % 3 == site
                }),
                vec![],
            ),
            Op::AllocBlob => (1, vec![]),
            Op::AllocSent => (ALLOC_SENT, vec![]),
            Op::SetEdge { from, field, to } => (
                kind(2, &|byte| {
                    (byte >> 4) & 3 == field
                        && (byte & 0x40 != 0) == to.is_none()
                        && byte & 0x80 == 0
                }),
                vec![from, to.unwrap_or(0)],
            ),
            Op::SetWeak { from, to } => (
                kind(2, &|byte| {
                    byte & 0x80 != 0 && (byte & 0x40 != 0) == to.is_none()
                }),
                vec![from, to.unwrap_or(0)],
            ),
            Op::SetBlob { from, to } => (
                kind(3, &|byte| {
                    !(odd(byte) && byte & 0x80 == 0) && (byte & 0x80 != 0) == to.is_none()
                }),
                vec![from, to.unwrap_or(0)],
            ),
            Op::SetBack { blob, to } => (
                kind(4, &|byte| {
                    !(odd(byte) && byte & 0x80 == 0) && (byte & 0x80 != 0) == to.is_none()
                }),
                vec![blob, to.unwrap_or(0)],
            ),
            Op::Push { list, to } => (5, vec![list, to]),
            Op::Remove { list, at } => (6, vec![list, at]),
            Op::SetPair { from, key, value } => (
                kind(5, &|byte| {
                    odd(byte)
                        && (byte & 0x80 != 0) == key.is_none()
                        && (key.is_none() || ((byte / KINDS / 2) % 2 == 1) == value.is_none())
                }),
                vec![from, key.unwrap_or(0), value.unwrap_or(0)],
            ),
            Op::ReadPair { from } => (kind(6, &odd), vec![from]),
            Op::NewId { object } => (kind(3, &|byte| odd(byte) && byte & 0x80 == 0), vec![object]),
            Op::ById { id } => (kind(4, &|byte| odd(byte) && byte & 0x80 == 0), vec![id]),
            Op::Root { object } => (7, vec![object]),
            Op::Unroot { root } => (8, vec![root]),
            Op::WeakRoot { object } => (kind(7, &odd), vec![object]),
            Op::WeakUnroot { root } => (kind(8, &odd), vec![root]),
            Op::ReadWeak { from } => (kind(10, &odd), vec![from]),
            Op::ReadWeakRoot { root } => (kind(11, &odd), vec![root]),
            Op::Safepoint => (9, vec![]),
            Op::MarkStep { budget } => (
                kind(9, &|byte| {
                    (byte / KINDS) % 2 == 1 && byte / (2 * KINDS) == budget
                }),
                vec![],
            ),
            Op::Collect => (10, vec![]),
            Op::Scavenge => (11, vec![]),
            Op::Wrapper { object } => (131, vec![object]),
            Op::Release { wrapper } => (155, vec![wrapper]),
            Op::Unwrap { wrapper } => (179, vec![wrapper]),
            Op::Link { from, to } => (203, vec![from, to]),
            Op::Unlink { from } => (227, vec![from]),
        };
        let engines = matches!(
            op,
            Op::Wrapper { .. }
                | Op::Release { .. }
                | Op::Unwrap { .. }
                | Op::Link { .. }
                | Op::Unlink { .. }
        );
        assert!(
            options.driven || !engines,
            "the engine's operations are a driven run's"
        );
        bytes.push(byte);
        for arg in args {
            bytes.extend(arg.to_le_bytes());
        }
    }
    bytes
}

/// The model's object: what its fields hold, by id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Model {
    blob: bool,
    edges: [Option<u32>; 4],
    /// The weak edge's object, until a read finds it cleared.
    weak: Option<u32>,
    /// The ephemeron's key and value, until a read finds it cleared.
    pair: Option<(u32, Option<u32>)>,
    list: Vec<u32>,
    to_blob: Option<u32>,
    probe: bool,
    /// What its wrapper's property refers to: that wrapper's object.
    js: Option<u32>,
}

#[derive(Clone, Copy)]
enum Handle<'gc> {
    Obj(Gc<'gc, Obj>),
    Blob(Gc<'gc, Blob>),
}

enum Root {
    Obj(Persistent<Obj>),
    Blob(Persistent<Blob>),
}

/// The workers of runs with a pool: one pool, not threads for each run,
/// that runs no posted job. Under Miri, a pool for each run, dropped with
/// its heap: Miri fails a test whose threads outlive its main thread.
fn pool() -> Arc<dyn Platform> {
    if cfg!(miri) {
        return Arc::new(NoPosting(Arc::new(ThreadPool::new(2))));
    }
    static POOL: OnceLock<Arc<ThreadPool>> = OnceLock::new();
    Arc::new(NoPosting(
        POOL.get_or_init(|| Arc::new(ThreadPool::new(2))).clone(),
    ))
}

/// A platform's `run`s, but none of its posted jobs.
struct NoPosting(Arc<ThreadPool>);

impl Platform for NoPosting {
    fn run(&self, job: &dyn Job) {
        self.0.run(job);
    }
}

/// A platform that keeps the jobs posted (a marking's, the discarding of
/// pages), and runs them only in `step`, on the caller (the fuzzer's
/// thread, between operations).
#[derive(Default)]
pub struct Stepping {
    /// By the number each was posted with.
    posted: Mutex<Vec<(usize, Arc<dyn Job + Send>)>>,
    next: Mutex<usize>,
}

impl Stepping {
    /// Runs each posted job until it is done or has been asked whether to
    /// yield `budget` times.
    pub fn step(&self, budget: u8) {
        let jobs: Vec<_> = self
            .posted
            .lock()
            .unwrap()
            .iter()
            .map(|(_, job)| job.clone())
            .collect();
        for job in jobs {
            job.run(&Step {
                asked: Cell::new(0),
                budget,
            });
        }
    }
}

struct Step {
    asked: Cell<u8>,
    budget: u8,
}

impl JobDelegate for Step {
    fn notify_concurrency_increase(&self) {}

    fn is_joining_thread(&self) -> bool {
        false
    }

    fn should_yield(&self) -> bool {
        let asked = self.asked.get();
        self.asked.set(asked.saturating_add(1));
        asked >= self.budget
    }
}

struct StepHandle(Arc<Stepping>, usize);

impl JobHandle for StepHandle {
    fn notify_concurrency_increase(&self) {}

    fn cancel(&self) {
        self.0
            .posted
            .lock()
            .unwrap()
            .retain(|&(id, _)| id != self.1);
    }
}

struct Steps(Arc<Stepping>);

impl Platform for Steps {
    fn run(&self, job: &dyn Job) {
        InlinePlatform.run(job);
    }

    fn post_job(&self, job: Arc<dyn Job + Send>) -> Option<Box<dyn JobHandle>> {
        let mut next = self.0.next.lock().unwrap();
        *next += 1;
        self.0.posted.lock().unwrap().push((*next, job));
        Some(Box::new(StepHandle(self.0.clone(), *next)))
    }

    fn posts_jobs(&self) -> bool {
        true
    }
}

/// A wrapper of the fake engine's.
struct Wrapper {
    /// Its object's id, and address (`Mutation::engine_address`).
    object: u32,
    address: NonNull<u8>,
    /// Script holds it: one of the engine's roots.
    held: bool,
    /// What its property refers to.
    link: Option<usize>,
    /// Marked in the engine's marking that runs, or the last.
    marked: bool,
    alive: bool,
}

/// The fake engine (V8's part): its wrappers, the JS edges the heap's
/// marker gives it, and its marking, which drives the heap's.
#[derive(Default)]
struct Engine {
    wrappers: Vec<Wrapper>,
    edges: Edges,
    marking: Option<EngineMarking>,
}

/// The engine's marking: the hand-over of the objects of the wrappers it
/// marks, and its worklist (marked, not yet scanned).
struct EngineMarking {
    local: WrapperLocal,
    worklist: Vec<usize>,
}

/// The JS edges the heap's marker gives the engine: their values, the
/// wrappers' indices plus one.
#[derive(Clone, Default)]
struct Edges(Arc<Mutex<Vec<u64>>>);

impl JsEdgeSink for Edges {
    fn local(&self) -> Box<dyn JsEdgeLocal> {
        Box::new(EdgesLocal(self.0.clone(), Vec::new()))
    }
}

/// A marking thread's JS edges, published when it ends.
struct EdgesLocal(Arc<Mutex<Vec<u64>>>, Vec<u64>);

impl JsEdgeLocal for EdgesLocal {
    unsafe fn mark(&mut self, edge: NonNull<JsEdge>) {
        // SAFETY: as the heap promises: a live object's edge, which only
        // the engine writes (on the heap's thread, before the marker could
        // see it), atomically.
        let value =
            unsafe { AtomicU64::from_ptr((*edge.as_ptr()).as_ptr()) }.load(Ordering::Relaxed);
        if value != 0 {
            self.1.push(value);
        }
    }
}

impl Drop for EdgesLocal {
    fn drop(&mut self) {
        self.0.lock().unwrap().append(&mut self.1);
    }
}

impl Engine {
    /// The ids of the held wrappers' objects: the engine's roots.
    fn held(&self) -> Vec<u32> {
        self.wrappers
            .iter()
            .filter(|w| w.alive && w.held)
            .map(|w| w.object)
            .collect()
    }

    /// The indices of the held wrappers.
    fn held_wrappers(&self) -> Vec<usize> {
        (0..self.wrappers.len())
            .filter(|&w| self.wrappers[w].alive && self.wrappers[w].held)
            .collect()
    }

    /// Marks wrapper `w` (gray: its object handed over when scanned).
    fn mark(&mut self, w: usize) {
        let wrapper = &mut self.wrappers[w];
        assert!(wrapper.alive, "the engine marks a dead wrapper {w}");
        if wrapper.marked {
            return;
        }
        wrapper.marked = true;
        if let Some(marking) = &mut self.marking {
            marking.worklist.push(w);
        }
    }

    /// Starts a marking: the heap's, and its own from its roots.
    fn start(&mut self, heap: &mut Heap) {
        let marker: WrapperMarker = heap.start_marking(Arc::new(self.edges.clone()));
        self.marking = Some(EngineMarking {
            local: marker.local(),
            worklist: Vec::new(),
        });
        for wrapper in &mut self.wrappers {
            wrapper.marked = false;
        }
        for w in self.held_wrappers() {
            self.mark(w);
        }
    }

    /// Scans up to `budget` wrappers of its worklist (all, for none),
    /// handing their objects over, then marks the wrappers of the JS edges
    /// the heap gave it: whether it has nothing left.
    fn drain(&mut self, budget: Option<usize>) -> bool {
        let mut scanned = 0;
        loop {
            let marking = self.marking.as_mut().expect("the engine marks");
            if budget.is_some_and(|budget| scanned >= budget) {
                break;
            }
            let Some(w) = marking.worklist.pop() else {
                break;
            };
            scanned += 1;
            let wrapper = &self.wrappers[w];
            // SAFETY: a live wrapper's object, which it keeps alive: the
            // address the heap gave it, of a live object of the heap.
            unsafe { marking.local.hand(wrapper.address) };
            if let Some(to) = wrapper.link {
                self.mark(to);
            }
        }
        let edges = std::mem::take(&mut *self.edges.0.lock().unwrap());
        for value in edges {
            self.mark(usize::try_from(value - 1).unwrap());
        }
        let marking = self.marking.as_mut().expect("the engine marks");
        marking.local.publish();
        marking.worklist.is_empty()
    }

    /// A step of its marking (`MarkStep`'s), or the start of one.
    fn step(&mut self, heap: &mut Heap, budget: u8) {
        if self.marking.is_none() {
            self.start(heap);
            return;
        }
        self.drain(Some(usize::from(budget)));
        heap.advance_marking(false, Duration::ZERO);
    }

    /// Its pause (`Collect`'s): its roots rescanned, both sides advanced
    /// until neither finds more, the heap's marking finished; the wrappers
    /// it didn't mark die. A marking is started first if none runs.
    /// `reached`: what the heap's and its own roots reach in the model.
    fn finish(&mut self, heap: &mut Heap, reached: &BTreeSet<u32>) {
        if self.marking.is_none() {
            self.start(heap);
        }
        loop {
            for w in self.held_wrappers() {
                self.mark(w);
            }
            let drained = self.drain(None);
            let heap_done = heap.advance_marking(true, Duration::MAX);
            // What the heap's pause gave it.
            let drained = self.drain(None) && drained;
            if heap_done && drained {
                break;
            }
        }
        heap.finish_marking();
        self.marking = None;
        for (w, wrapper) in self.wrappers.iter_mut().enumerate() {
            if wrapper.alive && !wrapper.marked {
                wrapper.alive = false;
                assert!(
                    !reached.contains(&wrapper.object),
                    "wrapper {w} died, but its object {} is reached",
                    wrapper.object
                );
            }
        }
    }
}

/// Runs `bytes`: panics where the heap and the model disagree.
pub fn run(bytes: &[u8]) {
    let (options, ops) = decode(bytes);
    let steps = options.concurrent.then(|| Arc::new(Stepping::default()));
    let platform: Arc<dyn Platform> = if let Some(steps) = &steps {
        Arc::new(Steps(steps.clone()))
    } else if options.pool {
        pool()
    } else {
        Arc::new(InlinePlatform)
    };
    let heap_options = HeapOptions {
        stress: options.stress,
        // Not under Miri, as `verified`.
        verify: !cfg!(miri),
        min_allocation_bytes: if options.small_limit {
            8 << 10
        } else {
            HeapOptions::default().min_allocation_bytes
        },
        // Small limits finish markings at them too, past half of them.
        min_overshoot_bytes: if options.small_limit {
            0
        } else {
            HeapOptions::default().min_overshoot_bytes
        },
        // One page: allocations past it go to the old generation (and
        // two at most, as it grows).
        nursery_bytes: if options.small_limit {
            1
        } else {
            HeapOptions::default().nursery_bytes
        },
        max_nursery_bytes: if options.fixed_nursery {
            0
        } else if options.small_limit {
            2 << 18
        } else {
            HeapOptions::default().max_nursery_bytes
        },
        // Walks the pages each time a sweep is done, whatever state they
        // are in.
        measure_fragmentation: true,
        // The heap starts and finishes its markings itself, or the engine
        // does.
        driven: options.driven,
    };
    let log: Rc<RefCell<Vec<u32>>> = Rc::default();
    let sent: Arc<Mutex<Vec<u32>>> = Arc::default();
    let mut heap = Heap::with_platform(heap_options, platform);
    let mut model: BTreeMap<u32, Model> = BTreeMap::new();
    let mut roots: Vec<(Root, u32)> = Vec::new();
    // Each weak root, and its object until a read finds it cleared.
    let mut weak_roots: Vec<(WeakPersistent<Obj>, Option<u32>)> = Vec::new();
    // Each id given, its object (until a lookup finds nothing), and
    // whether it is a blob.
    let mut ids: Vec<(Id, Option<u32>, bool)> = Vec::new();
    let mut next_id = 0u32;
    // Ids the model can't reach from the roots once a GC may have run.
    let mut dead: BTreeSet<u32> = BTreeSet::new();
    let mut engine = Engine::default();
    let mut ops = ops.into_iter().peekable();
    while ops.peek().is_some() {
        // A GC may run at this safepoint, before the operation's reads: what
        // the roots don't reach in the model may be gone by then. (Marked so
        // only after the operation, a weak read in it could find cleared an
        // object the model still counted alive.)
        if heap.should_collect() {
            let reached = reach(&roots, &engine.held(), &model);
            dead.extend(model.keys().filter(|id| !reached.contains(id)));
        }
        let stats = heap.stats();
        let end = heap.mutate(|cx| {
            let mut handles = check(cx, &roots, &engine, &model);
            for op in ops.by_ref() {
                match op {
                    Op::Safepoint | Op::MarkStep { .. } | Op::Collect | Op::Scavenge => {
                        return op;
                    }
                    op => apply(
                        cx,
                        op,
                        &mut State {
                            handles: &mut handles,
                            model: &mut model,
                            roots: &mut roots,
                            weak_roots: &mut weak_roots,
                            ids: &mut ids,
                            dead: &mut dead,
                            next_id: &mut next_id,
                            log: &log,
                            sent: &sent,
                            engine: &mut engine,
                        },
                    ),
                }
            }
            Op::Safepoint
        });
        match end {
            Op::Collect if options.driven => {
                let reached = reach(&roots, &engine.held(), &model);
                engine.finish(&mut heap, &reached);
            }
            Op::Collect => heap.collect(),
            Op::Scavenge => heap.scavenge(),
            Op::MarkStep { budget } => {
                if options.driven {
                    engine.step(&mut heap, budget);
                }
                if let Some(steps) = &steps {
                    steps.step(budget);
                }
            }
            _ => {}
        }
        // A GC ran (a scavenge, or a full GC) at this safepoint or after
        // the operation: what the roots don't reach in the model may be
        // gone.
        let after = heap.stats();
        if (after.gcs, after.scavenges) != (stats.gcs, stats.scavenges) {
            let reached = reach(&roots, &engine.held(), &model);
            dead.extend(model.keys().filter(|id| !reached.contains(id)));
        }
        for id in log.borrow().iter().chain(sent.lock().unwrap().iter()) {
            assert!(
                dead.contains(id),
                "object {id} finalized, but the model reaches it"
            );
        }
    }
    heap.mutate(|cx| {
        check(cx, &roots, &engine, &model);
    });
    drop(engine);
    drop(roots);
    drop(weak_roots);
    drop(ids);
    drop(heap);
    let mut finalized = log.borrow().clone();
    finalized.extend(sent.lock().unwrap().iter());
    finalized.sort_unstable();
    let probed: Vec<u32> = model
        .iter()
        .filter(|(_, object)| object.probe)
        .map(|(&id, _)| id)
        .collect();
    assert_eq!(finalized, probed, "every finalizer runs exactly once");
}

/// The ids the roots reach in the model, and the engine's (`held`, its
/// held wrappers' objects).
fn reach(roots: &[(Root, u32)], held: &[u32], model: &BTreeMap<u32, Model>) -> BTreeSet<u32> {
    let mut stack: Vec<u32> = roots.iter().map(|&(_, id)| id).collect();
    stack.extend(held);
    reach_from(stack, model)
}

/// The ids `stack`'s reach in the model, through strong edges, and
/// through wrappers: an object's wrapper's property's wrapper's object.
fn reach_from(mut stack: Vec<u32>, model: &BTreeMap<u32, Model>) -> BTreeSet<u32> {
    let mut reached = BTreeSet::new();
    while let Some(id) = stack.pop() {
        if !reached.insert(id) {
            continue;
        }
        let object = &model[&id];
        stack.extend(object.edges.iter().flatten());
        stack.extend(&object.list);
        stack.extend(object.to_blob);
        stack.extend(object.js);
    }
    reached
}

/// Walks the heap from the roots and the engine's held wrappers' objects,
/// checking every object against the model: the handles of the
/// operation, in the order of their ids.
fn check<'gc>(
    cx: &Mutation<'gc>,
    roots: &[(Root, u32)],
    engine: &Engine,
    model: &BTreeMap<u32, Model>,
) -> Vec<Handle<'gc>> {
    let mut found: BTreeMap<u32, Handle<'gc>> = BTreeMap::new();
    let mut stack: Vec<Handle<'gc>> = roots
        .iter()
        .map(|(root, _)| match root {
            Root::Obj(root) => Handle::Obj(root.get(cx)),
            Root::Blob(root) => Handle::Blob(root.get(cx)),
        })
        .collect();
    for wrapper in engine.wrappers.iter().filter(|w| w.alive && w.held) {
        // SAFETY: a live wrapper's object, which it keeps alive, of this
        // heap (marked if the heap marks: the engine's root).
        stack.push(Handle::Obj(unsafe {
            cx.from_engine::<Obj>(wrapper.address)
        }));
    }
    let id = |handle: Handle<'_>| match handle {
        Handle::Obj(object) => object.id.get(),
        Handle::Blob(blob) => blob.id.get(),
    };
    while let Some(handle) = stack.pop() {
        let at = id(handle);
        if found.insert(at, handle).is_some() {
            continue;
        }
        let expected = model
            .get(&at)
            .unwrap_or_else(|| panic!("object {at} isn't in the model"));
        match handle {
            Handle::Obj(object) => {
                let edges =
                    [&object.e0, &object.e1, &object.e2, &object.e3].map(|edge| edge.get(cx));
                let list = object.list.get(cx).expect("an object has a list");
                let items: Vec<Gc<'gc, Obj>> =
                    (0..list.len()).map(|i| list.get(cx, i).unwrap()).collect();
                let actual = Model {
                    blob: false,
                    edges: edges.map(|edge| edge.map(|edge| edge.id.get())),
                    // Not read here: a read while marking runs marks its
                    // object, which would keep the weak edges of every
                    // object reached; `ReadWeak` and `ReadPair` check them.
                    weak: expected.weak,
                    pair: expected.pair,
                    list: items.iter().map(|item| item.id.get()).collect(),
                    to_blob: object.blob.get(cx).map(|blob| blob.id.get()),
                    probe: object.probe.is_some() || object.tag.get(cx).is_some(),
                    // The engine's, not the heap's.
                    js: expected.js,
                };
                assert_eq!(&actual, expected, "object {at}");
                // Through its wrapper's property: that wrapper's object.
                // SAFETY: the edge's 8 aligned bytes, which only the
                // engine writes, on this thread.
                let wrapper =
                    unsafe { AtomicU64::from_ptr(object.wrapper.as_ptr()) }.load(Ordering::Relaxed);
                if wrapper != 0 {
                    let w = usize::try_from(wrapper - 1).unwrap();
                    let wrapper = &engine.wrappers[w];
                    assert!(wrapper.alive, "object {at} lives, but its wrapper {w} died");
                    if let Some(to) = wrapper.link {
                        // SAFETY: a live wrapper's object (a marked
                        // wrapper's target is marked), of this heap.
                        stack.push(Handle::Obj(unsafe {
                            cx.from_engine::<Obj>(engine.wrappers[to].address)
                        }));
                    }
                }
                stack.extend(edges.into_iter().flatten().map(Handle::Obj));
                stack.extend(items.into_iter().map(Handle::Obj));
                stack.extend(object.blob.get(cx).map(Handle::Blob));
            }
            Handle::Blob(blob) => {
                for (i, word) in blob.words.iter().enumerate().step_by(997) {
                    assert_eq!(
                        word.get(),
                        u64::from(at) << 16 | i as u64,
                        "blob {at}'s word {i}"
                    );
                }
                let back = blob.back.get(cx);
                let actual = Model {
                    blob: true,
                    edges: [back.map(|back| back.id.get()), None, None, None],
                    ..Model::default()
                };
                assert_eq!(&actual, expected, "blob {at}");
                stack.extend(back.map(Handle::Obj));
            }
        }
    }
    let reached = reach(roots, &engine.held(), model);
    assert_eq!(
        found.keys().copied().collect::<BTreeSet<u32>>(),
        reached,
        "the roots reach what the model says"
    );
    found.into_values().collect()
}

/// What an operation changes: its handles, the model, the roots.
struct State<'a, 'gc> {
    handles: &'a mut Vec<Handle<'gc>>,
    model: &'a mut BTreeMap<u32, Model>,
    roots: &'a mut Vec<(Root, u32)>,
    weak_roots: &'a mut Vec<(WeakPersistent<Obj>, Option<u32>)>,
    ids: &'a mut Vec<(Id, Option<u32>, bool)>,
    /// The ids the roots didn't reach at a GC since.
    dead: &'a mut BTreeSet<u32>,
    next_id: &'a mut u32,
    log: &'a Rc<RefCell<Vec<u32>>>,
    sent: &'a Arc<Mutex<Vec<u32>>>,
    engine: &'a mut Engine,
}

impl<'gc> State<'_, 'gc> {
    /// A weak edge's or root's read, `got`, against the model's `expected`:
    /// the same object, which the operation holds now (alive, with what it
    /// reaches); or nothing, once that object may be gone. Whether it was
    /// cleared.
    fn weak_read(&mut self, got: Option<Gc<'gc, Obj>>, expected: Option<u32>, what: &str) -> bool {
        self.weak_read_handle(got.map(Handle::Obj), expected, what)
    }

    /// `weak_read`'s, for an object or a blob.
    fn weak_read_handle(
        &mut self,
        got: Option<Handle<'gc>>,
        expected: Option<u32>,
        what: &str,
    ) -> bool {
        match (got, expected) {
            (Some(handle), expected) => {
                let id = match handle {
                    Handle::Obj(object) => object.id.get(),
                    Handle::Blob(blob) => blob.id.get(),
                };
                assert_eq!(Some(id), expected, "{what} gives object {id}");
                for id in reach_from(vec![id], self.model) {
                    self.dead.remove(&id);
                }
                self.handles.push(handle);
                false
            }
            (None, Some(id)) => {
                assert!(
                    self.dead.contains(&id),
                    "{what} is cleared, but its object {id} may be alive"
                );
                true
            }
            (None, None) => false,
        }
    }
}

fn apply<'gc>(cx: &Mutation<'gc>, op: Op, state: &mut State<'_, 'gc>) {
    let handles = &mut *state.handles;
    let model = &mut *state.model;
    let roots = &mut *state.roots;
    let next_id = &mut *state.next_id;
    let log = state.log;
    let objects: Vec<(Gc<'gc, Obj>, u32)> = handles
        .iter()
        .filter_map(|handle| match *handle {
            Handle::Obj(object) => Some((object, object.id.get())),
            Handle::Blob(_) => None,
        })
        .collect();
    let blobs: Vec<(Gc<'gc, Blob>, u32)> = handles
        .iter()
        .filter_map(|handle| match *handle {
            Handle::Blob(blob) => Some((blob, blob.id.get())),
            Handle::Obj(_) => None,
        })
        .collect();
    let pick = |n: u16, len: usize| (len > 0).then(|| usize::from(n) % len);
    match op {
        Op::Alloc { probe, site } => {
            if *next_id >= MAX_OBJECTS {
                return;
            }
            let id = *next_id;
            *next_id += 1;
            let init = ObjInit {
                id: Cell::new(id),
                e0: None,
                e1: None,
                e2: None,
                e3: None,
                weak: None,
                pair: (None, None),
                list: Some(HeapVector::new(cx, 0)),
                blob: None,
                probe: LocalExternal::new(probe.then(|| Probe {
                    id,
                    log: log.clone(),
                })),
                tag: None,
                wrapper: (),
            };
            let object = match usize::from(site) {
                0 => cx.alloc(init),
                site => cx.alloc_in(&SITES[site - 1], init),
            };
            model.insert(
                id,
                Model {
                    probe,
                    ..Model::default()
                },
            );
            handles.push(Handle::Obj(object));
        }
        Op::AllocSent => {
            if *next_id >= MAX_OBJECTS {
                return;
            }
            let id = *next_id;
            *next_id += 1;
            let tag = cx.alloc(TagInit {
                probe: External::new(SentProbe {
                    id,
                    log: state.sent.clone(),
                }),
            });
            let object = cx.alloc(ObjInit {
                id: Cell::new(id),
                e0: None,
                e1: None,
                e2: None,
                e3: None,
                weak: None,
                pair: (None, None),
                list: Some(HeapVector::new(cx, 0)),
                blob: None,
                probe: LocalExternal::new(None),
                tag: Some(tag),
                wrapper: (),
            });
            model.insert(
                id,
                Model {
                    probe: true,
                    ..Model::default()
                },
            );
            handles.push(Handle::Obj(object));
        }
        Op::AllocBlob => {
            let blobs_made = model.values().filter(|object| object.blob).count();
            if *next_id >= MAX_OBJECTS || blobs_made >= MAX_BLOBS as usize {
                return;
            }
            let id = *next_id;
            *next_id += 1;
            let blob = cx.alloc(BlobInit {
                id: Cell::new(id),
                words: std::array::from_fn(|i| Cell::new(u64::from(id) << 16 | i as u64)),
                back: None,
            });
            model.insert(
                id,
                Model {
                    blob: true,
                    ..Model::default()
                },
            );
            handles.push(Handle::Blob(blob));
        }
        Op::SetEdge { from, field, to } => {
            let Some(from) = pick(from, objects.len()) else {
                return;
            };
            let to = match to {
                Some(to) => match pick(to, objects.len()) {
                    Some(to) => Some(objects[to]),
                    None => return,
                },
                None => None,
            };
            let (object, id) = objects[from];
            let edge = [&object.e0, &object.e1, &object.e2, &object.e3][usize::from(field)];
            edge.set(cx, to.map(|(to, _)| to));
            model.get_mut(&id).unwrap().edges[usize::from(field)] = to.map(|(_, to)| to);
        }
        Op::SetBlob { from, to } => {
            let Some(from) = pick(from, objects.len()) else {
                return;
            };
            let to = match to {
                Some(to) => match pick(to, blobs.len()) {
                    Some(to) => Some(blobs[to]),
                    None => return,
                },
                None => None,
            };
            let (object, id) = objects[from];
            object.blob.set(cx, to.map(|(to, _)| to));
            model.get_mut(&id).unwrap().to_blob = to.map(|(_, to)| to);
        }
        Op::SetBack { blob, to } => {
            let Some(blob) = pick(blob, blobs.len()) else {
                return;
            };
            let to = match to {
                Some(to) => match pick(to, objects.len()) {
                    Some(to) => Some(objects[to]),
                    None => return,
                },
                None => None,
            };
            let (blob, id) = blobs[blob];
            blob.back.set(cx, to.map(|(to, _)| to));
            model.get_mut(&id).unwrap().edges[0] = to.map(|(_, to)| to);
        }
        Op::Push { list, to } => {
            let (Some(list), Some(to)) = (pick(list, objects.len()), pick(to, objects.len()))
            else {
                return;
            };
            let ((object, id), (to, to_id)) = (objects[list], objects[to]);
            object.list.get(cx).unwrap().push(cx, to);
            model.get_mut(&id).unwrap().list.push(to_id);
        }
        Op::Remove { list, at } => {
            let Some(list) = pick(list, objects.len()) else {
                return;
            };
            let (object, id) = objects[list];
            let entries = &mut model.get_mut(&id).unwrap().list;
            let Some(at) = pick(at, entries.len()) else {
                return;
            };
            let removed = object.list.get(cx).unwrap().remove(cx, at);
            assert_eq!(removed.id.get(), entries.remove(at));
        }
        Op::Root { object } => {
            let Some(at) = pick(object, handles.len()) else {
                return;
            };
            let root = match handles[at] {
                Handle::Obj(object) => (Root::Obj(Persistent::new(cx, object)), object.id.get()),
                Handle::Blob(blob) => (Root::Blob(Persistent::new(cx, blob)), blob.id.get()),
            };
            roots.push(root);
        }
        Op::Unroot { root } => {
            if let Some(at) = pick(root, roots.len()) {
                roots.swap_remove(at);
            }
        }
        Op::SetWeak { from, to } => {
            let Some(from) = pick(from, objects.len()) else {
                return;
            };
            let to = match to {
                Some(to) => match pick(to, objects.len()) {
                    Some(to) => Some(objects[to]),
                    None => return,
                },
                None => None,
            };
            let (object, id) = objects[from];
            object.weak.set(cx, to.map(|(to, _)| to));
            model.get_mut(&id).unwrap().weak = to.map(|(_, to)| to);
        }
        Op::ReadWeak { from } => {
            let Some(from) = pick(from, objects.len()) else {
                return;
            };
            let (object, id) = objects[from];
            let expected = model[&id].weak;
            if state.weak_read(
                object.weak.get(cx),
                expected,
                &format!("object {id}'s weak edge"),
            ) {
                state.model.get_mut(&id).unwrap().weak = None;
            }
        }
        Op::SetPair { from, key, value } => {
            let Some(from) = pick(from, objects.len()) else {
                return;
            };
            let pick_object = |at: Option<u16>| match at {
                Some(at) => pick(at, objects.len()).map(|at| Some(objects[at])),
                None => Some(None),
            };
            let (Some(key), Some(value)) = (pick_object(key), pick_object(value)) else {
                return;
            };
            let (object, id) = objects[from];
            object
                .pair
                .set(cx, key.map(|(key, _)| key), value.map(|(value, _)| value));
            model.get_mut(&id).unwrap().pair =
                key.map(|(_, key)| (key, value.map(|(_, value)| value)));
        }
        Op::ReadPair { from } => {
            let Some(from) = pick(from, objects.len()) else {
                return;
            };
            let (object, id) = objects[from];
            let expected = model[&id].pair;
            let key = object.pair.key(cx);
            let value = object.pair.value(cx);
            let what = format!("object {id}'s ephemeron");
            if state.weak_read(key, expected.map(|(key, _)| key), &format!("{what}'s key")) {
                assert!(value.is_none(), "{what}'s value outlives its key");
                state.model.get_mut(&id).unwrap().pair = None;
            } else if key.is_some() {
                // A live key's value is the model's.
                let expected = expected.and_then(|(_, value)| value);
                assert_eq!(
                    value.map(|value| value.id.get()),
                    expected,
                    "{what}'s value"
                );
                state.weak_read(value, expected, &format!("{what}'s value"));
            }
        }
        Op::NewId { object } => {
            if let Some(at) = pick(object, handles.len()) {
                let (id, model_id, blob) = match handles[at] {
                    Handle::Obj(object) => (cx.new_id(object), object.id.get(), false),
                    Handle::Blob(blob) => (cx.new_id(blob), blob.id.get(), true),
                };
                state.ids.push((id, Some(model_id), blob));
            }
        }
        Op::ById { id } => {
            let Some(at) = pick(id, state.ids.len()) else {
                return;
            };
            let (id, expected, blob) = state.ids[at];
            // The other type finds nothing.
            let (got, other) = if blob {
                (
                    cx.by_id::<Blob>(id).map(Handle::Blob),
                    cx.by_id::<Obj>(id).is_some(),
                )
            } else {
                (
                    cx.by_id::<Obj>(id).map(Handle::Obj),
                    cx.by_id::<Blob>(id).is_some(),
                )
            };
            assert!(!other, "id {} finds an object of another type", id.get());
            if state.weak_read_handle(got, expected, &format!("id {}", id.get())) {
                state.ids[at].1 = None;
            }
        }
        Op::WeakRoot { object } => {
            if let Some(&(object, id)) = pick(object, objects.len()).map(|at| &objects[at]) {
                state
                    .weak_roots
                    .push((WeakPersistent::new(cx, object), Some(id)));
            }
        }
        Op::WeakUnroot { root } => {
            if let Some(at) = pick(root, state.weak_roots.len()) {
                state.weak_roots.swap_remove(at);
            }
        }
        Op::ReadWeakRoot { root } => {
            let Some(at) = pick(root, state.weak_roots.len()) else {
                return;
            };
            let (root, expected) = &state.weak_roots[at];
            let got = root.get(cx);
            let expected = *expected;
            if state.weak_read(got, expected, &format!("weak root {at}")) {
                state.weak_roots[at].1 = None;
            }
        }
        Op::Wrapper { object } => {
            let Some(at) = pick(object, objects.len()) else {
                return;
            };
            let (object, id) = objects[at];
            let engine = &mut *state.engine;
            // SAFETY: the edge's 8 aligned bytes, the engine's to write
            // (here, on the heap's thread), which the marker reads
            // atomically.
            let edge = unsafe { AtomicU64::from_ptr(object.wrapper.as_ptr()) };
            match edge.load(Ordering::Relaxed) {
                0 => {
                    let w = engine.wrappers.len();
                    engine.wrappers.push(Wrapper {
                        object: id,
                        address: cx.engine_address(object),
                        held: true,
                        link: None,
                        // Black, as V8 allocates while it marks: the object
                        // is marked already (black, or in the heap's
                        // snapshot), so not handed over.
                        marked: engine.marking.is_some(),
                        alive: true,
                    });
                    edge.store(u64::try_from(w).unwrap() + 1, Ordering::Relaxed);
                }
                value => {
                    let w = usize::try_from(value - 1).unwrap();
                    assert!(
                        engine.wrappers[w].alive,
                        "object {id} lives, but its wrapper {w} died"
                    );
                    engine.wrappers[w].held = true;
                }
            }
        }
        Op::Release { wrapper } => {
            let held = state.engine.held_wrappers();
            if let Some(at) = pick(wrapper, held.len()) {
                state.engine.wrappers[held[at]].held = false;
            }
        }
        Op::Unwrap { wrapper } => {
            let held = state.engine.held_wrappers();
            let Some(at) = pick(wrapper, held.len()) else {
                return;
            };
            let wrapper = &state.engine.wrappers[held[at]];
            // SAFETY: a live wrapper's object, which it keeps alive, of this
            // heap.
            let object = unsafe { cx.from_engine::<Obj>(wrapper.address) };
            assert_eq!(object.id.get(), wrapper.object, "a wrapper's object");
            handles.push(Handle::Obj(object));
        }
        Op::Link { from, to } => {
            let held = state.engine.held_wrappers();
            let (Some(from), Some(to)) = (pick(from, held.len()), pick(to, held.len())) else {
                return;
            };
            let (from, to) = (held[from], held[to]);
            let engine = &mut *state.engine;
            engine.wrappers[from].link = Some(to);
            // The engine's barrier (V8's, Dijkstra's): a marked wrapper's
            // new target is marked.
            if engine.marking.is_some() && engine.wrappers[from].marked {
                engine.mark(to);
            }
            let (object, to) = (engine.wrappers[from].object, engine.wrappers[to].object);
            model.get_mut(&object).unwrap().js = Some(to);
        }
        Op::Unlink { from } => {
            let held = state.engine.held_wrappers();
            if let Some(at) = pick(from, held.len()) {
                let wrapper = &mut state.engine.wrappers[held[at]];
                wrapper.link = None;
                model.get_mut(&wrapper.object).unwrap().js = None;
            }
        }
        Op::Safepoint | Op::MarkStep { .. } | Op::Collect | Op::Scavenge => {
            unreachable!("they end the operation")
        }
    }
}
