use std::collections::HashMap;
use std::ffi::c_void;
use downcast_rs::{Downcast, impl_downcast};
use crate::v8::{Function, FunctionCallback, Value};
use crate::serde_v8::{ErasedDeserializer, ErasedSerialize};

pub use crate::ops_builtin_v8::MemoryUsage;

/// A JS backend that serves as a replacement for V8.
pub trait JsBackend: Downcast {
  /// Inject a native bridge object into the runtime with the given methods.
  fn inject_bridge(&mut self, path: &str, bridge: HashMap<&str, NativeFunctionCallback>);

  /// Execute some JS in the same context as Deno.
  fn execute_script(&mut self, name: &str, source_code: &str) -> Option<Value>;

  /// Grabs a JS function that might be invoked repeatedly by native code.
  fn grab_function(&mut self, name: &str) -> Option<Function>;

  /// Invokes a JS function acquired with [`grab_function`] on an undefined `this`.
  fn invoke_function<'s>(
    &mut self,
    fun: &Function,
    args: Vec<Box<dyn ErasedSerialize + 's>>
  ) -> Option<Box<dyn ErasedDeserializer<'static>>>;

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
    fn inject_bridge(&mut self, _path: &str, _bridge: HashMap<&str, NativeFunctionCallback>) {}

    fn execute_script(&mut self, _name: &str, _source_code: &str) {}

    fn grab_function(&mut self, _name: &str) -> Option<Function> {
      Some(Function(0))
    }

    fn invoke_function<'s>(
      &mut self,
      fun: &Function,
      args: Vec<Box<dyn ErasedSerialize + 's>>,
    ) -> Option<Box<dyn ErasedDeserializer<'static>>> {
      None
    }

    fn set_exception(&mut self, _class: &str, _message: &str) {}

    fn get_memory_usage(&mut self) -> MemoryUsage {
      MemoryUsage {
        rss: 0,
        heap_total: 0,
        heap_used: 0,
        external: 0,
      }
    }

    fn terminate(&mut self) {}
  }
}
