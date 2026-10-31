// An `External`'s `Drop` runs on the sweep's worker, so its value is
// `Send`; an `Rc` goes in a `LocalExternal`, finalized on the heap's
// thread.
use std::rc::Rc;

use elise::{External, GcObject};

#[derive(GcObject)]
struct Node {
    shared: External<Rc<u32>>,
}

fn main() {}
