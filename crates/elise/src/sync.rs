//! The atomics, locks and threads the GC shares between threads: std's,
//! or, built with `--cfg loom` or `--cfg shuttle`, loom's or shuttle's, so
//! that those check the code that uses them (loom every interleaving of a
//! small model under the C11 memory model; shuttle random schedules of a
//! larger one). Everything shared between the GC's threads goes through
//! here.

#[cfg(loom)]
pub(crate) use loom::{
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    sync::{Arc, Condvar, Mutex, MutexGuard},
    thread,
};
#[cfg(shuttle)]
pub(crate) use shuttle::{
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    sync::{Arc, Condvar, Mutex, MutexGuard},
    thread,
};
#[cfg(not(any(loom, shuttle)))]
pub(crate) use std::{
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    sync::{Arc, Condvar, Mutex, MutexGuard},
    thread,
};

/// Locks `mutex`, through a poisoning: the GC's locks guard plain counts and
/// lists, whole at every unlock, and a panic on a worker is passed on to the
/// thread that started the work.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Waits on `condvar`, through a poisoning, as `lock`.
pub(crate) fn wait<'a, T>(condvar: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    condvar
        .wait(guard)
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What `mutex` held, through a poisoning, as `lock`.
pub(crate) fn into_inner<T>(mutex: Mutex<T>) -> T {
    mutex
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
