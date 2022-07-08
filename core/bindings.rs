// Copyright 2018-2022 the Deno authors. All rights reserved. MIT license.

use std::collections::HashMap;
use crate::v8;
use crate::ops::OpCtx;
use std::os::raw::c_void;
use crate::minus_v8::backend::NativeFunctionCallback;

pub fn initialize_context<'s>(
  scope: &mut v8::HandleScope<'s>,
  op_ctxs: &[OpCtx],
) -> v8::Local<'s, v8::Context> {
  let context = v8::Context::new(scope);

  let callbacks = initialize_ops(op_ctxs);
  scope.backend.inject_bridge("Deno.core.ops", callbacks);

  context
}

fn initialize_ops(op_ctxs: &[OpCtx]) -> HashMap<&str, NativeFunctionCallback> {
  let mut callbacks = HashMap::new();
  for ctx in op_ctxs {
    let ctx_ptr = ctx as *const OpCtx as *const c_void;
    callbacks.insert(
      ctx.decl.name,
      NativeFunctionCallback {
        callback: ctx.decl.v8_fn_ptr.clone(),
        user_data: ctx_ptr,
      }
    );
  }
  callbacks
}

// TODO(minus_v8) promise rejection callback
/*
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
*/

pub fn throw_type_error(scope: &mut v8::HandleScope, message: impl AsRef<str>) {
  let message = v8::String::new(scope, message.as_ref()).unwrap();
  let exception = v8::Exception::type_error(scope, message);
  scope.throw_exception(exception);
}
