// Nor moved out of its object (into a `LocalExternal`, say): it isn't `Copy`
// or `Clone`, and a `Gc` lends only `&T`.
use elise::{GcObject, Heap, HeapOptions, LocalExternal, Member};

#[derive(GcObject)]
struct Node {
    parent: Member<Node>,
}

fn main() {
    let mut heap = Heap::new(HeapOptions::default());
    heap.mutate(|cx| {
        let node = cx.alloc(NodeInit { parent: None });
        let _moved = LocalExternal::new(node.parent);
    });
}
