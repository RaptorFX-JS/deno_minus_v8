// Copyright 2018-2022 the Deno authors. All rights reserved. MIT license.
use crate::v8;
use crate::serde_v8;
use crate::error::not_supported;
use crate::error::range_error;
use crate::error::type_error;
use crate::error::JsError;
use crate::serde_v8::from_v8;
use crate::JsRuntime;
use crate::OpDecl;
use crate::ZeroCopyBuf;
use anyhow::Error;
use deno_ops::op;
use serde::Deserialize;
use serde::Serialize;

pub(crate) fn init_builtins_v8() -> Vec<OpDecl> {
  vec![
    op_ref_op::decl(),
    op_unref_op::decl(),
    op_set_macrotask_callback::decl(),
    op_set_next_tick_callback::decl(),
    op_set_promise_reject_callback::decl(),
    op_set_uncaught_exception_callback::decl(),
    op_run_microtasks::decl(),
    op_has_tick_scheduled::decl(),
    op_set_has_tick_scheduled::decl(),
    op_eval_context::decl(),
    op_queue_microtask::decl(),
    op_create_host_object::decl(),
    op_encode::decl(),
    op_decode::decl(),
    op_serialize::decl(),
    op_deserialize::decl(),
    op_get_promise_details::decl(),
    op_get_proxy_details::decl(),
    op_memory_usage::decl(),
    op_set_wasm_streaming_callback::decl(),
    op_abort_wasm_streaming::decl(),
    op_destructure_error::decl(),
    op_terminate::decl(),
    op_op_names::decl(),
    op_apply_source_map::decl(),
    op_set_format_exception_callback::decl(),
  ]
}

fn to_v8_fn(
  scope: &mut v8::HandleScope,
  value: serde_v8::Value,
) -> Result<v8::Global<v8::Function>, Error> {
  v8::Local::<v8::Function>::try_from(value)
    .map(|cb| v8::Global::new(scope, cb))
    .map_err(|err| type_error(err.to_string()))
}

#[op(v8)]
fn op_ref_op(scope: &mut v8::HandleScope, promise_id: i32) {
  let state_rc = JsRuntime::state(scope);
  state_rc.borrow_mut().unrefed_ops.remove(&promise_id);
}

#[op(v8)]
fn op_unref_op(scope: &mut v8::HandleScope, promise_id: i32) {
  let state_rc = JsRuntime::state(scope);
  state_rc.borrow_mut().unrefed_ops.insert(promise_id);
}

#[op(v8)]
fn op_set_macrotask_callback(
  scope: &mut v8::HandleScope,
  cb: serde_v8::Value,
) -> Result<(), Error> {
  let cb = to_v8_fn(scope, cb)?;
  let state_rc = JsRuntime::state(scope);
  state_rc.borrow_mut().js_macrotask_cbs.push(cb);
  Ok(())
}

#[op(v8)]
fn op_set_next_tick_callback(
  scope: &mut v8::HandleScope,
  cb: serde_v8::Value,
) -> Result<(), Error> {
  let cb = to_v8_fn(scope, cb)?;
  let state_rc = JsRuntime::state(scope);
  state_rc.borrow_mut().js_nexttick_cbs.push(cb);
  Ok(())
}

#[op(v8)]
fn op_set_promise_reject_callback<'a>(
  scope: &mut v8::HandleScope<'a>,
  cb: serde_v8::Value,
) -> Result<Option<serde_v8::Value<'a>>, Error> {
  let cb = to_v8_fn(scope, cb)?;
  let state_rc = JsRuntime::state(scope);
  let old = state_rc.borrow_mut().js_promise_reject_cb.replace(cb);
  let old = old.map(|v| v8::Local::new(scope, v));
  Ok(old.map(|v| from_v8(scope, v.into()).unwrap()))
}

#[op(v8)]
fn op_set_uncaught_exception_callback<'a>(
  scope: &mut v8::HandleScope<'a>,
  cb: serde_v8::Value,
) -> Result<Option<serde_v8::Value<'a>>, Error> {
  let cb = to_v8_fn(scope, cb)?;
  let state_rc = JsRuntime::state(scope);
  let old = state_rc.borrow_mut().js_uncaught_exception_cb.replace(cb);
  let old = old.map(|v| v8::Local::new(scope, v));
  Ok(old.map(|v| from_v8(scope, v.into()).unwrap()))
}

