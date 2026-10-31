// Nor kept in a variable from outside it.
use std::cell::Cell;

use elise::{GcObject, Heap, HeapOptions};

#[derive(GcObject)]
struct Leaf {
    value: Cell<u32>,
}

fn main() {
    let mut heap = Heap::new(HeapOptions::default());
    let mut kept = None;
    heap.mutate(|cx| kept = Some(cx.alloc(LeafInit { value: Cell::new(1) })));
    heap.mutate(|_| drop(kept));
}
