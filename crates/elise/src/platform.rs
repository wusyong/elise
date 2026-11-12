//! Where the GC's parallel work runs (cppgc's `cppgc::Platform`, V8's
//! `v8::Platform::PostJob`): the heap hands a `Job` to its `Platform`,
//! which runs it on the calling thread and on as many workers as the job
//! asks for, and returns once every run has; or posts one to run on
//! workers alone while the heap's thread goes on (concurrent marking),
//! which a `JobHandle` cancels. The GC starts no threads of its own: an
//! embedder can give it the threads its other work uses (V8's, for a JS
//! document), and tests can give it shuttle's or loom's. `ThreadPool` is
//! the platform a heap gets when it is given none.

use std::any::Any;
use std::panic::{self, AssertUnwindSafe};

use crate::sync::{self, Arc, AtomicBool, Condvar, Mutex, Ordering, thread};

/// Runs parallel work.
pub trait Platform: Send + Sync {
    /// Runs `job` on the calling thread, and on workers while
    /// `job.max_concurrency` asks for more; returns once every run of it
    /// has. A panic in any run is resumed here, after the others return.
    fn run(&self, job: &dyn Job);

    /// Posts `job` to run on workers while `job.max_concurrency` asks for
    /// them, and returns at once (V8's `PostJob`): its runs go on until it
    /// is cancelled through the handle, or yield (`should_yield`) while
    /// the platform runs another job. `None` for a platform with no
    /// workers, which can't run one alongside the caller.
    fn post_job(&self, job: std::sync::Arc<dyn Job + Send>) -> Option<Box<dyn JobHandle>> {
        let _ = job;
        None
    }

    /// Whether `post_job` runs jobs: the heap marks alongside the mutator
    /// only on such a platform.
    fn posts_jobs(&self) -> bool {
        false
    }

    /// The embedder's trace, if it keeps one (cppgc's
    /// `Platform::GetTracingController`): asked once, as a heap is made.
    /// None by default.
    fn tracing_controller(&self) -> &'static dyn TracingController {
        &NoTracing
    }
}

/// The embedder's trace (cppgc's `TracingController`): the GC's phases
/// (`Gc.Pause`, `Gc.Mark`, `Gc.Sweep`, `Gc.Finalize`) begin and end in it,
/// and so does a job's run on a worker (`Gc.ConcurrentMark`,
/// `Gc.ConcurrentSweep`, `Gc.ParallelScavenge`, `Gc.Discard`), as cppgc
/// traces its concurrent marking and sweeping. `'static`, as Chromium's
/// `TraceLog` and the category flags its trace macros read: a job that
/// traces holds it, and owns nothing of its platform's by it (the platform
/// owns the job it runs, which may outlive the heap).
pub trait TracingController: Sync {
    /// Whether the trace records now: a flag the heap reads (relaxed)
    /// before each `begin`, which it calls only while it is set, so that a
    /// phase costs a load and a branch with no trace (cppgc's
    /// `GetCategoryGroupEnabled`, a pointer its trace macros read). Asked
    /// once, as a heap is made.
    fn enabled(&self) -> &std::sync::atomic::AtomicBool;

    /// A phase named `name` begins, on the calling thread.
    fn begin(&self, name: &'static str);

    /// The phase begun last on the calling thread ends.
    fn end(&self);
}

/// `Platform::tracing_controller`'s default: a trace that never records.
struct NoTracing;

/// `NoTracing`'s flag, never set.
static NOT_TRACING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

impl TracingController for NoTracing {
    fn enabled(&self) -> &std::sync::atomic::AtomicBool {
        &NOT_TRACING
    }

    fn begin(&self, name: &'static str) {
        let _ = name;
    }

    fn end(&self) {}
}

/// A heap's trace: its platform's `TracingController`, with the flag it
/// gave (read before each phase without a virtual call).
#[derive(Clone, Copy)]
pub(crate) struct Tracer {
    enabled: &'static std::sync::atomic::AtomicBool,
    controller: &'static dyn TracingController,
}

impl Tracer {
    /// `platform`'s trace.
    pub(crate) fn of(platform: &dyn Platform) -> Self {
        let controller = platform.tracing_controller();
        Self {
            enabled: controller.enabled(),
            controller,
        }
    }

