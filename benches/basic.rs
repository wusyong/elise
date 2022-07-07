#![feature(arbitrary_self_types)]
use criterion::{Criterion, black_box};
// use elise::{GcStore, Gc, collect, raw, letroot, GC};

#[macro_use]
extern crate criterion;

// #[derive(GC)]
// struct Foo<'root> {
//     #[gc]
//     item: GcStore<'root, i32>,
//     #[gc]
//     vec: Vec<GcStore<'root, i32>>,
//     #[gc]
//     option: Option<GcStore<'root, i32>>,
//     local: i32,
// }

// impl<'root> Foo<'root> {
//     fn new() -> Foo<'root> {
//         Foo {
//             item: GcStore::new(0),
//             vec: vec![GcStore::new(1), GcStore::new(2), GcStore::new(3)],
//             option: Some(GcStore::new(4)),
//             local: 5,
//         }
//     }
// }

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

// TODO test more threads and types
criterion_group!(
    compare,
    create,
    oneshot,
);

criterion_main!(compare);