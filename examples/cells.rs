#![feature(arbitrary_self_types)]

use elise::{GcStore, GC};

use pin_cell::PinCell;
use std::cell::RefCell;

#[derive(GC)]
struct Foo<'root> {
    null: RefCell<Null>,
    #[gc]
    traced: PinCell<GcStore<'root, i32>>,
}

#[derive(GC)]
#[gc(null_trace)]
enum Null {
    A(i32),
    B(String),
}

fn main() {
    elise::letroot!(root);
    let foo = root.gc(Foo {
        null: RefCell::new(Null::A(0)),
        traced: PinCell::new(GcStore::new(0)),
    });
    *foo.null.borrow_mut() = Null::B(String::new());
    println!("{}", foo.traced().borrow());
}
