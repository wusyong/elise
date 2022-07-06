use std::cell::Cell;
use std::mem;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

use log::*;

use crate::list::List;
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
    list: List<Allocation<Data>>,
    vtable: *mut Vtable,
    marked: Cell<bool>,
}

impl<T: Trace> Allocation<T> {
    pub fn new(data: T) -> Ptr<Allocation<T>> {
        let vtable = extract_vtable(&data);

        let allocation = Box::new(Allocation {
            header: Header {
                list: List::default(),
                vtable,
                marked: Cell::new(false),
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
        if !self.header.marked.replace(true) {
            self.dyn_data().mark()
        }
    }

    pub fn data(&self) -> &T {
        &self.data
    }

    pub fn marked(&self) -> bool {
        self.header.marked.replace(false)
    }

    pub fn is_unmanaged(&self) -> bool {
        self.header.list.is_head()
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

impl AsRef<List<Allocation<Data>>> for Allocation<Data> {
    fn as_ref(&self) -> &List<Allocation<Data>> {
        &self.header.list
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

unsafe impl Send for Header {}
unsafe impl Sync for Header {}

pub struct Ptr<T: ?Sized>(pub NonNull<T>);

unsafe impl<T: ?Sized + Send> Send for Ptr<T> {}
unsafe impl<T: ?Sized + Sync> Sync for Ptr<T> {}

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