#[op(v8)]
fn op_run_microtasks(scope: &mut v8::HandleScope) -> Result<(), Error> {
  // TODO(minus_v8) is there a way for us to force run microtasks?
  Err(not_supported())
}

#[op(v8)]
fn op_has_tick_scheduled(scope: &mut v8::HandleScope) -> bool {
  let state_rc = JsRuntime::state(scope);
  let state = state_rc.borrow();
  state.has_tick_scheduled
}

#[op(v8)]
fn op_set_has_tick_scheduled(scope: &mut v8::HandleScope, v: bool) {
  let state_rc = JsRuntime::state(scope);
  state_rc.borrow_mut().has_tick_scheduled = v;
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EvalContextError<'s> {
  thrown: serde_v8::Value<'s>,
  is_native_error: bool,
  is_compile_error: bool,
}

#[derive(Serialize)]
struct EvalContextResult<'s>(
  Option<serde_v8::Value<'s>>,
  Option<EvalContextError<'s>>,
);

#[op(v8)]
fn op_eval_context<'a>(
  scope: &mut v8::HandleScope<'a>,
  source: serde_v8::Value<'a>,
  specifier: Option<String>,
) -> Result<EvalContextResult<'a>, Error> {
  // TODO(minus_v8) can we polyfill this?
  Err(not_supported())
}

#[op(v8)]
fn op_queue_microtask(
  scope: &mut v8::HandleScope,
  cb: serde_v8::Value,
) -> Result<(), Error> {
  // TODO(minus_v8) delegate to backend `queueMicrotask`
  Err(not_supported())
}

#[op(v8)]
fn op_create_host_object<'a>(
  scope: &mut v8::HandleScope<'a>,
) -> Result<serde_v8::Value<'a>, Error> {
  // TODO(minus_v8) can we just implement this with `return {}`?
  Err(not_supported())
}

#[op(v8)]
fn op_encode<'a>(
  scope: &mut v8::HandleScope<'a>,
  text: serde_v8::Value<'a>,
) -> Result<Vec<u8>, Error> {
  let text = v8::Local::<v8::String>::try_from(text)
    .map_err(|_| type_error("Invalid argument"))?;
  let u8array = text.0.clone().into_bytes();
  Ok(u8array)
}

#[op(v8)]
fn op_decode<'a>(
  scope: &mut v8::HandleScope<'a>,
  zero_copy: ZeroCopyBuf,
) -> Result<v8::String, Error> {
  let buf = &zero_copy;

  // Strip BOM
  let buf =
    if buf.len() >= 3 && buf[0] == 0xef && buf[1] == 0xbb && buf[2] == 0xbf {
      &buf[3..]
    } else {
      buf
    };

  // If `String::new_from_utf8()` returns `None`, this means that the
  // length of the decoded string would be longer than what V8 can
  // handle. In this case we return `RangeError`.
  //
  // For more details see:
  // - https://encoding.spec.whatwg.org/#dom-textdecoder-decode
  // - https://github.com/denoland/deno/issues/6649
  // - https://github.com/v8/v8/blob/d68fb4733e39525f9ff0a9222107c02c28096e2a/include/v8.h#L3277-L3278
  match v8::String::new_from_utf8(scope, buf) {
    Some(text) => Ok(text),
    None => Err(range_error("string too long")),
  }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SerializeDeserializeOptions {}

#[op(v8)]
fn op_serialize(
  scope: &mut v8::HandleScope,
  value: serde_v8::Value,
  options: Option<SerializeDeserializeOptions>,
  error_callback: Option<serde_v8::Value>,
) -> Result<ZeroCopyBuf, Error> {
  // op_serialize and op_deserialize are internal implementation details of Deno
  // which are used to implement `structuredClone` and a few web APIs that we
  // don't need to implement
  Err(not_supported())
}

#[op(v8)]
fn op_deserialize<'a>(
  scope: &mut v8::HandleScope<'a>,
  zero_copy: ZeroCopyBuf,
  options: Option<SerializeDeserializeOptions>,
) -> Result<serde_v8::Value<'a>, Error> {
  // see comment in op_serialize
  Err(not_supported())
}

#[derive(Serialize)]
struct PromiseDetails<'s>(u32, Option<serde_v8::Value<'s>>);

