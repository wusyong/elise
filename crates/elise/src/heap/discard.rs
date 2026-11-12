//! Free pages' memory given back to the OS on a worker, not in a GC's
//! pause: the system calls take about as long as the bytes they give back
//! (some 6 ms for 350 MiB on an Apple M4), however many pages a call
//! covers. V8 gives back what stayed in its pool 8 s, in a delayed task on
//! a worker (`MemoryPool`'s `ReleasePooledChunksTask`); cppgc keeps its
//! pool but in GCs that reduce memory. Here a page waits a whole GC in the
//! pool, as on a platform with no workers; then the sweep hands it to this
//! job (`PageState::Discarding`, which nothing takes), and a safepoint
//! after the job is done with it lists it as discarded.

use crate::cage::Discarder;
use crate::page::{self, PAGE_BYTES};
use crate::platform::{Job, JobDelegate, Tracer};
use crate::sync::{self, AtomicUsize, Mutex, Ordering};

/// The pages to discard, and the job that does.
pub(super) struct Discards {
    discarder: Discarder,
    /// The heap's trace: a run on a worker is a phase of it.
    tracer: Tracer,
    queue: Mutex<Queue>,
    /// `queue.todo`'s length, for `max_concurrency`, which mustn't lock.
    left: AtomicUsize,
    /// `queue.done`'s length: a safepoint locks the queue only if some are.
    done: AtomicUsize,
}

#[derive(Default)]
struct Queue {
    todo: Vec<usize>,
    /// The worker's page, while it discards it.
    busy: Option<usize>,
    done: Vec<usize>,
}

impl Discards {
    pub(super) fn new(discarder: Discarder, tracer: Tracer) -> Self {
        Self {
            discarder,
            tracer,
            queue: Mutex::default(),
            left: AtomicUsize::new(0),
            done: AtomicUsize::new(0),
        }
    }

    /// Hands `pages` to the job.
    pub(super) fn push(&self, pages: &[usize]) {
        let mut queue = sync::lock(&self.queue);
        queue.todo.extend_from_slice(pages);
        self.left.store(queue.todo.len(), Ordering::Relaxed);
    }

    /// Whether pages were discarded since the last `take_done`.
    #[inline]
    pub(super) fn has_done(&self) -> bool {
        self.done.load(Ordering::Relaxed) != 0
    }

    /// The pages discarded since the last call.
    pub(super) fn take_done(&self) -> Vec<usize> {
        if self.done.load(Ordering::Relaxed) == 0 {
            return Vec::new();
        }
        let mut queue = sync::lock(&self.queue);
        self.done.store(0, Ordering::Relaxed);
        std::mem::take(&mut queue.done)
    }

    /// Whether page `number` is to be discarded, or was and isn't taken
    /// back yet (the heap verifier's check of its `Discarding` pages).
    pub(super) fn holds(&self, number: usize) -> bool {
        let queue = sync::lock(&self.queue);
        queue.todo.contains(&number) || queue.busy == Some(number) || queue.done.contains(&number)
    }
}

impl Job for Discards {
    fn run(&self, delegate: &dyn JobDelegate) {
        self.tracer.in_worker_span(delegate, "Gc.Discard", || {
            while !delegate.should_yield() {
                let number = {
                    let mut queue = sync::lock(&self.queue);
                    let Some(number) = queue.todo.pop() else {
                        return;
                    };
                    self.left.store(queue.todo.len(), Ordering::Relaxed);
                    queue.busy = Some(number);
                    number
                };
                // SAFETY: the heap cancels this job before its cage goes; the
                // page is `Discarding`, so it holds no object, and no thread
                // takes (and commits) it until a safepoint finds it done.
                unsafe { self.discarder.discard(page::start_of(number), PAGE_BYTES) };
                let mut queue = sync::lock(&self.queue);
                queue.busy = None;
                queue.done.push(number);
                self.done.store(queue.done.len(), Ordering::Relaxed);
            }
        })
    }

    fn max_concurrency(&self, _: usize) -> usize {
        // One worker: the OS orders the calls on a process's mappings.
        usize::from(self.left.load(Ordering::Relaxed) > 0)
    }
}