    /// The trace records now.
    #[inline]
    fn records(self) -> bool {
        self.enabled.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// A phase named `name`, begun if the trace records.
    #[inline]
    pub(crate) fn phase(self, name: &'static str) -> Phase {
        if !self.records() {
            return Phase(None);
        }
        self.controller.begin(name);
        Phase(Some(self.controller))
    }

    /// `run`, a job's run, as a phase named `name` on a worker, if the
    /// trace records (cppgc's `StatsCollector::EnabledConcurrentScope`,
    /// which traces `CppGC.ConcurrentMark` and the like): begun before it,
    /// ended after it, a panicking run's too. Nothing on the joining thread:
    /// the phase it runs in (`Gc.Mark`, `Gc.Sweep`, `Gc.Scavenge`) is its
    /// span already.
    pub(crate) fn in_worker_span<R>(
        self,
        delegate: &dyn JobDelegate,
        name: &'static str,
        run: impl FnOnce() -> R,
    ) -> R {
        let _span = if self.records() && !delegate.is_joining_thread() {
            self.controller.begin(name);
            WorkerSpan(Some(self.controller))
        } else {
            WorkerSpan(None)
        };
        run()
    }
}

/// A phase of the trace (`Tracer::phase`): `end` ends it if it was begun,
/// whatever the flag says by then, so the begins and ends pair.
#[must_use = "a phase ends with `end`"]
pub(crate) struct Phase(Option<&'static dyn TracingController>);

impl Phase {
    #[inline]
    pub(crate) fn end(self) {
        if let Some(controller) = self.0 {
            controller.end();
        }
    }
}

/// Parallel work (V8's `JobTask`).
pub trait Job: Sync {
    /// Works until there is nothing left to take. On the calling thread
    /// (`delegate.is_joining_thread()`), returns only once the whole job
    /// is done; on a worker, may return before (and run again).
    fn run(&self, delegate: &dyn JobDelegate);

    /// How many workers could work now, `active` of them running it
    /// already (V8's `GetMaxConcurrency`): it must not lock what `run`
    /// locks, as the platform asks it under its own lock.
    fn max_concurrency(&self, active: usize) -> usize;
}

/// What a run of a `Job` tells its platform (V8's `JobDelegate`).
pub trait JobDelegate {
    /// `max_concurrency` grew: more workers may help.
    fn notify_concurrency_increase(&self);

    fn is_joining_thread(&self) -> bool;

    /// The run should return soon, its work left for later (V8's
    /// `ShouldYield`): its job was cancelled, or the platform runs another.
    fn should_yield(&self) -> bool {
        false
    }
}

/// A posted job (V8's `JobHandle`).
pub trait JobHandle: Send {
    /// `max_concurrency` grew: more workers may help.
    fn notify_concurrency_increase(&self);

    /// Stops the job: its runs see `should_yield`, no new one starts, and
    /// this returns once every run has (V8's `Cancel`). A panic in a run
    /// is resumed here.
    fn cancel(&self);
}

/// `Tracer::in_worker_span`'s span: ended as it drops.
struct WorkerSpan(Option<&'static dyn TracingController>);

impl Drop for WorkerSpan {
    fn drop(&mut self) {
        if let Some(controller) = self.0 {
            controller.end();
        }
    }
}

/// A platform with no workers: a job runs on the calling thread only.
#[derive(Debug, Default)]
pub struct InlinePlatform;

impl Platform for InlinePlatform {
    fn run(&self, job: &dyn Job) {
        job.run(&Joining(None));
    }
}

/// The delegate of the calling thread's run: its pool, if any.
struct Joining<'a>(Option<&'a ThreadPool>);

impl JobDelegate for Joining<'_> {
    fn notify_concurrency_increase(&self) {
        if let Some(pool) = self.0 {
            wake(&pool.shared);
        }
    }

    fn is_joining_thread(&self) -> bool {
        true
    }
}

/// A worker's delegate.
struct Worker<'a>(&'a Shared);

impl JobDelegate for Worker<'_> {
    fn notify_concurrency_increase(&self) {
        wake(self.0);
    }

