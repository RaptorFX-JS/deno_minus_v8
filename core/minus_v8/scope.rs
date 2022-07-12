use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use crate::minus_v8::{Context, Handle, Isolate, Local, Value};

pub struct HandleScope<'s, C = Context> {
  isolate: &'s mut Isolate,
  phantom: PhantomData<&'s mut C>,
}

impl<'s> HandleScope<'s> {
  pub fn new(isolate: &'s mut Isolate) -> Self {
    Self {
      isolate,
      phantom: PhantomData,
    }
  }

  pub fn with_context<H: Handle<Data = Context>>(isolate: &'s mut Isolate, _context: &H) -> Self {
    Self {
      isolate,
      phantom: PhantomData,
    }
  }
}

impl<'s> HandleScope<'s, ()> {
  pub fn throw_exception(&mut self, exception: Local<Value>) -> Local<'s, Value> {
    if let Value::Exception { class, message } = &*exception {
      self.backend.set_exception(&class, &message);
      super::undefined(self)
    } else {
      panic!("tried to throw a value that wasn't an exception");
    }
  }
}

impl<'s, C> AsRef<HandleScope<'s, ()>> for HandleScope<'s, C> {
  fn as_ref(&self) -> &HandleScope<'s, ()> {
    // SAFETY: phantom is zero-sized and these should be identical
    unsafe { &*(self as *const Self as *const _) }
  }
}

impl<'s, C> AsMut<HandleScope<'s, ()>> for HandleScope<'s, C> {
  fn as_mut(&mut self) -> &mut HandleScope<'s, ()> {
    // SAFETY: phantom is zero-sized and these should be identical
    unsafe { &mut *(self as *mut Self as *mut _) }
  }
}

impl<'s> Deref for HandleScope<'s> {
  type Target = HandleScope<'s, ()>;

  fn deref(&self) -> &Self::Target {
    // SAFETY: phantom is zero-sized and these should be identical
    unsafe { &*(self as *const Self as *const _) }
  }
}

impl<'s> DerefMut for HandleScope<'s> {
  fn deref_mut(&mut self) -> &mut Self::Target {
    // SAFETY: phantom is zero-sized and these should be identical
    unsafe { &mut *(self as *mut Self as *mut _) }
  }
}

impl<'s, C> AsRef<Isolate> for HandleScope<'s, C> {
  fn as_ref(&self) -> &Isolate {
    &self.isolate
  }
}

impl<'s, C> AsMut<Isolate> for HandleScope<'s, C> {
  fn as_mut(&mut self) -> &mut Isolate {
    &mut self.isolate
  }
}

impl<'s> Deref for HandleScope<'s, ()> {
  type Target = Isolate;

  fn deref(&self) -> &Self::Target {
    &self.isolate
  }
}

impl<'s> DerefMut for HandleScope<'s, ()> {
  fn deref_mut(&mut self) -> &mut Self::Target {
    &mut self.isolate
  }
}

pub struct TryCatch<'s, P> {
  isolate: &'s Isolate,
  exception: Option<Local<'s, Value>>,
  phantom: PhantomData<&'s mut P>,
}

impl<'s, 'p, C> TryCatch<'s, HandleScope<'p, C>> {
  pub fn new(scope: &'s mut HandleScope<'p, C>) -> Self {
    Self {
      isolate: scope.isolate,
      exception: None,
      phantom: PhantomData,
    }
  }
}

impl<'s, P> TryCatch<'s, P> {
  pub fn has_caught(&self) -> bool {
    matches!(self.exception, Some(_))
  }

  /// TODO(minus_v8) research cancelable termination
  pub fn has_terminated(&self) -> bool {
    false
  }
}

impl<'s, 'p: 's, P> TryCatch<'s, P>
where
  Self: AsMut<HandleScope<'p, ()>>,
{
  pub fn exception(&mut self) -> Option<Local<'p, Value>> {
    self.exception.clone().map(|x| {
      unsafe {
        Local::from_raw(x.open(self.as_mut()).clone()).unwrap()
      }
    })
  }
}

impl<'s, 'p, C> AsRef<HandleScope<'p, C>> for TryCatch<'s, HandleScope<'p, C>> {
  fn as_ref(&self) -> &HandleScope<'p, C> {
    // SAFETY: phantom is zero-sized and TryCatch has the same layout as HandleScope
    unsafe { &*(self as *const Self as *const _) }
  }
}

impl<'s, 'p, C> AsMut<HandleScope<'p, C>> for TryCatch<'s, HandleScope<'p, C>> {
  fn as_mut(&mut self) -> &mut HandleScope<'p, C> {
    // SAFETY: phantom is zero-sized and TryCatch has the same layout as HandleScope
    unsafe { &mut *(self as *mut Self as *mut _) }
  }
}

impl<'s, 'p> AsRef<HandleScope<'p, ()>> for TryCatch<'s, HandleScope<'p>> {
  fn as_ref(&self) -> &HandleScope<'p, ()> {
    // SAFETY: phantom is zero-sized and TryCatch has the same layout as HandleScope
    unsafe { &*(self as *const Self as *const _) }
  }
}

impl<'s, 'p> AsMut<HandleScope<'p, ()>> for TryCatch<'s, HandleScope<'p>> {
  fn as_mut(&mut self) -> &mut HandleScope<'p, ()> {
    // SAFETY: phantom is zero-sized and TryCatch has the same layout as HandleScope
    unsafe { &mut *(self as *mut Self as *mut _) }
  }
}

impl<'s, 'p> Deref for TryCatch<'s, HandleScope<'p, ()>> {
  type Target = HandleScope<'p, ()>;

  fn deref(&self) -> &Self::Target {
    // SAFETY: phantom is zero-sized and TryCatch has the same layout as HandleScope
    unsafe { &*(self as *const Self as *const _) }
  }
}

impl<'s, 'p> DerefMut for TryCatch<'s, HandleScope<'p, ()>> {
  fn deref_mut(&mut self) -> &mut Self::Target {
    // SAFETY: phantom is zero-sized and TryCatch has the same layout as HandleScope
    unsafe { &mut *(self as *mut Self as *mut _) }
  }
}

impl<'s, 'p> Deref for TryCatch<'s, HandleScope<'p>> {
  type Target = HandleScope<'p>;

  fn deref(&self) -> &Self::Target {
    // SAFETY: phantom is zero-sized and TryCatch has the same layout as HandleScope
    unsafe { &*(self as *const Self as *const _) }
  }
}

impl<'s, 'p> DerefMut for TryCatch<'s, HandleScope<'p>> {
  fn deref_mut(&mut self) -> &mut Self::Target {
    // SAFETY: phantom is zero-sized and TryCatch has the same layout as HandleScope
    unsafe { &mut *(self as *mut Self as *mut _) }
  }
}
