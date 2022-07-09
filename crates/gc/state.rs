use std::pin::Pin;
use std::ptr::NonNull;

use crossbeam::queue::SegQueue;
use dashmap::iter::Iter;
use dashmap::DashMap;
use log::*;

use crate::alloc::{Allocation, Data, Ptr};
use crate::gc_ptr::GcPtr;
use crate::trace::Trace;

#[derive(Default)]
pub struct GcState {
    objects: SegQueue<Ptr<Allocation<Data>>>,
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

        // TODO pop and push back
        for _ in 0..self.count_objects() {
            match self.objects().pop() {
                Some(object) => {
                    let ptr = unsafe { object.as_ref() };
                    if !ptr.marked() {
                        debug!(
                            "FREEING unmarked object at: {:x}",
                            &*object as *const _ as usize
                        );
                        unsafe {
                            Allocation::free(
                                ptr as *const Allocation<Data> as *mut Allocation<Data>,
                            );
                        }
                    } else {
                        self.objects().push(object);
                    }
                }
                None => break,
            }
        }
    }

    pub unsafe fn manage<T: Trace + ?Sized>(self: Pin<&Self>, ptr: GcPtr<T>) {
        // TODO I should not need a dynamic check here but I am making mistakes
        if ptr.is_unmanaged() {
            let ptr = Ptr(NonNull::from(&*ptr.erased_pinned()));
            ptr.as_ref().managed();
            self.objects().push(ptr);
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

    pub fn objects<'a>(self: Pin<&'a Self>) -> Pin<&'a SegQueue<Ptr<Allocation<Data>>>> {
        unsafe { Pin::map_unchecked(self, |this| &this.objects) }
    }

    pub fn count_objects(&self) -> usize {
        self.objects.len()
    }
}

unsafe impl Send for GcState {}
unsafe impl Sync for GcState {}
