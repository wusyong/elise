#![feature(arbitrary_self_types)]
use std::{thread, time::Duration};

use criterion::{black_box, Criterion};
use elise::HeapRoot;
// use elise::{GcStore, Gc, collect, raw, letroot, GC};

#[macro_use]
extern crate criterion;

/// Create a Gc pointer.
fn create(b: &mut Criterion) {
    b.bench_function("shifgrethor-create", |b| {
        b.iter(|| {
            shifgrethor::letroot!(root);
            black_box(root.gc(u32::MAX));
        });
    });
    shifgrethor::collect();

    b.bench_function("elise-create", |b| {
        b.iter(|| {
            elise::letroot!(root);
            root.gc(u32::MAX);
        });
    });
    elise::collect();
}

/// Create a Gc pointer and then collect.
fn oneshot(b: &mut Criterion) {
    b.bench_function("shifgrethor-oneshot", |b| {
        b.iter(|| {
            {
                shifgrethor::letroot!(root);
                black_box(root.gc(u32::MAX));
            }
            shifgrethor::collect();
        });
    });

    b.bench_function("elise-oneshot", |b| {
        b.iter(|| {
            {
                elise::letroot!(root);
                root.gc(u32::MAX);
            }
            elise::collect();
        });
    });
}

/// Create 1,000,000 Gc pointers with 10 collects.
fn chonk(b: &mut Criterion) {
    b.bench_function("shifgrethor-chonk", |b| {
        b.iter(|| {
            for _ in 0..10 {
                for _ in 0..100000 {
                    shifgrethor::letroot!(root);
                    root.gc(u32::MAX);
                }
                shifgrethor::collect();
            }
        });
    });

    b.bench_function("elise-chonk", |b| {
        b.iter(|| {
            for _ in 0..10 {
                for _ in 0..100000 {
                    elise::letroot!(root);
                    root.gc(u32::MAX);
                }
                elise::collect();
            }
        });
    });
}

// TODO Make this function paramterable.
/// Create 10 threads and each creates 100,000 GC pointers.
/// 1% of GC pointers are kept until thread join.
fn tide(b: &mut Criterion) {
    b.bench_function("elise-tide", |b| {
        thread::spawn(|| {
            thread::sleep(Duration::new(1, 0));
            elise::collect();
        });

        b.iter(|| {
            let mut threads = vec![];
            for _ in 0..10 {
                // thread
                let t = thread::spawn(|| {
                    let mut keeps = vec![];
                    for i in 0..100000 {
                        // roots
                        if i % 100 == 0 {
                            let root = HeapRoot::new(1);
                            keeps.push(root);
                        } else {
                            elise::letroot!(root);
                            root.gc(u32::MAX);
                        }
                    }
                });
                threads.push(t);
            }

            for t in threads {
                t.join().unwrap();
            }
        });
    });
}

// TODO test more threads and types
criterion_group!(
    compare, // create,
    // oneshot,
    // chonk,
    tide
);

criterion_main!(compare);
