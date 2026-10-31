//! The fuzzer's operations outside the fuzzer: the regressions it found
//! (`crates/elise/fuzz/regressions/`, each the bytes of a run that failed,
//! minimized), runs of pseudo-random bytes, and the encoding.

use crate::ops::{self, Op, Options};

/// The regressions the fuzzer found, replayed.
#[test]
fn the_fuzzers_regressions_pass() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/regressions");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut files: Vec<_> = entries.map(|entry| entry.unwrap().path()).collect();
    files.sort();
    for file in files {
        eprintln!("{}", file.display());
        ops::run(&std::fs::read(&file).unwrap());
    }
}

/// Runs of xorshift bytes, each with other options: the fuzz target's
/// checks hold on whatever they decode to.
#[test]
fn random_runs_agree_with_the_model() {
    let runs = if cfg!(miri) { 2 } else { 80 };
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    for run in 0..runs {
        let bytes: Vec<u8> = (0..if cfg!(miri) { 300 } else { 3000 })
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect();
        let mut bytes = bytes;
        bytes[0] = run as u8 & 63;
        ops::run(&bytes);
    }
}

/// A run written out: an object rooted, a cycle through a list and a blob,
/// garbage with finalizers (the heap's thread's and the sweep's), a root
/// dropped.
#[test]
fn a_written_run_agrees_with_the_model() {
    let ops = [
        Op::Alloc {
            probe: true,
            site: 0,
        },
        Op::Alloc {
            probe: true,
            site: 0,
        },
        Op::AllocBlob,
        Op::Root { object: 0 },
        Op::Push { list: 0, to: 1 },
        Op::SetBlob {
            from: 1,
            to: Some(0),
        },
        Op::SetBack {
            blob: 0,
            to: Some(0),
        },
        Op::SetEdge {
            from: 1,
            field: 3,
            to: Some(1),
        },
        Op::Collect,
        // Old now: an edge from the root to a new, young object, kept by
        // the slot set through two scavenges (copied, then promoted).
        Op::Alloc {
            probe: true,
            site: 0,
        },
        Op::SetEdge {
            from: 0,
            field: 2,
            to: Some(2),
        },
        Op::Scavenge,
        Op::Scavenge,
        Op::Alloc {
            probe: true,
            site: 0,
        },
        Op::Root { object: 3 },
        // Objects whose finalizers are the sweep's: one kept, one garbage.
        Op::AllocSent,
        Op::AllocSent,
        Op::Push { list: 3, to: 4 },
        Op::Unroot { root: 0 },
        Op::Safepoint,
        Op::Remove { list: 0, at: 0 },
        Op::Collect,
        Op::MarkStep { budget: 9 },
        Op::Collect,
    ];
    // And on a heap the engine drives, whose `Collect`s are its.
    for flags in 0..64 {
        ops::run(&ops::encode(&options(flags), &ops));
    }
}

/// The options of `flags`, as the first byte decodes to.
fn options(flags: u8) -> Options {
    Options {
        stress: flags & 1 != 0,
        pool: flags & 2 != 0,
        small_limit: flags & 4 != 0,
        fixed_nursery: flags & 8 != 0,
        concurrent: flags & 16 != 0,
        driven: flags & 32 != 0,
    }
}

/// A run written out where a site is tenured (with a nursery that doesn't
/// grow) and taken back: 120 objects of site 1 kept through a scavenge,
/// then more of it, old, and of site 2, garbage; then the root dropped, so
/// that a full GC finds site 1's old objects dead.
#[test]
fn a_written_run_with_sites_agrees_with_the_model() {
    let mut ops = vec![
        Op::Alloc {
            probe: true,
            site: 0,
        },
        Op::Root { object: 0 },
    ];
    for handle in 1..=120 {
        ops.push(Op::Alloc {
            probe: true,
            site: 1,
        });
        ops.push(Op::Push {
            list: 0,
            to: handle,
        });
    }
    ops.push(Op::Scavenge);
    for _ in 0..300 {
        ops.push(Op::Alloc {
            probe: true,
            site: 1,
        });
        ops.push(Op::Alloc {
            probe: false,
            site: 2,
        });
    }
    ops.extend([
        Op::Scavenge,
        Op::Unroot { root: 0 },
        Op::Collect,
        Op::Alloc {
            probe: true,
            site: 1,
        },
        Op::Root { object: 0 },
        Op::Collect,
    ]);
    for flags in 0..32 {
        ops::run(&ops::encode(&options(flags), &ops));
    }
}

