// Copyright 2018-2022 the Deno authors. All rights reserved. MIT license.

use crate::error::is_instance_of_error;
use crate::ops::OpCtx;
use crate::JsRuntime;
use log::debug;
use std::option::Option;
use std::os::raw::c_void;
use v8::MapFnTo;

// TODO(nayeemrmn): Move to runtime and/or make `pub(crate)`.
pub fn script_origin<'a>(
  s: &mut v8::HandleScope<'a>,
  resource_name: v8::Local<'a, v8::String>,
) -> v8::ScriptOrigin<'a> {
  let source_map_url = v8::String::new(s, "").unwrap();
  v8::ScriptOrigin::new(
    s,
    resource_name.into(),
    0,
    0,
    false,
    123,
    source_map_url.into(),
    true,
    false,
    false,
  )
}

pub fn initialize_context<'s>(
  scope: &mut v8::HandleScope<'s, ()>,
  op_ctxs: &[OpCtx],
) -> v8::Local<'s, v8::Context> {
  let scope = &mut v8::EscapableHandleScope::new(scope);

  let context = v8::Context::new(scope);
  let global = context.global(scope);

  let scope = &mut v8::ContextScope::new(scope, context);

  // global.Deno = { core: { } };
  let core_val = JsRuntime::ensure_objs(scope, global, "Deno.core").unwrap();

  // Bind functions to Deno.core.*
  set_func(scope, core_val, "callConsole", call_console);

  // Bind functions to Deno.core.ops.*
  let ops_obj = JsRuntime::ensure_objs(scope, global, "Deno.core.ops").unwrap();
  initialize_ops(scope, ops_obj, op_ctxs);
  scope.escape(context)
}

fn initialize_ops(
  scope: &mut v8::HandleScope,
  ops_obj: v8::Local<v8::Object>,
  op_ctxs: &[OpCtx],
) {
  for ctx in op_ctxs {
    let ctx_ptr = ctx as *const OpCtx as *const c_void;
    set_func_raw(scope, ops_obj, ctx.decl.name, ctx.decl.v8_fn_ptr, ctx_ptr);
  }
}

pub fn set_func(
  scope: &mut v8::HandleScope<'_>,
  obj: v8::Local<v8::Object>,
  name: &'static str,
  callback: impl v8::MapFnTo<v8::FunctionCallback>,
) {
  let key = v8::String::new(scope, name).unwrap();
  let val = v8::Function::new(scope, callback).unwrap();
  val.set_name(key);
  obj.set(scope, key.into(), val.into());
}

// Register a raw v8::FunctionCallback
// with some external data.
pub fn set_func_raw(
  scope: &mut v8::HandleScope<'_>,
  obj: v8::Local<v8::Object>,
  name: &'static str,
  callback: v8::FunctionCallback,
  external_data: *const c_void,
) {
  let key = v8::String::new(scope, name).unwrap();
  let external = v8::External::new(scope, external_data as *mut c_void);
  let val = v8::Function::builder_raw(callback)
    .data(external.into())
    .build(scope)
    .unwrap();
  val.set_name(key);
  obj.set(scope, key.into(), val.into());
}

