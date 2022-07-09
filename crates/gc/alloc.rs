use std::mem;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering::*};

use log::*;

use crate::trace::Trace;

pub struct Data {
    _priv: (),
}

struct Vtable {
    _priv: (),
}

pub struct Allocation<T: ?Sized> {
    header: Header,
    pub(crate) data: T,
}

struct Header {
    vtable: *mut Vtable,
    marked: AtomicBool,
    managed: AtomicBool,
}

impl<T: Trace> Allocation<T> {
    pub fn new(data: T) -> Ptr<Allocation<T>> {
        let vtable = extract_vtable(&data);

        let allocation = Box::new(Allocation {
            header: Header {
                vtable,
                marked: AtomicBool::new(false),
                managed: AtomicBool::new(false),
            },
            data,
        });
        unsafe { Ptr(NonNull::new_unchecked(Box::into_raw(allocation))) }
    }
}

impl Allocation<Data> {
    pub unsafe fn free(this: *mut Allocation<Data>) {
        (&mut *this).dyn_data_mut().finalize();
        drop(Box::from_raw(this))
    }
}

impl<T: ?Sized> Allocation<T> {
    pub unsafe fn mark(&self) {
        debug!(
            "MARKING object at:          {:x}",
            self.erased() as *const _ as usize
        );
        if !self.header.marked.swap(true, AcqRel) {
            self.dyn_data().mark()
        }
    }

    pub fn data(&self) -> &T {
        &self.data
    }

    pub fn marked(&self) -> bool {
        self.header.marked.swap(false, AcqRel)
    }

    pub fn is_unmanaged(&self) -> bool {
        self.header.marked.load(Acquire)
    }

    pub fn managed(&self) {
        self.header.managed.store(true, Release);
    }

    fn dyn_data(&self) -> &dyn Trace {
        unsafe {
            let object = Object {
                data: self.erased().data() as *const Data,
                vtable: self.header.vtable,
            };
            mem::transmute::<Object, &dyn Trace>(object)
        }
    }

    fn dyn_data_mut(&mut self) -> &mut dyn Trace {
        unsafe {
            let object = Object {
                data: self.erased().data() as *const Data,
                vtable: self.header.vtable,
            };
            mem::transmute::<Object, &mut dyn Trace>(object)
        }
    }

    fn erased(&self) -> &Allocation<Data> {
        unsafe { &*(self as *const Allocation<T> as *const Allocation<Data>) }
    }
}

#[repr(C)]
struct Object {
    data: *const Data,
    vtable: *mut Vtable,
}

fn extract_vtable<T: Trace>(data: &T) -> *mut Vtable {
    unsafe {
        let obj = data as &dyn Trace;
        mem::transmute::<&dyn Trace, Object>(obj).vtable
    }
}

pub struct Ptr<T: ?Sized>(pub NonNull<T>);

impl<T: ?Sized> Copy for Ptr<T> {}
impl<T: ?Sized> Clone for Ptr<T> {
    fn clone(&self) -> Ptr<T> {
        *self
    }
}

impl<T: ?Sized> Deref for Ptr<T> {
    type Target = NonNull<T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T: ?Sized> DerefMut for Ptr<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
