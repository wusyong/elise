use std::pin::Pin;

use dashmap::iter::Iter;
use dashmap::DashMap;
use log::*;

use crate::alloc::{Allocation, Data, Ptr};
use crate::gc_ptr::GcPtr;
use crate::list::List;
use crate::trace::Trace;

#[derive(Default)]
pub struct GcState {
    objects: List<Allocation<Data>>,
    roots: DashMap<usize, Option<Ptr<Allocation<Data>>>>,
}

impl GcState {
    pub fn collect(self: Pin<&Self>) {
        for pair in self.roots() {
            if let Some(root) = pair.value() {
                debug!(
                    "TRACING from root at:       {:x} (idx {:x})",
                    &*root as *const _ as usize,
                    pair.key()
                );
                unsafe {
                    root.as_ref().mark();
                }
            }
        }

        for object in self.objects() {
            if !object.marked() {
                debug!(
                    "FREEING unmarked object at: {:x}",
                    &*object as *const _ as usize
                );
                unsafe {
                    (&*object as *const Allocation<Data> as *mut Allocation<Data>).free();
                }
            }
        }
    }

    pub unsafe fn manage<T: Trace + ?Sized>(self: Pin<&Self>, ptr: GcPtr<T>) {
        // TODO I should not need a dynamic check here but I am making mistakes
        if ptr.is_unmanaged() {
            self.objects().insert(ptr.erased_pinned());
        }
        ptr.data().manage();
    }

    pub fn set_root<T: Trace + ?Sized>(self: Pin<&Self>, idx: usize, ptr: GcPtr<T>) {
        let root: Ptr<Allocation<Data>> = ptr.erased();
        debug!(
            "ENROOTING root at:          {:x} (idx {:x})",
            root.as_ptr() as usize,
            idx
        );
        self.roots.insert(idx, Some(root));
    }

    pub fn pop_root(self: Pin<&Self>, idx: usize) {
        if let Some((idx, Some(root))) = self.roots.remove(&idx) {
            debug!(
                " DROPPING root at:           {:x} (idx {:x})",
                root.as_ptr() as usize,
                idx
            );
        }
    }

    pub fn roots(&self) -> Iter<usize, Option<Ptr<Allocation<Data>>>> {
        self.roots.iter()
    }

    pub fn count_roots(&self) -> usize {
        self.roots.len()
    }

    pub fn objects<'a>(self: Pin<&'a Self>) -> Pin<&'a List<Allocation<Data>>> {
        unsafe { Pin::map_unchecked(self, |this| &this.objects) }
    }
}
