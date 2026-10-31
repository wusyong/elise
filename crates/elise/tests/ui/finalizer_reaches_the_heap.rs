// A finalizer is an `External`'s or a `LocalExternal`'s `Drop`, and either
// holds only `'static` data: not a `Gc`, so a finalizer can't reach the
// heap. (An `External` also wants its data `Send`, which a `Gc` isn't
// either: a `LocalExternal` checks the `'static` alone.)
use std::cell::Cell;

use elise::{Gc, GcObject, Heap, HeapOptions, LocalExternal};

#[derive(GcObject)]
struct Leaf {
    value: Cell<u32>,
}

struct Holder<'gc> {
    leaf: Gc<'gc, Leaf>,
}

impl Drop for Holder<'_> {
    fn drop(&mut self) {
        self.leaf.value.set(0);
    }
}

fn main() {
    let mut heap = Heap::new(HeapOptions::default());
    heap.mutate(|cx| {
        let leaf = cx.alloc(LeafInit { value: Cell::new(1) });
        let _holder = LocalExternal::new(Holder { leaf });
    });
}
