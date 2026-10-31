// A `Member` can't be made outside the heap: no constructor, no `Default`.
use std::cell::Cell;

use elise::{GcObject, Member};

#[derive(GcObject)]
struct Leaf {
    value: Cell<u32>,
}

fn main() {
    let _member: Member<Leaf> = Member::default();
}
