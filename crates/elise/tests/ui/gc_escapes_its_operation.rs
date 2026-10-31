// A `Gc` can't be returned from its operation: the GC frees objects
// between operations.
use std::cell::Cell;

use elise::{GcObject, Heap, HeapOptions};

#[derive(GcObject)]
struct Leaf {
    value: Cell<u32>,
}

fn main() {
    let mut heap = Heap::new(HeapOptions::default());
    let leaf = heap.mutate(|cx| cx.alloc(LeafInit { value: Cell::new(1) }));
    let _ = leaf;
}
