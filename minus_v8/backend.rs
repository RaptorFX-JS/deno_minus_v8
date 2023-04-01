use crate::serde::{ErasedDeserializer, ErasedSerialize};
use crate::{Function, FunctionCallback, Isolate, Value};
use downcast_rs::{impl_downcast, Downcast};
use serde::Serialize;
use std::collections::HashMap;
use std::ffi::c_void;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryUsage {
  pub physical_total: usize,
  pub heap_total: usize,
  pub heap_used: usize,
  pub external: usize,
}

/// A JS backend that serves as a replacement for V8.
pub trait JsBackend: Downcast {
  /// Inject a native bridge object into the runtime with the given methods.
  fn inject_bridge(
    &mut self,
    path: &str,
    bridge: HashMap<&str, NativeFunctionCallback>,
  );

  /// Execute some JS in the same context as Deno.
  // note: this is a function that returns a function so that this trait is object safe
  fn execute_script(
    &self,
  ) -> &'static dyn Fn(
    /*isolate:*/ &mut Isolate,
    /*name:*/ &str,
    /*source_code:*/ &str,
  ) -> Option<Value>;

  /// Register an ESM module to be run later.
  // note: this is a function that returns a function so that this trait is object safe
  fn load_module(
    &self
  ) -> &'static dyn Fn(
    /*isolate:*/ &mut Isolate,
    /*id:*/ i32,
    /*name:*/ &str,
    /*code:*/ Option<String>,
  );

  // Run a previously-registered ESM module.
  // note: this is a function that returns a function so that this trait is object safe
  fn execute_module(
    &self,
  ) -> &'static dyn Fn(
    /*isolate:*/ &mut Isolate,
    /*id:*/ i32,
  ) -> Option<()>;

  /// Grabs a JS function that might be invoked repeatedly by native code.
  // note: this is a function that returns a function so that this trait is object safe
  fn grab_function(
    &self,
  ) -> &'static dyn Fn(
    /*isolate:*/ &mut Isolate,
    /*name:*/ &str,
  ) -> Option<Function>;

  /// Invokes a JS function acquired with [`grab_function`] on an undefined `this`.
  // note: this is a function that returns a function so that this trait is object safe
  fn invoke_function(
    &self,
  ) -> &'static dyn for<'s> Fn(
    /*isolate:*/ &mut Isolate,
    /*fun:*/ &Function,
    /*args:*/ Vec<Box<dyn ErasedSerialize + 's>>,
  )
    -> Option<Box<dyn ErasedDeserializer<'static>>>;

  /// Set an exception object to throw when control is returned to the backend.
  fn set_exception(&mut self, class: &str, message: &str);

  /// Get memory usage information about the backend.
  fn get_memory_usage(&mut self) -> MemoryUsage;

  /// Requests that the backend terminates.
  fn terminate(&mut self);
}

impl_downcast!(JsBackend);

/// A native function that should be exposed to JS.
pub struct NativeFunctionCallback {
  pub callback: FunctionCallback,
  pub user_data: *const c_void,
}

// TODO this won't pass any tests lol
#[cfg(test)]
pub mod test {
  use super::*;

  pub struct TotallyLegitJSBackend;

  impl JsBackend for TotallyLegitJSBackend {
    fn inject_bridge(
      &mut self,
      _path: &str,
      _bridge: HashMap<&str, NativeFunctionCallback>,
    ) {}

    fn execute_script(
      &self,
    ) -> &'static dyn Fn(&mut Isolate, &str, &str) -> Option<Value> {
      fn inner(
        _isolate: &mut Isolate,
        _name: &str,
        _source_code: &str,
      ) -> Option<Value> {
        None
      }
      &inner
    }

    fn load_module(
      &self,
    ) -> &'static dyn Fn(&mut Isolate, i32, &str, Option<String>) {
      fn inner(
        _isolate: &mut Isolate,
        _id: i32,
        _name: &str,
        _code: Option<String>
      ) {}
      &inner
    }

    // Run a previous-registered ESM module.
    // note: this is a function that returns a function so that this trait is object safe
    fn execute_module(
      &self,
    ) -> &'static dyn Fn(&mut Isolate, i32) -> Option<()> {
      fn inner(
        _isolate: &mut Isolate,
        _id: i32,
      ) -> Option<()> {
        None
      }
      &inner
    }

    fn grab_function(
      &self,
    ) -> &'static dyn Fn(&mut Isolate, &str) -> Option<Function> {
      fn inner(_isolate: &mut Isolate, _name: &str) -> Option<Function> {
        Some(Function(0))
      }
      &inner
    }

    fn invoke_function(
      &self,
    ) -> &'static dyn for<'s> Fn(
      &mut Isolate,
      &Function,
      Vec<Box<dyn ErasedSerialize + 's>>,
    ) -> Option<
      Box<dyn ErasedDeserializer<'static>>,
    > {
      fn inner<'s>(
        _isolate: &mut Isolate,
        _fun: &Function,
        _args: Vec<Box<dyn ErasedSerialize + 's>>,
      ) -> Option<Box<dyn ErasedDeserializer<'static>>> {
        None
      }
      &inner
    }

    fn set_exception(&mut self, _class: &str, _message: &str) {}

    fn get_memory_usage(&mut self) -> MemoryUsage {
      MemoryUsage {
        physical_total: 0,
        heap_total: 0,
        heap_used: 0,
        external: 0,
      }
    }

    fn terminate(&mut self) {}
  }
}
