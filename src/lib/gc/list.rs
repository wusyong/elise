use std::marker::{PhantomData, PhantomPinned};
use std::pin::Pin;
use std::ptr::NonNull;

use crossbeam::atomic::AtomicCell;

use crate::alloc::Ptr;

pub struct List<T: AsRef<List<T>> + ?Sized> {
    // TODO they should be in one cell
    prev: AtomicCell<Option<Ptr<List<T>>>>,
    next: AtomicCell<Option<Ptr<T>>>,
    _pinned: PhantomPinned,
}

impl<T: AsRef<List<T>> + ?Sized> Default for List<T> {
    fn default() -> List<T> {
        List {
            prev: AtomicCell::default(),
            next: AtomicCell::default(),
            _pinned: PhantomPinned,
        }
    }
}

impl<T: AsRef<List<T>> + ?Sized> List<T> {
    pub fn insert(self: Pin<&Self>, new: Pin<&T>) {
        let this: &Self = &*self;
        let new: &T = &*new;

        let list: &List<T> = new.as_ref();
        list.prev.store(Some(Ptr(NonNull::from(this))));
        list.next.store(this.next.load());

        if let Some(next) = this.next.load() {
            unsafe {
                let next: &List<T> = next.as_ref().as_ref();
                next.prev.store(Some(Ptr(NonNull::from(list))));
            }
        }

        this.next.store(Some(Ptr(NonNull::from(new))));
    }

    pub fn is_head(&self) -> bool {
        self.prev.load().is_none()
    }
}

impl<T: AsRef<List<T>> + ?Sized> Drop for List<T> {
    fn drop(&mut self) {
        if let Some(prev) = self.prev.load() {
            unsafe {
                prev.as_ref().next.store(self.next.load());
            }
        }
        if let Some(next) = self.next.load() {
            unsafe {
                next.as_ref().as_ref().prev.store(self.prev.load());
            }
        }
    }
}

impl<'a, T: AsRef<List<T>> + ?Sized> IntoIterator for Pin<&'a List<T>> {
    type IntoIter = Iter<'a, T>;
    type Item = Pin<&'a T>;
    fn into_iter(self) -> Iter<'a, T> {
        Iter {
            next: (*self).next.load(),
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
                self.next = next.as_ref().as_ref().next.load();
                Some(Pin::new_unchecked(&*next.as_ptr()))
            }
        } else {
            None
        }
    }
}
