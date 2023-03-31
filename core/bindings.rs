// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

use std::collections::HashMap;
use std::option::Option;
use std::os::raw::c_void;
use v8::backend::NativeFunctionCallback;
use v8::MapFnTo;

use crate::ops::OpCtx;

pub fn initialize_context<'s>(
  scope: &mut v8::HandleScope<'s>,
  op_ctxs: &[OpCtx],
) -> v8::Local<'s, v8::Context> {
  let context = v8::Context::new(scope);

  let mut callbacks = initialize_ops(op_ctxs);
  initialize_async_ops_info(scope, &mut callbacks, op_ctxs);
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

  let context_state_rc = JsRealm::state_from_scope(scope);
  let mut context_state = context_state_rc.borrow_mut();

  if let Some(js_promise_reject_cb) = context_state.js_promise_reject_cb.clone()
  {
    drop(context_state);

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

    let promise_global = v8::Global::new(tc_scope, promise);
    let args = &[type_.into(), promise.into(), reason];
    let maybe_has_unhandled_rejection_handler = js_promise_reject_cb
      .open(tc_scope)
      .call(tc_scope, undefined, args);

    let has_unhandled_rejection_handler =
      if let Some(value) = maybe_has_unhandled_rejection_handler {
        value.is_true()
      } else {
        false
      };

    if has_unhandled_rejection_handler {
      let state_rc = JsRuntime::state(tc_scope);
      let mut state = state_rc.borrow_mut();
      if let Some(pending_mod_evaluate) = state.pending_mod_evaluate.as_mut() {
        if !pending_mod_evaluate.has_evaluated {
          pending_mod_evaluate
            .handled_promise_rejections
            .push(promise_global);
        }
      }
    }
  } else {
    let promise = message.get_promise();
    let promise_global = v8::Global::new(scope, promise);
    match message.get_event() {
      PromiseRejectWithNoHandler => {
        let error = message.get_value().unwrap();
        let error_global = v8::Global::new(scope, error);
        context_state
          .pending_promise_rejections
          .insert(promise_global, error_global);
      }
      PromiseHandlerAddedAfterReject => {
        context_state
          .pending_promise_rejections
          .remove(&promise_global);
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

struct AsyncOpsInfo {
  ptr: *const OpCtx,
  len: usize,
}

impl<'s> IntoIterator for &'s AsyncOpsInfo {
  type Item = &'s OpCtx;
  type IntoIter = AsyncOpsInfoIterator<'s>;

  fn into_iter(self) -> Self::IntoIter {
    AsyncOpsInfoIterator {
      // SAFETY: OpCtx slice is valid for the lifetime of the Isolate
      info: unsafe { std::slice::from_raw_parts(self.ptr, self.len) },
      index: 0,
    }
  }
}

struct AsyncOpsInfoIterator<'s> {
  info: &'s [OpCtx],
  index: usize,
}

impl<'s> Iterator for AsyncOpsInfoIterator<'s> {
  type Item = &'s OpCtx;

  fn next(&mut self) -> Option<Self::Item> {
    loop {
      match self.info.get(self.index) {
        Some(ctx) if ctx.decl.is_async => {
          self.index += 1;
          return Some(ctx);
        }
        Some(_) => {
          self.index += 1;
        }
        None => return None,
      }
    }
  }
}

fn async_ops_info(
  _scope: &mut v8::HandleScope,
  args: v8::FunctionCallbackArguments,
  rv: &mut v8::ReturnValue,
) {
  let mut async_op_names = HashMap::new();
  let external = args.data;
  let info: &AsyncOpsInfo =
    // SAFETY: external is guaranteed to be a valid pointer to AsyncOpsInfo
    unsafe { &*(external as *const AsyncOpsInfo) };
  for ctx in info {
    async_op_names.insert(ctx.decl.name, ctx.decl.argc as i32);
  }
  rv.set(Box::new(async_op_names));
}

fn initialize_async_ops_info(
  _scope: &mut v8::HandleScope,
  ops_obj: &mut HashMap<&str, NativeFunctionCallback>,
  op_ctxs: &[OpCtx],
) {
  let key = "asyncOpsInfo";
  let external = Box::into_raw(Box::new(AsyncOpsInfo {
    ptr: op_ctxs as *const [OpCtx] as _,
    len: op_ctxs.len(),
  })) as *mut c_void;
  let val = NativeFunctionCallback {
    callback: async_ops_info.map_fn_to(),
    user_data: external,
  };
  ops_obj.insert(key, val);
}