    fn is_joining_thread(&self) -> bool {
        false
    }
}

/// A set of worker threads, parked between jobs, running one job at a time:
/// a job started while another runs runs on its calling thread only.
pub struct ThreadPool {
    shared: Arc<Shared>,
    workers: Vec<thread::JoinHandle<()>>,
}

struct Shared {
    state: Mutex<State>,
    /// Workers wait on it for a job that wants them.
    wake: Condvar,
    /// The joining thread waits on it for the workers' runs to return.
    done: Condvar,
    /// A job runs through `run`: no posted job's run starts, and those
    /// running yield.
    running: AtomicBool,
}

/// A posted job of a pool's.
struct Background {
    id: u64,
    /// std's `Arc`: loom's can't hold a trait object.
    job: std::sync::Arc<dyn Job + Send>,
    /// Workers running it.
    active: usize,
    /// Its runs yield, and none starts.
    cancelled: Arc<AtomicBool>,
    /// The first panic of a run.
    panic: Option<Box<dyn Any + Send>>,
}

#[derive(Default)]
struct State {
    /// The jobs posted, running alongside whatever else.
    background: Vec<Background>,
    next_background: u64,
    /// The job running, its lifetime erased: `run` waits for every worker's
    /// run of it to return before it clears it, and returns.
    job: Option<&'static dyn Job>,
    /// Once the joining thread's run has returned: no worker starts it.
    closed: bool,
    /// Workers running it.
    active: usize,
    /// The first panic of a worker's run.
    panic: Option<Box<dyn Any + Send>>,
    shutdown: bool,
}

impl ThreadPool {
    /// A pool of `workers` threads.
    pub fn new(workers: usize) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            done: Condvar::new(),
            running: AtomicBool::new(false),
        });
        let workers = (0..workers)
            .map(|_| {
                let shared = shared.clone();
                thread::spawn(move || work(&shared))
            })
            .collect();
        Self { shared, workers }
    }
}

/// Wakes the workers, to look at the job's `max_concurrency` again. The
/// lock orders this after a worker that found no reason to run has started
/// to wait: else the wakeup could fall between its look and its wait.
fn wake(shared: &Shared) {
    drop(sync::lock(&shared.state));
    shared.wake.notify_one();
}

impl Platform for ThreadPool {
    fn post_job(&self, job: std::sync::Arc<dyn Job + Send>) -> Option<Box<dyn JobHandle>> {
        Some(self.post(job))
    }

    fn posts_jobs(&self) -> bool {
        !self.workers.is_empty()
    }

    fn run(&self, job: &dyn Job) {
        {
            let mut state = sync::lock(&self.shared.state);
            if state.job.is_some() {
                drop(state);
                job.run(&Joining(None));
                return;
            }
            // SAFETY: only the lifetime changes. Workers reach the job only
            // through `state.job`, which is cleared below once no worker
            // runs it and none can start (`closed`), before this returns:
            // no reference to it outlives this call.
            let job: &'static dyn Job =
                unsafe { std::mem::transmute::<&dyn Job, &'static dyn Job>(job) };
            state.job = Some(job);
            state.closed = false;
            self.shared.running.store(true, Ordering::Relaxed);
        }
        // A worker only for a job that wants one with none running yet
        // (`max_concurrency` counts workers, not the caller): most GCs
        // start with nothing for one to take.
        if job.max_concurrency(0) > 0 {
            wake(&self.shared);
        }
        let joined = panic::catch_unwind(AssertUnwindSafe(|| job.run(&Joining(Some(self)))));
        let mut state = sync::lock(&self.shared.state);
        state.closed = true;
        while state.active > 0 {
            state = sync::wait(&self.shared.done, state);
        }
        state.job = None;
        self.shared.running.store(false, Ordering::Relaxed);
        let worker_panic = state.panic.take();
        // The posted jobs' runs yielded: the workers may take them again,
        // if they want any (waking all of them after every run doubled
        // small scavenges).
        let wanted = state.background.iter().any(|posted| {
            !posted.cancelled.load(Ordering::Relaxed)
                && posted.active < posted.job.max_concurrency(posted.active)
        });
        drop(state);
        if wanted {
            self.shared.wake.notify_all();
        }
        if let Err(payload) = joined {
            panic::resume_unwind(payload);
        }
        if let Some(payload) = worker_panic {
            panic::resume_unwind(payload);
        }
    }
}

impl ThreadPool {
    fn post(&self, job: std::sync::Arc<dyn Job + Send>) -> Box<dyn JobHandle> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut state = sync::lock(&self.shared.state);
        let id = state.next_background;
        state.next_background = id.wrapping_add(1);
        // As many workers as the job wants now (V8's `PostJob`), none for
        // none (as `run`'s start).
        let wanted = job.max_concurrency(0).min(self.workers.len());
        state.background.push(Background {
            id,
            job,
            active: 0,
            cancelled: cancelled.clone(),
            panic: None,
        });
        drop(state);
        for _ in 0..wanted {
            self.shared.wake.notify_one();
        }
        Box::new(PostedJob {
            shared: self.shared.clone(),
            id,
            cancelled,
        })
    }
}