pub extern "C" fn promise_reject_callback(message: v8::PromiseRejectMessage) {
  use v8::PromiseRejectEvent::*;

  // SAFETY: `CallbackScope` can be safely constructed from `&PromiseRejectMessage`
  let scope = &mut unsafe { v8::CallbackScope::new(&message) };

  let state_rc = JsRuntime::state(scope);
  let mut state = state_rc.borrow_mut();

  // Node compat: perform synchronous process.emit("unhandledRejection").
  //
  // Note the callback follows the (type, promise, reason) signature of Node's
  // internal promiseRejectHandler from lib/internal/process/promises.js, not
  // the (promise, reason) signature of the "unhandledRejection" event listener.
  //
  // Short-circuits Deno's regular unhandled rejection logic because that's
  // a) asynchronous, and b) always terminates.
  if let Some(js_promise_reject_cb) = state.js_promise_reject_cb.clone() {
    let js_uncaught_exception_cb = state.js_uncaught_exception_cb.clone();
    drop(state); // Drop borrow, callbacks can call back into runtime.

    let tc_scope = &mut v8::TryCatch::new(scope);
    let undefined: v8::Local<v8::Value> = v8::undefined(tc_scope).into();
    let type_ = v8::Integer::new(tc_scope, message.get_event() as i32);
    let promise = message.get_promise();

    let reason = match message.get_event() {
      PromiseRejectWithNoHandler
      | PromiseRejectAfterResolved
      | PromiseResolveAfterResolved => message.get_value().unwrap_or(undefined),
      PromiseHandlerAddedAfterReject => undefined,
    };

    let args = &[type_.into(), promise.into(), reason];
    js_promise_reject_cb
      .open(tc_scope)
      .call(tc_scope, undefined, args);

    if let Some(exception) = tc_scope.exception() {
      if let Some(js_uncaught_exception_cb) = js_uncaught_exception_cb {
        tc_scope.reset(); // Cancel pending exception.
        js_uncaught_exception_cb.open(tc_scope).call(
          tc_scope,
          undefined,
          &[exception],
        );
      }
    }

    if tc_scope.has_caught() {
      // If we get here, an exception was thrown by the unhandledRejection
      // handler and there is ether no uncaughtException handler or the
      // handler threw an exception of its own.
      //
      // TODO(bnoordhuis) Node terminates the process or worker thread
      // but we don't really have that option. The exception won't bubble
      // up either because V8 cancels it when this function returns.
      let exception = tc_scope
        .stack_trace()
        .or_else(|| tc_scope.exception())
        .map(|value| value.to_rust_string_lossy(tc_scope))
        .unwrap_or_else(|| "no exception".into());
      eprintln!("Unhandled exception: {}", exception);
    }
  } else {
    let promise = message.get_promise();
    let promise_global = v8::Global::new(scope, promise);

    match message.get_event() {
      PromiseRejectWithNoHandler => {
        let error = message.get_value().unwrap();
        let error_global = v8::Global::new(scope, error);
        state
          .pending_promise_exceptions
          .insert(promise_global, error_global);
      }
      PromiseHandlerAddedAfterReject => {
        state.pending_promise_exceptions.remove(&promise_global);
      }
      PromiseRejectAfterResolved => {}
      PromiseResolveAfterResolved => {
        // Should not warn. See #1272
      }
    }
  }
}

/// This binding should be used if there's a custom console implementation
/// available. Using it will make sure that proper stack frames are displayed
/// in the inspector console.
///
/// Each method on console object should be bound to this function, eg:
/// ```ignore
/// function wrapConsole(consoleFromDeno, consoleFromV8) {
///   const callConsole = core.callConsole;
///
///   for (const key of Object.keys(consoleFromV8)) {
///     if (consoleFromDeno.hasOwnProperty(key)) {
///       consoleFromDeno[key] = callConsole.bind(
///         consoleFromDeno,
///         consoleFromV8[key],
///         consoleFromDeno[key],
///       );
///     }
///   }
/// }
/// ```
///
/// Inspired by:
/// https://github.com/nodejs/node/blob/1317252dfe8824fd9cfee125d2aaa94004db2f3b/src/inspector_js_api.cc#L194-L222
fn call_console(
  scope: &mut v8::HandleScope,
  args: v8::FunctionCallbackArguments,
  _rv: v8::ReturnValue,
) {
  if args.length() < 2
    || !args.get(0).is_function()
    || !args.get(1).is_function()
  {
    return throw_type_error(scope, "Invalid arguments");
  }

  let mut call_args = vec![];
  for i in 2..args.length() {
    call_args.push(args.get(i));
  }

  let receiver = args.this();
  let inspector_console_method =
    v8::Local::<v8::Function>::try_from(args.get(0)).unwrap();
  let deno_console_method =
    v8::Local::<v8::Function>::try_from(args.get(1)).unwrap();

  inspector_console_method.call(scope, receiver.into(), &call_args);
  deno_console_method.call(scope, receiver.into(), &call_args);
}

pub fn throw_type_error(scope: &mut v8::HandleScope, message: impl AsRef<str>) {
  let message = v8::String::new(scope, message.as_ref()).unwrap();
  let exception = v8::Exception::type_error(scope, message);
  scope.throw_exception(exception);
}
