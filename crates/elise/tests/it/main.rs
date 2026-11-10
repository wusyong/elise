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

/// Options that check the heap after every GC: every test runs the heap
/// verifier, but under Miri (too slow there; it looks for undefined
/// behavior, the verifier's checks run in `cargo test`).
/// The flag of a test's platform whose trace always records
/// (`Platform::tracing`).
pub static TRACING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn verified() -> elise::HeapOptions {
    elise::HeapOptions {
        verify: !cfg!(miri),
        ..elise::HeapOptions::default()
    }
}