/// A run written out with weak edges and roots: to a young object kept,
/// one dying young, one dying old; each read after a scavenge, a full GC,
/// and steps of a marking (where a read keeps its object, linked after).
#[test]
fn a_written_run_with_weak_edges_agrees_with_the_model() {
    let alloc = Op::Alloc {
        probe: true,
        site: 0,
    };
    let ops = [
        alloc.clone(),
        alloc.clone(),
        alloc.clone(),
        alloc.clone(),
        Op::Root { object: 0 },
        // 0 → 1 strong, weak to 1, 2 and 3; weak roots to 2 and 3.
        Op::SetEdge {
            from: 0,
            field: 0,
            to: Some(1),
        },
        Op::SetWeak {
            from: 0,
            to: Some(1),
        },
        Op::SetWeak {
            from: 1,
            to: Some(2),
        },
        Op::SetWeak {
            from: 2,
            to: Some(3),
        },
        Op::WeakRoot { object: 2 },
        Op::WeakRoot { object: 3 },
        Op::Scavenge,
        Op::ReadWeak { from: 0 },
        Op::ReadWeak { from: 1 },
        Op::ReadWeakRoot { root: 0 },
        Op::Safepoint,
        Op::Collect,
        Op::ReadWeak { from: 0 },
        Op::ReadWeak { from: 1 },
        Op::ReadWeakRoot { root: 1 },
        // Old garbage behind a weak edge, then GCs and marking steps
        // between reads; a read links what it gets from the root.
        alloc,
        Op::SetWeak {
            from: 0,
            to: Some(2),
        },
        Op::Collect,
        Op::MarkStep { budget: 1 },
        Op::ReadWeak { from: 0 },
        Op::SetEdge {
            from: 0,
            field: 1,
            to: Some(2),
        },
        Op::MarkStep { budget: 9 },
        Op::Safepoint,
        Op::Collect,
        Op::WeakUnroot { root: 0 },
        Op::Collect,
    ];
    for flags in 0..32 {
        ops::run(&ops::encode(&options(flags), &ops));
    }
}

/// A run written out with ephemerons: a live key's value kept, a dead
/// key's pair cleared (its value pointing back to its key), a chain of two
/// pairs; each read after scavenges, full GCs and marking steps.
#[test]
fn a_written_run_with_ephemerons_agrees_with_the_model() {
    let alloc = Op::Alloc {
        probe: true,
        site: 0,
    };
    let mut ops = vec![alloc.clone(); 6];
    ops.extend([
        Op::Root { object: 0 },
        Op::Root { object: 1 },
        // 0's pair: 1 → 2; 2's pair: 3 → 4, 4 → 3 back; 1's pair: 2 → 5.
        Op::SetPair {
            from: 0,
            key: Some(1),
            value: Some(2),
        },
        Op::SetPair {
            from: 2,
            key: Some(3),
            value: Some(4),
        },
        Op::SetEdge {
            from: 4,
            field: 0,
            to: Some(3),
        },
        Op::SetPair {
            from: 1,
            key: Some(2),
            value: Some(5),
        },
        Op::Scavenge,
        Op::ReadPair { from: 0 },
        Op::ReadPair { from: 1 },
        Op::Safepoint,
        Op::Collect,
        Op::ReadPair { from: 0 },
        Op::ReadPair { from: 1 },
        Op::Safepoint,
        Op::MarkStep { budget: 1 },
        Op::ReadPair { from: 0 },
        Op::MarkStep { budget: 9 },
        Op::Safepoint,
        Op::Unroot { root: 1 },
        Op::Collect,
        Op::ReadPair { from: 0 },
        Op::Collect,
    ]);
    for flags in 0..32 {
        ops::run(&ops::encode(&options(flags), &ops));
    }
}

/// A run written out with ids: objects and a blob given ids, one kept,
/// the others garbage; each looked up after scavenges, full GCs and
/// marking steps (where a lookup keeps what it finds, linked after).
#[test]
fn a_written_run_with_ids_agrees_with_the_model() {
    let alloc = Op::Alloc {
        probe: true,
        site: 0,
    };
    let ops = [
        alloc.clone(),
        alloc.clone(),
        alloc.clone(),
        Op::AllocBlob,
        Op::Root { object: 0 },
        Op::NewId { object: 0 },
        Op::NewId { object: 1 },
        Op::NewId { object: 3 },
        Op::Scavenge,
        Op::ById { id: 0 },
        Op::ById { id: 1 },
        Op::ById { id: 2 },
        Op::Safepoint,
        Op::Collect,
        Op::ById { id: 0 },
        Op::ById { id: 1 },
        alloc,
        Op::NewId { object: 1 },
        Op::Collect,
        Op::MarkStep { budget: 1 },
        Op::ById { id: 3 },
        Op::SetEdge {
            from: 0,
            field: 0,
            to: Some(1),
        },
        Op::MarkStep { budget: 9 },
        Op::Safepoint,
        Op::Collect,
        Op::ById { id: 3 },
    ];
    for flags in 0..32 {
        ops::run(&ops::encode(&options(flags), &ops));
    }
}

