use std::marker::{PhantomData, PhantomPinned};
use std::pin::Pin;
use std::ptr::NonNull;

use parking_lot::RwLock;

use crate::alloc::Ptr;

pub struct List<T: AsRef<List<T>> + ?Sized> {
    // parking_lot::RwLock is just an AtomicUsize
    prev: RwLock<Option<Ptr<List<T>>>>,
    next: RwLock<Option<Ptr<T>>>,
    _pinned: PhantomPinned,
}

impl<T: AsRef<List<T>> + ?Sized> Default for List<T> {
    fn default() -> List<T> {
        List {
            prev: RwLock::default(),
            next: RwLock::default(),
            _pinned: PhantomPinned,
        }
    }
}

impl<T: AsRef<List<T>> + ?Sized> List<T> {
    pub fn insert(self: Pin<&Self>, new: Pin<&T>) {
        let this: &Self = &*self;
        let new: &T = &*new;

        let list: &List<T> = new.as_ref();
        let mut list_prev = list.prev.write();
        let mut list_next = list.next.write();
        let mut this_next = this.next.write();
        *list_prev = Some(Ptr(NonNull::from(this)));
        *list_next = *this_next;

        if let Some(next) = *this_next {
            unsafe {
                let next: &List<T> = next.as_ref().as_ref();
                let mut next_prev = next.prev.write();
                *next_prev = Some(Ptr(NonNull::from(list)));
            }
        }

        *this_next = Some(Ptr(NonNull::from(new)));
    }

    pub fn is_head(&self) -> bool {
        self.prev.read().is_none()
    }
}

impl<T: AsRef<List<T>> + ?Sized> Drop for List<T> {
    fn drop(&mut self) {
        let prev = self.prev.read();
        let next = self.next.read();
        if let Some(prev) = *prev {
            unsafe {
                let mut prev_next = prev.as_ref().next.write();
                *prev_next = *next;
            }
        }
        if let Some(next) = *next {
            unsafe {
                let mut next_prev = next.as_ref().as_ref().prev.write();
                *next_prev = *prev;
            }
        }
    }
}

impl<'a, T: AsRef<List<T>> + ?Sized> IntoIterator for Pin<&'a List<T>> {
    type IntoIter = Iter<'a, T>;
    type Item = Pin<&'a T>;
    fn into_iter(self) -> Iter<'a, T> {
        Iter {
            next: *(*self).next.read(),
            _marker: PhantomData,
        }
    }
}

pub struct Iter<'a, T: AsRef<List<T>> + ?Sized + 'a> {
    next: Option<Ptr<T>>,
    _marker: PhantomData<&'a T>,
}

impl<'a, T: AsRef<List<T>> + ?Sized> Iterator for Iter<'a, T> {
    type Item = Pin<&'a T>;
    fn next(&mut self) -> Option<Pin<&'a T>> {
        if let Some(next) = self.next {
            unsafe {
                self.next = *next.as_ref().as_ref().next.read();
                Some(Pin::new_unchecked(&*next.as_ptr()))
            }
        } else {
            None
        }
    }
}