/// A pool's handle on a posted job.
struct PostedJob {
    shared: Arc<Shared>,
    id: u64,
    cancelled: Arc<AtomicBool>,
}

impl JobHandle for PostedJob {
    fn notify_concurrency_increase(&self) {
        wake(&self.shared);
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        let mut state = sync::lock(&self.shared.state);
        let panic = loop {
            let Some(at) = state.background.iter().position(|job| job.id == self.id) else {
                return;
            };
            if state.background[at].active == 0 {
                break state.background.swap_remove(at).panic;
            }
            state = sync::wait(&self.shared.done, state);
        };
        drop(state);
        if let Some(payload) = panic {
            panic::resume_unwind(payload);
        }
    }
}

impl Drop for PostedJob {
    /// A job isn't left running past its handle (it may borrow what the
    /// handle's owner frees next). While unwinding, it is only told to
    /// stop: a panic in one of its runs would abort.
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.cancelled.store(true, Ordering::Relaxed);
        } else {
            self.cancel();
        }
    }
}

/// A posted job's run's delegate.
struct BackgroundWorker<'a> {
    shared: &'a Shared,
    cancelled: &'a AtomicBool,
}

impl JobDelegate for BackgroundWorker<'_> {
    fn notify_concurrency_increase(&self) {
        wake(self.shared);
    }

    fn is_joining_thread(&self) -> bool {
        false
    }

    fn should_yield(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed) || self.shared.running.load(Ordering::Relaxed)
    }
}

impl Drop for ThreadPool {
    fn drop(&mut self) {
        let mut state = sync::lock(&self.shared.state);
        state.shutdown = true;
        // Posted jobs left running stop, or the workers would never return.
        for posted in &state.background {
            posted.cancelled.store(true, Ordering::Relaxed);
        }
        drop(state);
        self.shared.wake.notify_all();
        for worker in self.workers.drain(..) {
            // A worker's panics are caught; joining fails only if one
            // panicked outside a job, which the pool's own code doesn't.
            let _ = worker.join();
        }
    }
}

/// A worker: runs the job while it wants more workers than run it; with
/// none running through `run`, a posted job that wants them.
fn work(shared: &Shared) {
    let mut state = sync::lock(&shared.state);
    loop {
        if state.shutdown {
            return;
        }
        let job = state
            .job
            .filter(|job| !state.closed && state.active < job.max_concurrency(state.active));
        let Some(job) = job else {
            if state.job.is_none()
                && let Some(at) = state.background.iter().position(|posted| {
                    !posted.cancelled.load(Ordering::Relaxed)
                        && posted.active < posted.job.max_concurrency(posted.active)
                })
            {
                state = run_posted(shared, state, at);
                continue;
            }
            state = sync::wait(&shared.wake, state);
            continue;
        };
        state.active = state.active.saturating_add(1);
        drop(state);
        let ran = panic::catch_unwind(AssertUnwindSafe(|| job.run(&Worker(shared))));
        state = sync::lock(&shared.state);
        state.active = state.active.saturating_sub(1);
        if let Err(payload) = ran {
            state.panic.get_or_insert(payload);
        }
        shared.done.notify_all();
    }
}

