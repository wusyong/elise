use std::sync::atomic::{AtomicUsize, Ordering};

use crossbeam::queue::SegQueue;

use crate::gc_ptr::GcPtr;
use crate::trace::Trace;

static COUNTER: AtomicUsize = AtomicUsize::new(0);
static RECYCLE: SegQueue<usize> = SegQueue::new();

pub struct Root {
    idx: usize,
}

impl Root {
    pub fn new() -> Root {
        Root {
            idx: RECYCLE
                .pop()
                .unwrap_or(COUNTER.fetch_add(1, Ordering::Relaxed)),
        }
    }

    pub unsafe fn enroot<T: Trace + ?Sized>(&self, gc_ptr: GcPtr<T>) {
        super::set_root(self.idx, gc_ptr)
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        RECYCLE.push(self.idx);
        super::pop_root(self.idx);
    }
}
