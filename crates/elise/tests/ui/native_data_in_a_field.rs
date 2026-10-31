// Native data goes in an `External`: a `Box`, a `Vec` or a reference as a
// field isn't a `HeapField`.
use elise::GcObject;

#[derive(GcObject)]
struct Boxed {
    value: Box<u32>,
}

#[derive(GcObject)]
struct Listed {
    values: Vec<u32>,
}

#[derive(GcObject)]
struct Borrowed {
    value: &'static u32,
}

fn main() {}