#[op(v8)]
fn op_get_promise_details<'a>(
  scope: &mut v8::HandleScope<'a>,
  promise: serde_v8::Value<'a>,
) -> Result<PromiseDetails<'a>, Error> {
  // TODO(minus_v8) replace with some sort of JS polyfill
  Err(not_supported())
}

// Based on https://github.com/nodejs/node/blob/1e470510ff74391d7d4ec382909ea8960d2d2fbc/src/node_util.cc
// Copyright Joyent, Inc. and other Node contributors.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the
// "Software"), to deal in the Software without restriction, including
// without limitation the rights to use, copy, modify, merge, publish,
// distribute, sublicense, and/or sell copies of the Software, and to permit
// persons to whom the Software is furnished to do so, subject to the
// following conditions:
//
// The above copyright notice and this permission notice shall be included
// in all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
// DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
// OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE
// USE OR OTHER DEALINGS IN THE SOFTWARE.
#[op(v8)]
fn op_get_proxy_details<'a>(
  scope: &mut v8::HandleScope<'a>,
  proxy: serde_v8::Value<'a>,
) -> Option<(serde_v8::Value<'a>, serde_v8::Value<'a>)> {
  // TODO(minus_v8) replace with some sort of JS polyfill
  // should return Some((target, handler))
  None
}

// HeapStats stores values from a isolate.get_heap_statistics() call
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryUsage {
  pub rss: usize,
  pub heap_total: usize,
  pub heap_used: usize,
  pub external: usize,
  // TODO: track ArrayBuffers, would require using a custom allocator to track
  // but it's otherwise a subset of external so can be indirectly tracked
  // array_buffers: usize,
}

#[op(v8)]
fn op_memory_usage(scope: &mut v8::HandleScope) -> MemoryUsage {
  scope.backend.get_memory_usage()
}

#[op(v8)]
fn op_set_wasm_streaming_callback(
  scope: &mut v8::HandleScope,
  cb: serde_v8::Value,
) -> Result<(), Error> {
  // we rely on the backend runtime for WASM support
  Err(not_supported())
}

#[op(v8)]
fn op_abort_wasm_streaming(
  scope: &mut v8::HandleScope,
  rid: u32,
  error: serde_v8::Value,
) -> Result<(), Error> {
  // we rely on the backend runtime for WASM support
  Err(not_supported())
}

#[op(v8)]
fn op_destructure_error(
  scope: &mut v8::HandleScope,
  error: serde_v8::Value,
) -> Result<JsError, Error> {
  // this seems to be only used to implement the web APIs
  Err(not_supported())
}

#[op(v8)]
fn op_terminate(scope: &mut v8::HandleScope, exception: serde_v8::Value) {
  let state_rc = JsRuntime::state(scope);
  let mut state = state_rc.borrow_mut();
  state.explicit_terminate_exception = unsafe {
    Some(v8::Global::from_raw(scope, exception.try_into().unwrap()).unwrap())
  };
  scope.terminate_execution();
}

#[op(v8)]
fn op_op_names(scope: &mut v8::HandleScope) -> Vec<String> {
  let state_rc = JsRuntime::state(scope);
  let state = state_rc.borrow();
  state
    .op_ctxs
    .iter()
    .map(|o| o.decl.name.to_string())
    .collect()
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Location {
  file_name: String,
  line_number: u32,
  column_number: u32,
}

#[op(v8)]
fn op_apply_source_map(
  scope: &mut v8::HandleScope,
  location: Location,
) -> Result<Location, Error> {
  // internal implementation detail of `02_error.js` which itself is only implementable on V8
  Err(not_supported())
}

/// Set a callback which formats exception messages as stored in
/// `JsError::exception_message`. The callback is passed the error value and
/// should return a string or `null`. If no callback is set or the callback
/// returns `null`, the built-in default formatting will be used.
#[op(v8)]
fn op_set_format_exception_callback<'a>(
  scope: &mut v8::HandleScope<'a>,
  cb: serde_v8::Value<'a>,
) -> Result<Option<serde_v8::Value<'a>>, Error> {
  let cb = to_v8_fn(scope, cb)?;
  let state_rc = JsRuntime::state(scope);
  let old = state_rc.borrow_mut().js_format_exception_cb.replace(cb);
  let old = old.map(|v| v8::Local::new(scope, v));
  Ok(old.map(|v| from_v8(scope, v.into()).unwrap()))
}