/// Runs the posted job at `at` on this worker, then finds it again (others
/// may have come and gone) to count the run back.
fn run_posted<'a>(
    shared: &'a Shared,
    mut state: sync::MutexGuard<'a, State>,
    at: usize,
) -> sync::MutexGuard<'a, State> {
    let posted = &mut state.background[at];
    posted.active = posted.active.saturating_add(1);
    let (id, job, cancelled) = (posted.id, posted.job.clone(), posted.cancelled.clone());
    drop(state);
    let ran = panic::catch_unwind(AssertUnwindSafe(|| {
        job.run(&BackgroundWorker {
            shared,
            cancelled: &cancelled,
        });
    }));
    let mut state = sync::lock(&shared.state);
    if let Some(posted) = state.background.iter_mut().find(|posted| posted.id == id) {
        posted.active = posted.active.saturating_sub(1);
        if let Err(payload) = ran {
            posted.panic.get_or_insert(payload);
            posted.cancelled.store(true, Ordering::Relaxed);
        }
    }
    shared.done.notify_all();
    state
}

/// The platform of heaps given none: one pool for the process, with a
/// worker for each core but the calling thread's (V8's default).
#[cfg(not(any(loom, shuttle, miri)))]
pub(crate) fn default_platform() -> std::sync::Arc<dyn Platform> {
    static POOL: std::sync::OnceLock<std::sync::Arc<ThreadPool>> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let cores = std::thread::available_parallelism().map_or(1, usize::from);
        std::sync::Arc::new(ThreadPool::new(cores.saturating_sub(1)))
    })
    .clone()
}

/// Under loom and shuttle, statics outlive each run of a model; under Miri,
/// the process's pool would still run when the program ends, which it
/// reports (pools of their own, joined when dropped, test the threads): a
/// heap given no platform runs its jobs inline.
#[cfg(any(loom, shuttle, miri))]
pub(crate) fn default_platform() -> std::sync::Arc<dyn Platform> {
    std::sync::Arc::new(InlinePlatform)
}

