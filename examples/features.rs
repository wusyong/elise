#![feature(arbitrary_self_types)]

use elise::{Gc, GcStore};

#[derive(elise::GC)]
#[gc(finalize)]
struct Foo<'root> {
    #[gc]
    item: GcStore<'root, i32>,
    #[gc]
    vec: Vec<GcStore<'root, i32>>,
    #[gc]
    option: Option<GcStore<'root, i32>>,
    local: i32,
}

impl<'root> Foo<'root> {
    fn new() -> Foo<'root> {
        Foo {
            item: GcStore::new(0),
            vec: vec![GcStore::new(1), GcStore::new(2), GcStore::new(3)],
            option: Some(GcStore::new(4)),
            local: 5,
        }
    }

    fn print_nonlocal(self: Gc<'_, Self>) {
        println!("{}", self.item());

        for elem in self.vec() {
            println!("{}", elem);
        }

        if let Some(thing) = self.option() {
            println!("{}", thing);
        }
    }
}

impl<'root> elise::Finalize for Foo<'root> {
    fn finalize(&mut self) {
        println!("{}", self.local);
    }
}

fn main() {
    {
        elise::letroot!(root);

        let foo = root.gc(Foo::new());

        elise::collect();

        foo.print_nonlocal();
    }

    elise::collect();
}
