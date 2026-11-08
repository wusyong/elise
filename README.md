# Elise

A concurrent garbage collector for DOM-like object graphs, in Rust. 
Code that uses it writes no `unsafe`: objects are
plain structs with `#[derive(GcObject)]`, and the compiler keeps pointers
from outliving the operation that holds them.

## Features

- **Collected only at safepoints.** The mutator runs in operations
  (`Heap::mutate`); no GC happens inside one, so a `Gc<'gc, T>` is a raw
  pointer branded with the operation's lifetime, nothing scans the stack,
  and roots are explicit (`Persistent<T>`).
- **Compressed pointers.** Each heap reserves a 4 GiB cage; an edge
  (`Member<T>`) is a 32-bit offset into it.
- **Generational.** New objects go to a nursery collected by a parallel
  scavenger. Allocation sites that keep surviving are pretenured.
- **Concurrent old generation.** Mark-sweep with snapshot-at-the-beginning
  marking and sweeping on worker threads while the mutator runs. A cycle
  pauses the mutator only to start and to finish, at safepoints.
- **Size-class pages** with side mark bitmaps and bump allocation through
  free ranges; large objects get pages of their own. Empty pages are pooled,
  then handed back to the OS.
- **Weak references and ephemerons:** `WeakMember`, `WeakPersistent`,
  `EphemeronPair`, plus an id table for code outside the heap.
- **Finalizers that can't touch the heap:** native data lives in
  `External<T>` (dropped on a sweeper thread) or `LocalExternal<T>` (dropped
  on the heap's thread).
- **Marking with a JS engine:** an engine can drive the marking, the way V8
  drives cppgc in Chrome's unified heap. Objects hold the engine's
  references, and wrappers hold objects' addresses.
- **Heap snapshots:** `Heap::walk` hands out every object the heap holds,
  with its type, size and edges by their fields' names, for an engine's
  heap profiler.
- **Types registered at run time** (`raw`): layouts an embedder gives as it
  runs, such as a wasm engine's GC types, whose edges may hold 31-bit
  integers (`I31`, wasm's `i31ref`); allocated by type index, and
  collected with roots the embedder hands over.
- **Pluggable threads:** a `Platform` decides where the parallel work runs.
  The default is a process-wide thread pool.

## Example

```rust
use std::cell::Cell;

use elise::{GcObject, Heap, HeapOptions, Member, Persistent};

#[derive(GcObject)]
struct Node {
    parent: Member<Node>,
    id: Cell<u32>,
}

let mut heap = Heap::new(HeapOptions::default());
let root = heap.mutate(|cx| {
    let parent = cx.alloc(NodeInit { parent: None, id: Cell::new(1) });
    let child = cx.alloc(NodeInit { parent: Some(parent), id: Cell::new(2) });
    assert_eq!(child.parent.get(cx).unwrap().id.get(), 1);
    Persistent::new(cx, child)
});
heap.collect();
heap.mutate(|cx| assert_eq!(root.get(cx).parent.get(cx).unwrap().id.get(), 1));
```

The derive generates `NodeInit<'gc>`, in which each `Member<T>` becomes an
`Option<Gc<'gc, T>>`. `alloc` writes it in place, so an edge never exists
outside the heap. Fields that can't be traced, such as a `Box`, a `Vec` or
a reference, fail to compile. So does a `Gc` kept past its operation, or an
object that implements `Drop`.

To use it:

```toml
[dependencies]
elise = "0.2"
```

## Safety

`unsafe` is confined to the `elise` crate. Why safe code can't cause
undefined behavior through its API is argued invariant by invariant in
[docs/soundness.md](docs/soundness.md). Beyond the unit and integration
tests, it is checked by:

- a heap verifier that walks the heap again after every GC in the tests;
- Miri (Stacked Borrows, strict provenance) over the whole test suite;
- loom (exhaustive interleavings) and shuttle (random schedules) on the
  parallel code;
- cargo-fuzz with ASan, against a reference model that never collects;
- Verus proofs of the pages' bitmap search;
- TLA+ models of the marking protocols, checked with TLC;
- compile-fail tests (trybuild) for the rules the type system enforces.

## Platforms

64-bit macOS, Linux and Windows: the cage is reserved with `mmap` or
`VirtualAlloc`. Built and tested with Rust 1.99.0, pinned in
`rust-toolchain.toml`. Fuzzing and Miri use the nightly pinned in
`crates/elise/fuzz/rust-toolchain.toml`.

## Building and testing

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
python3 scripts/sanity.py tests       # the repo's rules
python3 scripts/sanity.py numerics
```

The slower runs need tools of their own (`python3 scripts/test.py -h`):

```sh
python3 scripts/test.py models        # loom and shuttle
python3 scripts/test.py fuzz          # cargo-fuzz
python3 scripts/test.py verus         # Verus proofs
python3 scripts/test.py miri          # Miri, through cargo-nextest
python3 tla/marking/check.py          # TLA+ models (TLC, needs Java)
python3 tla/unified/check.py
```

## Repository layout

```text
crates/elise/          the heap
crates/elise/fuzz/     the fuzz target and the regressions it found
crates/elise-derive/   #[derive(GcObject)]
docs/                  design and reference documents
scripts/               test runs beyond cargo test, and the repo's checks
tla/                   TLA+ models of the marking protocols
```

## Documentation

The documents in `docs/` are written in Traditional Chinese:

- [api.md](docs/api.md): the types, basic use and the rules objects follow.
- [design.md](docs/design.md): how each part works, compared with V8 and
  cppgc, and why it differs.
- [soundness.md](docs/soundness.md): the safety argument, its assumptions
  and what to review when changing the code.
- [testing.md](docs/testing.md): every kind of test, what each covers and
  how to run it.
- [contributing.md](docs/contributing.md): the repo's rules (`unsafe`,
  integer overflow, tests, line endings) and how elise stays in sync with
  machopo.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