#[cfg(all(test, not(any(loom, shuttle))))]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Counts its runs, asking for `want` workers until `stop`.
    struct Counting {
        want: usize,
        runs: AtomicUsize,
        workers: AtomicUsize,
        panic_on_worker: bool,
    }

    impl Job for Counting {
        #[expect(clippy::disallowed_methods, reason = "tests' counters")]
        fn run(&self, delegate: &dyn JobDelegate) {
            self.runs.fetch_add(1, Ordering::Relaxed);
            if !delegate.is_joining_thread() {
                self.workers.fetch_add(1, Ordering::Relaxed);
                assert!(!self.panic_on_worker, "a worker's panic");
                return;
            }
            // The joining thread waits for a worker, if it asked for one:
            // a while, not forever, so that a worker never woken fails the
            // test rather than hangs it.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while self.want > 0 && self.workers.load(Ordering::Relaxed) == 0 {
                assert!(std::time::Instant::now() < deadline, "no worker came");
                std::thread::yield_now();
            }
        }

        fn max_concurrency(&self, active: usize) -> usize {
            // One run each: no more once a worker has run.
            if self.workers.load(Ordering::Relaxed) > 0 {
                active
            } else {
                self.want
            }
        }
    }

    fn counting(want: usize) -> Counting {
        Counting {
            want,
            runs: AtomicUsize::new(0),
            workers: AtomicUsize::new(0),
            panic_on_worker: false,
        }
    }

    #[test]
    fn a_job_runs_on_the_caller_and_the_workers_it_asks_for() {
        let pool = ThreadPool::new(3);
        // The workers asleep first: the job's start has to wake one.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let job = counting(1);
        pool.run(&job);
        assert!(job.workers.load(Ordering::Relaxed) >= 1);
        let job = counting(0);
        pool.run(&job);
        assert_eq!(
            job.runs.load(Ordering::Relaxed),
            1,
            "no worker when none is asked for"
        );
    }

    #[test]
    fn inline_runs_on_the_caller_only() {
        let job = counting(0);
        InlinePlatform.run(&job);
        assert_eq!(job.runs.load(Ordering::Relaxed), 1);
    }

    /// A worker still inside its run when the joining thread's returns.
    struct Slow {
        started: AtomicUsize,
        inside: AtomicUsize,
    }

    impl Job for Slow {
        #[expect(clippy::disallowed_methods, reason = "tests' counters")]
        fn run(&self, delegate: &dyn JobDelegate) {
            if delegate.is_joining_thread() {
                while self.started.load(Ordering::SeqCst) == 0 {
                    std::thread::yield_now();
                }
                return;
            }
            self.inside.fetch_add(1, Ordering::SeqCst);
            self.started.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(50));
            self.inside.fetch_sub(1, Ordering::SeqCst);
        }

        fn max_concurrency(&self, active: usize) -> usize {
            if self.started.load(Ordering::SeqCst) > 0 {
                active
            } else {
                1
            }
        }
    }

    /// `run` returns only once its workers' runs have: the job may be gone
    /// right after.
    #[test]
    fn run_waits_for_its_workers() {
        let pool = ThreadPool::new(2);
        let job = Slow {
            started: AtomicUsize::new(0),
            inside: AtomicUsize::new(0),
        };
        pool.run(&job);
        assert_eq!(
            job.inside.load(Ordering::SeqCst),
            0,
            "a worker is still in the job"
        );
    }

    #[test]
    #[should_panic = "a worker's panic"]
    fn a_workers_panic_reaches_the_caller() {
        let pool = ThreadPool::new(2);
        let mut job = counting(1);
        job.panic_on_worker = true;
        pool.run(&job);
    }

    /// A posted job: each run spins until it should yield, counting its
    /// runs and yields; it wants `want` workers until `stop`.
    struct Spinning {
        want: usize,
        stop: std::sync::atomic::AtomicBool,
        runs: AtomicUsize,
        yields: AtomicUsize,
        inside: AtomicUsize,
        panic: bool,
    }

    impl Job for Spinning {
        #[expect(clippy::disallowed_methods, reason = "tests' counters")]
        fn run(&self, delegate: &dyn JobDelegate) {
            assert!(!delegate.is_joining_thread(), "a posted job has no caller");
            self.runs.fetch_add(1, Ordering::SeqCst);
            assert!(!self.panic, "a posted job's panic");
            self.inside.fetch_add(1, Ordering::SeqCst);
            while !delegate.should_yield() && !self.stop.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            self.yields.fetch_add(1, Ordering::SeqCst);
            self.inside.fetch_sub(1, Ordering::SeqCst);
        }

        fn max_concurrency(&self, active: usize) -> usize {
            if self.stop.load(Ordering::SeqCst) {
                0
            } else {
                self.want.max(active)
            }
        }
    }

    fn spinning(want: usize) -> std::sync::Arc<Spinning> {
        std::sync::Arc::new(Spinning {
            want,
            stop: std::sync::atomic::AtomicBool::new(false),
            runs: AtomicUsize::new(0),
            yields: AtomicUsize::new(0),
            inside: AtomicUsize::new(0),
            panic: false,
        })
    }

    /// Waits until `done`, a while, not forever.
    fn wait_until(what: &str, done: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "{what} never came");
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_posted_job_runs_on_workers_until_cancelled() {
        let pool = ThreadPool::new(2);
        let job = spinning(2);
        let handle = pool.post_job(job.clone()).expect("a pool runs posted jobs");
        // The caller goes on while two workers run it.
        wait_until("two runs", || job.inside.load(Ordering::SeqCst) == 2);
        handle.cancel();
        assert_eq!(
            job.inside.load(Ordering::SeqCst),
            0,
            "cancel waits for its runs"
        );
        let runs = job.runs.load(Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(job.runs.load(Ordering::SeqCst), runs, "no run after cancel");
        // A pool's run is free again.
        let next = counting(1);
        pool.run(&next);
        assert!(next.workers.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn a_run_takes_the_workers_from_posted_jobs_and_gives_them_back() {
        let pool = ThreadPool::new(2);
        let posted = spinning(2);
        let handle = pool.post_job(posted.clone()).unwrap();
        wait_until("two runs", || posted.inside.load(Ordering::SeqCst) == 2);
        // The posted job's runs yield, and the run's job gets a worker.
        let job = counting(1);
        pool.run(&job);
        assert!(job.workers.load(Ordering::Relaxed) >= 1);
        // One run yielded at least, for a worker to take the run's job (the
        // other may have seen the run end first).
        assert!(posted.yields.load(Ordering::SeqCst) >= 1);
        // Then the workers go back to the posted job.
        wait_until("runs again", || posted.inside.load(Ordering::SeqCst) == 2);
        drop(handle);
        assert_eq!(
            posted.inside.load(Ordering::SeqCst),
            0,
            "dropping the handle cancels"
        );
    }

    #[test]
    fn two_posted_jobs_share_the_workers() {
        let pool = ThreadPool::new(2);
        let (first, second) = (spinning(1), spinning(1));
        let first_handle = pool.post_job(first.clone()).unwrap();
        let second_handle = pool.post_job(second.clone()).unwrap();
        wait_until("both run", || {
            first.inside.load(Ordering::SeqCst) == 1 && second.inside.load(Ordering::SeqCst) == 1
        });
        second_handle.cancel();
        assert_eq!(
            first.inside.load(Ordering::SeqCst),
            1,
            "only its own runs stop"
        );
        first_handle.cancel();
    }

    #[test]
    #[should_panic = "a posted job's panic"]
    fn a_posted_jobs_panic_reaches_cancel() {
        let pool = ThreadPool::new(1);
        let job = std::sync::Arc::new(Spinning {
            panic: true,
            ..std::sync::Arc::into_inner(spinning(1)).unwrap()
        });
        let handle = pool.post_job(job.clone()).unwrap();
        wait_until("a run", || job.runs.load(Ordering::SeqCst) > 0);
        handle.cancel();
    }

    /// A pool dropped with a posted job still running (its handle kept
    /// past it) stops the job, and its workers return; the handle drops
    /// after the pool.
    #[test]
    fn a_dropped_pool_stops_its_posted_jobs() {
        let pool = ThreadPool::new(2);
        let job = spinning(2);
        let handle = pool.post_job(job.clone()).unwrap();
        wait_until("two runs", || job.inside.load(Ordering::SeqCst) == 2);
        drop(pool);
        assert_eq!(job.inside.load(Ordering::SeqCst), 0);
        drop(handle);
    }

    #[test]
    fn inline_posts_nothing() {
        assert!(InlinePlatform.post_job(spinning(1)).is_none());
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;

    /// A posted job's run: inside until it should yield.
    struct Yielding {
        inside: AtomicUsize,
        stop: AtomicBool,
    }

    impl Job for Yielding {
        fn run(&self, delegate: &dyn JobDelegate) {
            self.inside.fetch_add(1, Ordering::SeqCst);
            while !delegate.should_yield() && !self.stop.load(Ordering::SeqCst) {
                loom::thread::yield_now();
            }
            self.inside.fetch_sub(1, Ordering::SeqCst);
        }

        fn max_concurrency(&self, active: usize) -> usize {
            if self.stop.load(Ordering::SeqCst) {
                0
            } else {
                active.max(1)
            }
        }
    }

    /// A job posted to a pool of one worker, then cancelled, in every
    /// interleaving: cancel returns with none of its runs inside, and
    /// none starts after; the pool still shuts down.
    #[test]
    fn cancel_leaves_no_run_of_a_posted_job() {
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = Some(2);
        builder.check(|| {
            let pool = ThreadPool::new(1);
            let job = std::sync::Arc::new(Yielding {
                inside: AtomicUsize::new(0),
                stop: AtomicBool::new(false),
            });
            let handle = pool.post_job(job.clone()).unwrap();
            handle.cancel();
            assert_eq!(
                job.inside.load(Ordering::SeqCst),
                0,
                "a run outlived cancel"
            );
            job.stop.store(true, Ordering::SeqCst);
            drop(handle);
            drop(pool);
            assert_eq!(
                job.inside.load(Ordering::SeqCst),
                0,
                "a run started after cancel"
            );
        });
    }
}