/// `decode` gives back what `encode` encoded.
#[test]
fn encoding_round_trips() {
    let ops = vec![
        Op::Alloc {
            probe: false,
            site: 0,
        },
        Op::Alloc {
            probe: true,
            site: 0,
        },
        Op::Alloc {
            probe: false,
            site: 1,
        },
        Op::Alloc {
            probe: true,
            site: 2,
        },
        Op::AllocBlob,
        Op::SetEdge {
            from: 7,
            field: 2,
            to: None,
        },
        Op::SetEdge {
            from: 1,
            field: 3,
            to: Some(300),
        },
        Op::SetBlob { from: 2, to: None },
        Op::SetBack {
            blob: 9,
            to: Some(4),
        },
        Op::Push { list: 1, to: 2 },
        Op::Remove { list: 3, at: 4 },
        Op::Root { object: 5 },
        Op::Unroot { root: 6 },
        Op::SetWeak {
            from: 3,
            to: Some(8),
        },
        Op::SetWeak { from: 4, to: None },
        Op::ReadWeak { from: 2 },
        Op::WeakRoot { object: 1 },
        Op::WeakUnroot { root: 0 },
        Op::ReadWeakRoot { root: 3 },
        Op::SetPair {
            from: 1,
            key: Some(2),
            value: Some(3),
        },
        Op::SetPair {
            from: 1,
            key: Some(2),
            value: None,
        },
        Op::SetPair {
            from: 6,
            key: None,
            value: None,
        },
        Op::ReadPair { from: 7 },
        Op::NewId { object: 8 },
        Op::ById { id: 9 },
        Op::Safepoint,
        Op::MarkStep { budget: 0 },
        Op::MarkStep { budget: 9 },
        Op::Collect,
        Op::Scavenge,
    ];
    let mut ops = ops;
    ops.push(Op::AllocSent);
    let (decoded_options, decoded) = ops::decode(&ops::encode(&options(0b11101), &ops));
    assert_eq!(decoded, ops);
    assert!(
        decoded_options.stress
            && !decoded_options.pool
            && decoded_options.small_limit
            && decoded_options.fixed_nursery
            && decoded_options.concurrent
            && !decoded_options.driven
    );
    // The engine's, a driven run's; its bytes are scavenges in others.
    ops.extend([
        Op::Wrapper { object: 3 },
        Op::Release { wrapper: 1 },
        Op::Unwrap { wrapper: 2 },
        Op::Link { from: 4, to: 5 },
        Op::Unlink { from: 6 },
    ]);
    let (decoded_options, decoded) = ops::decode(&ops::encode(&options(0b100000), &ops));
    assert_eq!(decoded, ops);
    assert!(decoded_options.driven);
    let (_, decoded) = ops::decode(&[0, 35, 131, 155, 179, 203, 227, 251]);
    assert_eq!(decoded, vec![Op::Scavenge; 7]);
    let (_, decoded) = ops::decode(&[32, 11, 35, 1, 0, 251]);
    assert_eq!(
        decoded,
        vec![Op::Scavenge, Op::Wrapper { object: 1 }, Op::Scavenge]
    );
}

/// A run written out on a heap the engine drives: wrappers held, a chain
/// through two wrappers' property (both heaps, twice), a marking stepped
/// while script makes and drops wrappers and links them, the engine's
/// pauses; then what only the engine held goes.
#[test]
fn a_written_driven_run_agrees_with_the_model() {
    let alloc = Op::Alloc {
        probe: true,
        site: 0,
    };
    let ops = vec![
        alloc.clone(),
        alloc.clone(),
        alloc.clone(),
        // Object 0's wrapper refers to object 1's, whose object points to
        // object 2; script holds 0's only.
        Op::Wrapper { object: 0 },
        Op::Wrapper { object: 1 },
        Op::SetEdge {
            from: 1,
            field: 0,
            to: Some(2),
        },
        Op::Link { from: 0, to: 1 },
        Op::Release { wrapper: 1 },
        Op::Safepoint,
        Op::Collect,
        // A marking stepped: a wrapper made (black) and linked while it
        // runs, another dropped.
        Op::MarkStep { budget: 0 },
        alloc.clone(),
        Op::Wrapper { object: 3 },
        Op::Unwrap { wrapper: 0 },
        Op::Link { from: 0, to: 1 },
        Op::MarkStep { budget: 1 },
        Op::Release { wrapper: 1 },
        Op::MarkStep { budget: 9 },
        Op::Collect,
        // Script drops everything: all goes.
        Op::Unlink { from: 0 },
        Op::Release { wrapper: 0 },
        Op::Collect,
        Op::Collect,
    ];
    for flags in 32..64 {
        ops::run(&ops::encode(&options(flags), &ops));
    }
}
