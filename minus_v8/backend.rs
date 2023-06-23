use crate::serde::{ErasedDeserializer, ErasedSerialize};
use crate::{Function, FunctionCallback, Isolate, Value};
use downcast_rs::{impl_downcast, Downcast};
use serde::Serialize;
use std::collections::HashMap;
use std::ffi::c_void;
use std::future::Future;
use std::pin::Pin;
use anyhow::Error;
use futures::channel::oneshot::Sender;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryUsage {
  pub physical_total: usize,
  pub heap_total: usize,
  pub heap_used: usize,
  pub external: usize,
}

pub type AsyncFnOutput<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

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
  ) -> &'static dyn for<'a> Fn(
    /*isolate:*/ &'a mut Isolate,
    /*name:*/ &'a str,
    /*source_code:*/ &'a str,
  ) -> AsyncFnOutput<'a, Option<Value>>;

  /// Register an ESM module to be run later.
  // note: this is a function that returns a function so that this trait is object safe
  fn load_module(
    &self
  ) -> &'static dyn for<'a> Fn(
    /*isolate:*/ &'a mut Isolate,
    /*id:*/ i32,
    /*name:*/ &'a str,
    /*code:*/ Option<String>,
  ) -> AsyncFnOutput<'a, ()>;

  // Run a previously-registered ESM module.
  // note: this is a function that returns a function so that this trait is object safe
  fn execute_module(
    &self,
  ) -> &'static dyn for<'a> Fn(
    /*isolate:*/ &'a mut Isolate,
    /*id:*/ i32,
    /*resolve:*/ Sender<Result<(), Error>>,
  ) -> AsyncFnOutput<'a, Option<()>>;

  /// Grabs a JS function that might be invoked repeatedly by native code.
  // note: this is a function that returns a function so that this trait is object safe
  fn grab_function(
    &self,
  ) -> &'static dyn for<'a> Fn(
    /*isolate:*/ &'a mut Isolate,
    /*name:*/ &'a str,
  ) -> AsyncFnOutput<'a, Option<Function>>;

  /// Invokes a JS function acquired with [`grab_function`] on an undefined `this`.
  // note: this is a function that returns a function so that this trait is object safe
  fn invoke_function(
    &self,
  ) -> &'static dyn for<'a> Fn(
    /*isolate:*/ &'a mut Isolate,
    /*fun:*/ &'a Function,
    /*args:*/ Vec<Box<dyn ErasedSerialize + 'a>>,
  )
    -> AsyncFnOutput<'a, Option<Box<dyn ErasedDeserializer<'static>>>>;

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
    ) -> &'static dyn for<'a> Fn(&'a mut Isolate, &'a str, &'a str) -> AsyncFnOutput<'a, Option<Value>> {
      fn inner<'a>(
        _isolate: &'a mut Isolate,
        _name: &'a str,
        _source_code: &'a str,
      ) -> AsyncFnOutput<'a, Option<Value>> {
        Box::pin(async { None })
      }
      &inner
    }

    fn load_module(
      &self,
    ) -> &'static dyn for<'a> Fn(&'a mut Isolate, i32, &'a str, Option<String>) -> AsyncFnOutput<'a, ()> {
      fn inner<'a>(
        _isolate: &'a mut Isolate,
        _id: i32,
        _name: &'a str,
        _code: Option<String>
      ) -> AsyncFnOutput<'a, ()> {
        Box::pin(async { () })
      }
      &inner
    }

    // Run a previous-registered ESM module.
    // note: this is a function that returns a function so that this trait is object safe
    fn execute_module(
      &self,
    ) -> &'static dyn for<'a> Fn(&'a mut Isolate, i32, Sender<Result<(), Error>>) -> AsyncFnOutput<'a, Option<()>> {
      fn inner(
        _isolate: &mut Isolate,
        _id: i32,
        _resolve: Sender<Result<(), Error>>,
      ) -> AsyncFnOutput<Option<()>> {
        Box::pin(async { None })
      }
      &inner
    }

    fn grab_function(
      &self,
    ) -> &'static dyn for<'a> Fn(&'a mut Isolate, &'a str) -> AsyncFnOutput<'a, Option<Function>> {
      fn inner<'a>(_isolate: &'a mut Isolate, _name: &'a str) -> AsyncFnOutput<'a, Option<Function>> {
        Box::pin(async { Some(Function(0)) })
      }
      &inner
    }

    fn invoke_function(
      &self,
    ) -> &'static dyn for<'a> Fn(
      &'a mut Isolate,
      &'a Function,
      Vec<Box<dyn ErasedSerialize + 'a>>,
    ) -> AsyncFnOutput<'a, Option<
      Box<dyn ErasedDeserializer<'static>>,
    >> {
      fn inner<'a>(
        _isolate: &'a mut Isolate,
        _fun: &'a Function,
        _args: Vec<Box<dyn ErasedSerialize + 'a>>,
      ) -> AsyncFnOutput<'a, Option<Box<dyn ErasedDeserializer<'static>>>> {
        Box::pin(async { None })
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
