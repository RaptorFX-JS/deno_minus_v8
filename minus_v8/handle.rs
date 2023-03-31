use crate::handle::private::HandleInternal;
use crate::Isolate;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::ops::Deref;
use std::sync::Arc;

pub struct Global<T> {
  data: Arc<T>,
}

impl<T> Global<T> {
  pub fn new(_isolate: &mut Isolate, data: impl Handle<Data = T>) -> Self {
    Self {
      data: data.get_data().clone(),
    }
  }

  pub unsafe fn from_raw(_isolate: &mut Isolate, data: T) -> Option<Self> {
    Some(Self {
      data: Arc::new(data),
    })
  }
}

pub type Weak<T> = Global<T>;

pub struct Local<'s, T> {
  pub(crate) data: Arc<T>,
  phantom: PhantomData<&'s ()>,
}

impl<'s, T> Local<'s, T> {
  pub fn new(_isolate: &mut Isolate, data: impl Handle<Data = T>) -> Self {
    Self {
      data: data.get_data().clone(),
      phantom: PhantomData,
    }
  }

  pub unsafe fn from_raw(data: T) -> Option<Self> {
    Some(Self {
      data: Arc::new(data),
      phantom: PhantomData,
    })
  }
}

impl<T: Hash> Hash for Global<T> {
  fn hash<H: Hasher>(&self, state: &mut H) {
    self.data.as_ref().hash(state);
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
    self.data.as_ref() == other.get_data().as_ref()
  }
}

impl<'s, T, Rhs: Handle> PartialEq<Rhs> for Local<'s, T>
where
  T: PartialEq<Rhs::Data>,
{
  fn eq(&self, other: &Rhs) -> bool {
    self.data.as_ref() == other.get_data().as_ref()
  }
}

impl<T> Eq for Global<T> where T: Eq {}
impl<'s, T> Eq for Local<'s, T> where T: Eq {}

impl<T> Clone for Global<T> {
  fn clone(&self) -> Self {
    Self {
      data: self.data.clone(),
    }
  }
}

// WARNING(minus_v8) we can't impl Copy
// impl<'s, T> Copy for Local<'s, T> {}

impl<'s, T> Clone for Local<'s, T> {
  fn clone(&self) -> Self {
    Self {
      data: self.data.clone(),
      phantom: self.phantom.clone(),
    }
  }
}

impl<'s, T> Deref for Local<'s, T> {
  type Target = T;

  fn deref(&self) -> &T {
    &self.data
  }
}

mod private {
  use super::*;

  pub trait HandleInternal {
    type DataInternal;

    fn get_data(&self) -> &Arc<Self::DataInternal>;
  }
}

pub trait Handle: HandleInternal<DataInternal = Self::Data> + Sized {
  type Data;

  fn open(&self, isolate: &mut Isolate) -> &Self::Data;
}

impl<T> HandleInternal for Global<T> {
  type DataInternal = T;

  fn get_data(&self) -> &Arc<Self::DataInternal> {
    &self.data
  }
}

impl<T> Handle for Global<T> {
  type Data = T;

  fn open(&self, _isolate: &mut Isolate) -> &Self::Data {
    &self.data
  }
}

impl<'s, T> HandleInternal for Local<'s, T> {
  type DataInternal = T;

  fn get_data(&self) -> &Arc<Self::DataInternal> {
    &self.data
  }
}

impl<'s, T> Handle for Local<'s, T> {
  type Data = T;

  fn open(&self, _isolate: &mut Isolate) -> &Self::Data {
    &self.data
  }
}
