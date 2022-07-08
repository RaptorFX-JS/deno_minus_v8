use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::ops::Deref;
use std::ptr::NonNull;
use crate::minus_v8::handle::private::HandleInternal;
use crate::minus_v8::Isolate;

// TODO(minus_v8) this entire module is a memory leak right now

pub struct Global<T> {
  data: NonNull<T>,
}

impl<T> Global<T> {
  pub fn new(_isolate: &mut Isolate, data: impl Handle<Data = T>) -> Self {
    Self {
      data: data.get_data(),
    }
  }

  pub unsafe fn from_raw(ptr: *const T) -> Option<Self> {
    NonNull::new(ptr as *mut _).map(|nn| Self::from_non_null(nn))
  }

  pub unsafe fn from_non_null(nn: NonNull<T>) -> Self {
    Self {
      data: nn,
    }
  }
}

pub struct Local<'s, T> {
  data: NonNull<T>,
  phantom: PhantomData<&'s ()>,
}

impl<'s, T> Local<'s, T> {
  pub fn new(_isolate: &mut Isolate, data: impl Handle<Data = T>) -> Self {
    Self {
      data: data.get_data(),
      phantom: PhantomData,
    }
  }

  pub unsafe fn from_raw(ptr: *const T) -> Option<Self> {
    NonNull::new(ptr as *mut _).map(|nn| Self::from_non_null(nn))
  }

  pub unsafe fn from_non_null(nn: NonNull<T>) -> Self {
    Self {
      data: nn,
      phantom: PhantomData,
    }
  }
}

impl<T: Hash> Hash for Global<T> {
  fn hash<H: Hasher>(&self, state: &mut H) {
    unsafe { self.data.as_ref().hash(state); }
  }
}

impl<'s, T: Hash> Hash for Local<'s, T> {
  fn hash<H: Hasher>(&self, state: &mut H) {
    (&**self).hash(state)
  }
}

impl<'s, T, Rhs: Handle> PartialEq<Rhs> for Global<T>
where
  T: PartialEq<Rhs::Data>,
{
  fn eq(&self, other: &Rhs) -> bool {
    unsafe { self.data.as_ref() == other.get_data().as_ref() }
  }
}

impl<'s, T, Rhs: Handle> PartialEq<Rhs> for Local<'s, T>
where
  T: PartialEq<Rhs::Data>,
{
  fn eq(&self, other: &Rhs) -> bool {
    unsafe { self.data.as_ref() == other.get_data().as_ref() }
  }
}

impl<T> Eq for Global<T> where T: Eq {}
impl<'s, T> Eq for Local<'s, T> where T: Eq {}

impl<T> Clone for Global<T> {
  fn clone(&self) -> Self {
    Self {
      data: self.data,
    }
  }
}

impl<'s, T> Copy for Local<'s, T> {}

impl<'s, T> Clone for Local<'s, T> {
  fn clone(&self) -> Self {
    *self
  }
}

impl<'s, T> Deref for Local<'s, T> {
  type Target = T;

  fn deref(&self) -> &T {
    unsafe { self.data.as_ref() }
  }
}

mod private {
  use super::*;

  pub trait HandleInternal {
    type DataInternal;

    fn get_data(&self) -> NonNull<Self::DataInternal>;
  }
}

pub trait Handle: HandleInternal<DataInternal = Self::Data> + Sized {
  type Data;

  fn open<'a>(&'a self, isolate: &mut Isolate) -> &'a Self::Data;
}

impl<T> HandleInternal for Global<T> {
  type DataInternal = T;

  fn get_data(&self) -> NonNull<Self::DataInternal> {
    self.data
  }
}

impl<T> Handle for Global<T> {
  type Data = T;

  fn open(&self, _isolate: &mut Isolate) -> &Self::Data {
    unsafe { &*self.data.as_ptr() }
  }
}

impl<'s, T> HandleInternal for Local<'s, T> {
  type DataInternal = T;

  fn get_data(&self) -> NonNull<Self::DataInternal> {
    self.data
  }
}

impl<'s, T> Handle for Local<'s, T> {
  type Data = T;

  fn open<'a>(&'a self, _isolate: &mut Isolate) -> &'a Self::Data {
    unsafe { &*self.data.as_ptr() }
  }
}

