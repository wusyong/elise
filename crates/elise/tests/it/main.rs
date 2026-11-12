//! `elise`'s integration tests, one binary. `compile_fail` (trybuild) is a
//! binary of its own. Built with `--cfg shuttle`, only the shuttle tests
//! (the heap's threads are shuttle's then); with `--cfg loom`, none (loom's
//! models are unit tests: they need the crate's private worklist).

#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    reason = "tests"
)]

#[cfg(not(any(loom, shuttle)))]
mod alloc;
#[cfg(not(any(loom, shuttle)))]
mod concurrent;
#[cfg(not(any(loom, shuttle)))]
mod engine;
#[cfg(not(any(loom, shuttle)))]
mod ephemeron;
#[cfg(not(any(loom, shuttle)))]
mod fragmentation;
#[cfg(not(any(loom, shuttle)))]
mod fuzz;
#[cfg(not(any(loom, shuttle)))]
mod heap;
#[cfg(not(any(loom, shuttle)))]
mod ids;
#[cfg(not(any(loom, shuttle)))]
mod js;
#[cfg(not(any(loom, shuttle)))]
mod model;
#[cfg(not(any(loom, shuttle)))]
pub mod ops;
#[cfg(not(any(loom, shuttle)))]
mod parallel;
#[cfg(not(any(loom, shuttle)))]
mod raw;
#[cfg(shuttle)]
mod shuttle;
#[cfg(not(any(loom, shuttle)))]
mod sites;
#[cfg(not(any(loom, shuttle)))]
mod sweep;
#[cfg(not(any(loom, shuttle)))]
mod token;
#[cfg(not(any(loom, shuttle)))]
mod walk;
#[cfg(not(any(loom, shuttle)))]
mod weak;
#[cfg(not(any(loom, shuttle)))]
mod young;

/// A test's trace (a platform's `TracingController`): it keeps the events
/// it is sent, `begin <name>` and `end`, each with the thread that sent it
/// and when. A `static` of the test's, as the trait asks for `'static`.
pub struct Recorder {
    /// Whether it records.
    pub on: std::sync::atomic::AtomicBool,
    /// Its flag is cleared as this phase begins.
    stops_in: Option<&'static str>,
    events: std::sync::Mutex<Vec<(std::thread::ThreadId, String, std::time::Instant)>>,
}

impl Recorder {
    /// A trace that records if `on`.
    pub const fn new(on: bool) -> Self {
        Self::with(on, None)
    }

    /// A trace that records until phase `name` begins.
    pub const fn stopping_in(name: &'static str) -> Self {
        Self::with(true, Some(name))
    }

    const fn with(on: bool, stops_in: Option<&'static str>) -> Self {
        Self {
            on: std::sync::atomic::AtomicBool::new(on),
            stops_in,
            events: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// The events.
    pub fn names(&self) -> Vec<String> {
        let events = self.events.lock().unwrap();
        events.iter().map(|(_, event, _)| event.clone()).collect()
    }

    /// The events, each with the thread that sent it.
    pub fn by_thread(&self) -> Vec<(std::thread::ThreadId, String)> {
        let events = self.events.lock().unwrap();
        events
            .iter()
            .map(|(thread, event, _)| (*thread, event.clone()))
            .collect()
    }

    /// When each event was.
    pub fn times(&self) -> Vec<std::time::Instant> {
        let events = self.events.lock().unwrap();
        events.iter().map(|&(_, _, at)| at).collect()
    }

    pub fn clear(&self) {
        self.events.lock().unwrap().clear();
    }

    fn push(&self, event: String) {
        let thread = std::thread::current().id();
        let at = std::time::Instant::now();
        self.events.lock().unwrap().push((thread, event, at));
    }
}

impl elise::TracingController for Recorder {
    fn enabled(&self) -> &std::sync::atomic::AtomicBool {
        &self.on
    }

    fn begin(&self, name: &'static str) {
        self.push(format!("begin {name}"));
        if self.stops_in == Some(name) {
            self.on.store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn end(&self) {
        self.push("end".to_owned());
    }
}

/// `inner`, its trace `trace`.
pub fn with_trace(
    inner: std::sync::Arc<dyn elise::Platform>,
    trace: &'static Recorder,
) -> std::sync::Arc<dyn elise::Platform> {
    std::sync::Arc::new(WithTrace { inner, trace })
}

/// A platform, its trace `trace` (`with_trace`).
struct WithTrace {
    inner: std::sync::Arc<dyn elise::Platform>,
    trace: &'static Recorder,
}

impl elise::Platform for WithTrace {
    fn run(&self, job: &dyn elise::Job) {
        self.inner.run(job);
    }

    fn post_job(
        &self,
        job: std::sync::Arc<dyn elise::Job + Send>,
    ) -> Option<Box<dyn elise::JobHandle>> {
        self.inner.post_job(job)
    }

    fn posts_jobs(&self) -> bool {
        self.inner.posts_jobs()
    }

    fn tracing_controller(&self) -> &'static dyn elise::TracingController {
        self.trace
    }
}

/// Options that check the heap after every GC: every test runs the heap
/// verifier, but under Miri (too slow there; it looks for undefined
/// behavior, the verifier's checks run in `cargo test`).
pub fn verified() -> elise::HeapOptions {
    elise::HeapOptions {
        verify: !cfg!(miri),
        ..elise::HeapOptions::default()
    }
}
