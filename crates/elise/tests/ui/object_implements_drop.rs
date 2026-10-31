// An object's only drop glue is its `External`s': it can't implement
// `Drop` itself (it could reach other objects there, dead ones too).
use elise::{GcObject, Member};

#[derive(GcObject)]
struct Node {
    parent: Member<Node>,
}

impl Drop for Node {
    fn drop(&mut self) {}
}

fn main() {}
