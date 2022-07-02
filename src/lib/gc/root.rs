use std::sync::atomic::{AtomicUsize, Ordering};

use crate::gc_ptr::GcPtr;
use crate::trace::Trace;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

pub struct Root {
    idx: usize,
}

impl Root {
    pub fn new() -> Root {
        Root { idx: COUNTER.fetch_add(1, Ordering::Relaxed) }
    }

    pub unsafe fn enroot<T: Trace + ?Sized>(&self, gc_ptr: GcPtr<T>) {
        super::set_root(self.idx, gc_ptr)
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        super::pop_root(self.idx);
    }
}
