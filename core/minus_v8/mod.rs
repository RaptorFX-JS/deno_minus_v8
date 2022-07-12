//! A variety of shims and hacks to compile Deno code without V8
//! with minimal source modification to Deno itself. This module
//! effectively replaces both `v8` and `serde_v8`.

pub mod backend;
pub mod serde;

mod data;
mod handle;
mod function;
mod scope;

pub use data::*;
pub use handle::*;
pub use function::*;
pub use scope::*;

use crate::gotham_state::GothamState;
use backend::JsBackend;

pub struct V8;

impl V8 {
  pub fn get_version() -> &'static str {
    "10.4.20.69+deno_minus_v8_shim"
  }
}

pub struct Context {
  global: Global<Object>,
}

impl Context {
  pub fn new<'s>(scope: &mut HandleScope<'s, ()>) -> Local<'s, Context> {
    unsafe {
      Local::from_raw(Context {
        global: Global::from_raw(scope, Object {}).unwrap(),
      }).unwrap()
    }
  }

  pub fn global<'s>(&self, scope: &mut HandleScope<'s, ()>) -> Local<'s, Object> {
    Local::new(scope, self.global.clone())
  }
}

pub struct Isolate {
  pub backend: Box<dyn JsBackend>,
  slots: GothamState,
}

impl Isolate {
  pub fn new(backend: Box<dyn JsBackend>) -> Self {
    Self {
      backend,
      slots: GothamState::default(),
    }
  }

  pub fn get_current_context(&self) -> Local<Context> {
    unimplemented!("minus_v8: no global context access")
  }

  #[inline]
  pub fn get_slot<T: 'static>(&self) -> Option<&T> {
    self.slots.try_borrow()
  }

  #[inline]
  pub fn set_slot<T: 'static>(&mut self, value: T) {
    self.slots.put(value)
  }

  pub fn terminate_execution(&mut self) -> bool {
    self.backend.terminate();
    true
  }

  /// TODO(minus_v8) research cancelable termination
  pub fn is_execution_terminating(&self) -> bool {
    false
  }
}

pub type OwnedIsolate = Isolate;
