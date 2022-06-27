// Copyright 2018-2022 the Deno authors. All rights reserved. MIT license.

use crate::bindings;
use crate::error::JsError;
use crate::extensions::OpDecl;
use crate::extensions::OpEventLoopFn;
use crate::op_void_async;
use crate::op_void_sync;
use crate::ops::*;
use crate::Extension;
use crate::OpMiddlewareFn;
use crate::OpResult;
use crate::OpState;
use crate::PromiseId;
use anyhow::Error;
use futures::future::poll_fn;
use futures::future::Future;
use futures::future::FutureExt;
use futures::stream::FuturesUnordered;
use futures::stream::StreamExt;
use futures::task::AtomicWaker;
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::option::Option;
use std::rc::Rc;
use std::task::Context;
use std::task::Poll;

type PendingOpFuture = OpCall<(PromiseId, OpId, OpResult)>;

pub type GetErrorClassFn = &'static dyn for<'e> Fn(&'e Error) -> &'static str;

/// A single execution context of JavaScript. Corresponds roughly to the "Web
/// Worker" concept in the DOM. A JsRuntime is a Future that can be used with
/// an event loop (Tokio, async_std).
////
/// The JsRuntime future completes when there is an error or when all
/// pending ops have completed.
///
/// Pending ops are created in JavaScript by calling Deno.core.opAsync(), and in Rust
/// by implementing an async function that takes a serde::Deserialize "control argument"
/// and an optional zero copy buffer, each async Op is tied to a Promise in JavaScript.
pub struct JsRuntime {
  extensions: Vec<Extension>,
  event_loop_middlewares: Vec<Box<OpEventLoopFn>>,
}

/// Internal state for JsRuntime which is stored in one of v8::Isolate's
/// embedder slots.
pub(crate) struct JsRuntimeState {
  global_realm: Option<JsRealm>,
  pub(crate) js_recv_cb: Option<v8::Global<v8::Function>>,
  pub(crate) js_macrotask_cbs: Vec<v8::Global<v8::Function>>,
  pub(crate) js_nexttick_cbs: Vec<v8::Global<v8::Function>>,
  pub(crate) js_promise_reject_cb: Option<v8::Global<v8::Function>>,
  pub(crate) js_uncaught_exception_cb: Option<v8::Global<v8::Function>>,
  pub(crate) js_format_exception_cb: Option<v8::Global<v8::Function>>,
  pub(crate) has_tick_scheduled: bool,
  pub(crate) pending_promise_exceptions: HashMap<v8::Global<v8::Promise>, v8::Global<v8::Value>>,
  pub(crate) pending_ops: FuturesUnordered<PendingOpFuture>,
  pub(crate) unrefed_ops: HashSet<i32>,
  pub(crate) have_unpolled_ops: bool,
  pub(crate) op_state: Rc<RefCell<OpState>>,
  #[allow(dead_code)]
  // We don't explicitly re-read this prop but need the slice to live alongside the isolate
  pub(crate) op_ctxs: Box<[OpCtx]>,
  /// The error that was passed to an explicit `Deno.core.terminate` call.
  /// It will be retrieved by `exception_to_err_result` and used as an error
  /// instead of any other exceptions.
  pub(crate) explicit_terminate_exception: Option<v8::Global<v8::Value>>,
  waker: AtomicWaker,
}

#[derive(Default)]
pub struct RuntimeOptions {
  /// Allows to map error type to a string "class" used to represent
  /// error in JavaScript.
  pub get_error_class_fn: Option<GetErrorClassFn>,

  /// JsRuntime extensions, not to be confused with ES modules
  /// these are sets of ops and other JS code to be initialized.
  pub extensions: Vec<Extension>,
}

impl JsRuntime {
  /// Only constructor, configuration is done through `options`.
  pub fn new(mut options: RuntimeOptions) -> Self {
    // Add builtins extension
    options
      .extensions
      .insert(0, crate::ops_builtin::init_builtins());

    let ops = Self::collect_ops(&mut options.extensions);
    let mut op_state = OpState::new(ops.len());

    if let Some(get_error_class_fn) = options.get_error_class_fn {
      op_state.get_error_class_fn = get_error_class_fn;
    }

    let op_state = Rc::new(RefCell::new(op_state));
    let op_ctxs = ops
      .into_iter()
      .enumerate()
      .map(|(id, decl)| OpCtx {
        id,
        state: op_state.clone(),
        decl,
      })
      .collect::<Vec<_>>()
      .into_boxed_slice();

    let global_context;
    let mut isolate = {
      let isolate = v8::Isolate::new(params);
      let mut isolate = JsRuntime::setup_isolate(isolate);
      {
        let scope = &mut v8::HandleScope::new(&mut isolate);
        let context = bindings::initialize_context(scope, &op_ctxs);

        global_context = v8::Global::new(scope, context);
      }
      isolate
    };

    isolate.set_slot(Rc::new(RefCell::new(JsRuntimeState {
      global_realm: Some(JsRealm(global_context)),
      pending_promise_exceptions: HashMap::new(),
      js_recv_cb: None,
      js_macrotask_cbs: vec![],
      js_nexttick_cbs: vec![],
      js_promise_reject_cb: None,
      js_uncaught_exception_cb: None,
      js_format_exception_cb: None,
      has_tick_scheduled: false,
      pending_ops: FuturesUnordered::new(),
      unrefed_ops: HashSet::new(),
      op_state: op_state.clone(),
      op_ctxs,
      have_unpolled_ops: false,
      explicit_terminate_exception: None,
      waker: AtomicWaker::new(),
    })));

    let mut js_runtime = Self {
      event_loop_middlewares: Vec::with_capacity(options.extensions.len()),
      extensions: options.extensions,
    };

    // TODO(@AaronO): diff extensions inited in snapshot and those provided
    // for now we assume that snapshot and extensions always match
    let realm = js_runtime.global_realm();
    js_runtime.init_extension_js(&realm).unwrap();
    // Init extension ops
    js_runtime.init_extension_ops().unwrap();
    // Init callbacks (opresolve)
    js_runtime.init_cbs();

    js_runtime
  }

  pub fn global_context(&mut self) -> v8::Global<v8::Context> {
    self.global_realm().0
  }

  pub fn v8_isolate(&mut self) -> &mut v8::OwnedIsolate {
    self.v8_isolate.as_mut().unwrap()
  }

  pub fn global_realm(&mut self) -> JsRealm {
    let state = Self::state(self.v8_isolate());
    let state = state.borrow();
    state.global_realm.clone().unwrap()
  }

  pub fn handle_scope(&mut self) -> v8::HandleScope {
    self.global_realm().handle_scope(self)
  }

  fn setup_isolate(mut isolate: v8::OwnedIsolate) -> v8::OwnedIsolate {
    isolate.set_capture_stack_trace_for_uncaught_exceptions(true, 10);
    isolate.set_promise_reject_callback(bindings::promise_reject_callback);
    isolate
  }

  pub(crate) fn state(isolate: &v8::Isolate) -> Rc<RefCell<JsRuntimeState>> {
    let s = isolate.get_slot::<Rc<RefCell<JsRuntimeState>>>().unwrap();
    s.clone()
  }

  /// Initializes JS of provided Extensions in the given realm
  fn init_extension_js(&mut self, realm: &JsRealm) -> Result<(), Error> {
    // Take extensions to avoid double-borrow
    let mut extensions: Vec<Extension> = std::mem::take(&mut self.extensions);
    for m in extensions.iter_mut() {
      let js_files = m.init_js();
      for (filename, source) in js_files {
        // TODO(@AaronO): use JsRuntime::execute_static() here to move src off heap
        realm.execute_script(self, filename, source)?;
      }
    }
    // Restore extensions
    self.extensions = extensions;

    Ok(())
  }

  /// Collects ops from extensions & applies middleware
  fn collect_ops(extensions: &mut [Extension]) -> Vec<OpDecl> {
    // Middleware
    let middleware: Vec<Box<OpMiddlewareFn>> = extensions
      .iter_mut()
      .filter_map(|e| e.init_middleware())
      .collect();

    // macroware wraps an opfn in all the middleware
    let macroware = move |d| middleware.iter().fold(d, |d, m| m(d));

    // Flatten ops, apply middlware & override disabled ops
    extensions
      .iter_mut()
      .filter_map(|e| e.init_ops())
      .flatten()
      .map(|d| OpDecl {
        name: d.name,
        ..macroware(d)
      })
      .map(|op| match op.enabled {
        true => op,
        false => OpDecl {
          v8_fn_ptr: match op.is_async {
            true => op_void_async::v8_fn_ptr(),
            false => op_void_sync::v8_fn_ptr(),
          },
          ..op
        },
      })
      .collect()
  }

  /// Initializes ops of provided Extensions
  fn init_extension_ops(&mut self) -> Result<(), Error> {
    let op_state = self.op_state();
    // Take extensions to avoid double-borrow
    let mut extensions: Vec<Extension> = std::mem::take(&mut self.extensions);

    // Setup state
    for e in extensions.iter_mut() {
      // ops are already registered during in bindings::initialize_context();
      e.init_state(&mut op_state.borrow_mut())?;

      // Setup event-loop middleware
      if let Some(middleware) = e.init_event_loop_middleware() {
        self.event_loop_middlewares.push(middleware);
      }
    }

    // Restore extensions
    self.extensions = extensions;

    Ok(())
  }

  /// Grab a Global handle to a v8 value returned by the expression
  pub(crate) fn grab<'s, T>(
    scope: &mut v8::HandleScope<'s>,
    root: v8::Local<'s, v8::Value>,
    path: &str,
  ) -> Option<v8::Local<'s, T>>
  where
    v8::Local<'s, T>: TryFrom<v8::Local<'s, v8::Value>, Error = v8::DataError>,
  {
    path
      .split('.')
      .fold(Some(root), |p, k| {
        let p = v8::Local::<v8::Object>::try_from(p?).ok()?;
        let k = v8::String::new(scope, k)?;
        p.get(scope, k.into())
      })?
      .try_into()
      .ok()
  }

  pub(crate) fn grab_global<'s, T>(
    scope: &mut v8::HandleScope<'s>,
    path: &str,
  ) -> Option<v8::Local<'s, T>>
  where
    v8::Local<'s, T>: TryFrom<v8::Local<'s, v8::Value>, Error = v8::DataError>,
  {
    let context = scope.get_current_context();
    let global = context.global(scope);
    Self::grab(scope, global.into(), path)
  }

  pub(crate) fn ensure_objs<'s>(
    scope: &mut v8::HandleScope<'s>,
    root: v8::Local<'s, v8::Object>,
    path: &str,
  ) -> Option<v8::Local<'s, v8::Object>> {
    path.split('.').fold(Some(root), |p, k| {
      let k = v8::String::new(scope, k)?.into();
      match p?.get(scope, k) {
        Some(v) if !v.is_null_or_undefined() => v.try_into().ok(),
        _ => {
          let o = v8::Object::new(scope);
          p?.set(scope, k, o.into());
          Some(o)
        }
      }
    })
  }

  /// Grabs a reference to core.js' opresolve & syncOpsCache()
  fn init_cbs(&mut self) {
    let scope = &mut self.handle_scope();
    let recv_cb =
      Self::grab_global::<v8::Function>(scope, "Deno.core.opresolve").unwrap();
    let recv_cb = v8::Global::new(scope, recv_cb);
    // Put global handles in state
    let state_rc = JsRuntime::state(scope);
    let mut state = state_rc.borrow_mut();
    state.js_recv_cb.replace(recv_cb);
  }

  /// Returns the runtime's op state, which can be used to maintain ops
  /// and access resources between op calls.
  pub fn op_state(&mut self) -> Rc<RefCell<OpState>> {
    let state_rc = Self::state(self.v8_isolate());
    let state = state_rc.borrow();
    state.op_state.clone()
  }

  /// Executes traditional JavaScript code (traditional = not ES modules).
  ///
  /// The execution takes place on the current global context, so it is possible
  /// to maintain local JS state and invoke this method multiple times.
  ///
  /// `name` can be a filepath or any other string, eg.
  ///
  ///   - "/some/file/path.js"
  ///   - "<anon>"
  ///   - "[native code]"
  ///
  /// The same `name` value can be used for multiple executions.
  ///
  /// `Error` can usually be downcast to `JsError`.
  pub fn execute_script(
    &mut self,
    name: &str,
    source_code: &str,
  ) -> Result<v8::Global<v8::Value>, Error> {
    self.global_realm().execute_script(self, name, source_code)
  }

  /// Runs event loop to completion
  ///
  /// This future resolves when:
  ///  - there are no more pending dynamic imports
  ///  - there are no more pending ops
  ///  - there are no more active inspector sessions (only if `wait_for_inspector` is set to true)
  pub async fn run_event_loop(
    &mut self,
    _wait_for_inspector: bool,
  ) -> Result<(), Error> {
    poll_fn(|cx| self.poll_event_loop(cx)).await
  }

  /// Runs a single tick of event loop
  ///
  /// If `wait_for_inspector` is set to true event loop
  /// will return `Poll::Pending` if there are active inspector sessions.
  pub fn poll_event_loop(
    &mut self,
    cx: &mut Context,
  ) -> Poll<Result<(), Error>> {
    let state_rc = Self::state(self.v8_isolate());
    {
      let state = state_rc.borrow();
      state.waker.register(cx.waker());
    }

    // Ops
    {
      self.resolve_async_ops(cx)?;
      self.drain_nexttick()?;
      self.drain_macrotasks()?;
      self.check_promise_exceptions()?;
    }

    // Event loop middlewares
    let mut maybe_scheduling = false;
    {
      let state = state_rc.borrow();
      let op_state = state.op_state.clone();
      for f in &self.event_loop_middlewares {
        if f(&mut op_state.borrow_mut(), cx) {
          maybe_scheduling = true;
        }
      }
    }

    let mut state = state_rc.borrow_mut();

    let has_pending_refed_ops =
      state.pending_ops.len() > state.unrefed_ops.len();
    let has_tick_scheduled = state.has_tick_scheduled;

    if !has_pending_refed_ops
      && !has_tick_scheduled
      && !maybe_scheduling
    {
      return Poll::Ready(Ok(()));
    }

    // Check if more async ops have been dispatched
    // during this turn of event loop.
    // If there are any pending background tasks, we also wake the runtime to
    // make sure we don't miss them.
    // TODO(andreubotella) The event loop will spin as long as there are pending
    // background tasks. We should look into having V8 notify us when a
    // background task is done.
    if state.have_unpolled_ops
      || has_tick_scheduled
      || maybe_scheduling
    {
      state.waker.wake();
    }

    Poll::Pending
  }
}

impl JsRuntimeState {
  /// Called by `bindings::host_import_module_dynamically_callback`
  /// after initiating new dynamic import load.
  pub fn notify_new_dynamic_import(&mut self) {
    // Notify event loop to poll again soon.
    self.waker.wake();
  }
}

pub(crate) fn exception_to_err_result<'s, T>(
  scope: &mut v8::HandleScope<'s>,
  exception: v8::Local<v8::Value>,
  in_promise: bool,
) -> Result<T, Error> {
  let state_rc = JsRuntime::state(scope);

  let is_terminating_exception = scope.is_execution_terminating();
  let mut exception = exception;

  if is_terminating_exception {
    // TerminateExecution was called. Cancel isolate termination so that the
    // exception can be created..
    scope.cancel_terminate_execution();

    // If the termination is the result of a `Deno.core.terminate` call, we want
    // to use the exception that was passed to it rather than the exception that
    // was passed to this function.
    let mut state = state_rc.borrow_mut();
    exception = state
      .explicit_terminate_exception
      .take()
      .map(|exception| v8::Local::new(scope, exception))
      .unwrap_or_else(|| {
        // Maybe make a new exception object.
        if exception.is_null_or_undefined() {
          let message = v8::String::new(scope, "execution terminated").unwrap();
          v8::Exception::error(scope, message)
        } else {
          exception
        }
      });
  }

  let mut js_error = JsError::from_v8_exception(scope, exception);
  if in_promise {
    js_error.exception_message = format!(
      "Uncaught (in promise) {}",
      js_error.exception_message.trim_start_matches("Uncaught ")
    );
  }

  if is_terminating_exception {
    // Re-enable exception termination.
    scope.terminate_execution();
  }

  Err(js_error.into())
}

// Related to module loading
impl JsRuntime {
  fn check_promise_exceptions(&mut self) -> Result<(), Error> {
    let state_rc = Self::state(self.v8_isolate());
    let mut state = state_rc.borrow_mut();

    if state.pending_promise_exceptions.is_empty() {
      return Ok(());
    }

    let key = {
      state
        .pending_promise_exceptions
        .keys()
        .next()
        .unwrap()
        .clone()
    };
    let handle = state.pending_promise_exceptions.remove(&key).unwrap();
    drop(state);

    let scope = &mut self.handle_scope();
    let exception = v8::Local::new(scope, handle);
    exception_to_err_result(scope, exception, true)
  }

  // Send finished responses to JS
  fn resolve_async_ops(&mut self, cx: &mut Context) -> Result<(), Error> {
    let state_rc = Self::state(self.v8_isolate());

    let js_recv_cb_handle = state_rc.borrow().js_recv_cb.clone().unwrap();
    let scope = &mut self.handle_scope();

    // We return async responses to JS in unbounded batches (may change),
    // each batch is a flat vector of tuples:
    // `[promise_id1, op_result1, promise_id2, op_result2, ...]`
    // promise_id is a simple integer, op_result is an ops::OpResult
    // which contains a value OR an error, encoded as a tuple.
    // This batch is received in JS via the special `arguments` variable
    // and then each tuple is used to resolve or reject promises
    let mut args: Vec<v8::Local<v8::Value>> = vec![];

    // Now handle actual ops.
    {
      let mut state = state_rc.borrow_mut();
      state.have_unpolled_ops = false;

      while let Poll::Ready(Some(item)) = state.pending_ops.poll_next_unpin(cx)
      {
        let (promise_id, op_id, resp) = item;
        state.unrefed_ops.remove(&promise_id);
        state.op_state.borrow().tracker.track_async_completed(op_id);
        args.push(v8::Integer::new(scope, promise_id as i32).into());
        args.push(resp.to_v8(scope).unwrap());
      }
    }

    if args.is_empty() {
      return Ok(());
    }

    let tc_scope = &mut v8::TryCatch::new(scope);
    let js_recv_cb = js_recv_cb_handle.open(tc_scope);
    let this = v8::undefined(tc_scope).into();
    js_recv_cb.call(tc_scope, this, args.as_slice());

    match tc_scope.exception() {
      None => Ok(()),
      Some(exception) => exception_to_err_result(tc_scope, exception, false),
    }
  }

  fn drain_macrotasks(&mut self) -> Result<(), Error> {
    let state = Self::state(self.v8_isolate());

    if state.borrow().js_macrotask_cbs.is_empty() {
      return Ok(());
    }

    let js_macrotask_cb_handles = state.borrow().js_macrotask_cbs.clone();
    let scope = &mut self.handle_scope();

    for js_macrotask_cb_handle in js_macrotask_cb_handles {
      let js_macrotask_cb = js_macrotask_cb_handle.open(scope);

      // Repeatedly invoke macrotask callback until it returns true (done),
      // such that ready microtasks would be automatically run before
      // next macrotask is processed.
      let tc_scope = &mut v8::TryCatch::new(scope);
      let this = v8::undefined(tc_scope).into();
      loop {
        let is_done = js_macrotask_cb.call(tc_scope, this, &[]);

        if let Some(exception) = tc_scope.exception() {
          return exception_to_err_result(tc_scope, exception, false);
        }

        if tc_scope.has_terminated() || tc_scope.is_execution_terminating() {
          return Ok(());
        }

        let is_done = is_done.unwrap();
        if is_done.is_true() {
          break;
        }
      }
    }

    Ok(())
  }

  fn drain_nexttick(&mut self) -> Result<(), Error> {
    let state = Self::state(self.v8_isolate());

    if state.borrow().js_nexttick_cbs.is_empty() {
      return Ok(());
    }

    if !state.borrow().has_tick_scheduled {
      let scope = &mut self.handle_scope();
      scope.perform_microtask_checkpoint();
    }

    // TODO(bartlomieju): Node also checks for absence of "rejection_to_warn"
    if !state.borrow().has_tick_scheduled {
      return Ok(());
    }

    let js_nexttick_cb_handles = state.borrow().js_nexttick_cbs.clone();
    let scope = &mut self.handle_scope();

    for js_nexttick_cb_handle in js_nexttick_cb_handles {
      let js_nexttick_cb = js_nexttick_cb_handle.open(scope);

      let tc_scope = &mut v8::TryCatch::new(scope);
      let this = v8::undefined(tc_scope).into();
      js_nexttick_cb.call(tc_scope, this, &[]);

      if let Some(exception) = tc_scope.exception() {
        return exception_to_err_result(tc_scope, exception, false);
      }
    }

    Ok(())
  }
}

/// A representation of a JavaScript realm tied to a [`JsRuntime`], that allows
/// execution in the realm's context.
///
/// A [`JsRealm`] instance does not hold ownership of its corresponding realm,
/// so they can be created and dropped as needed. And since every operation on
/// them requires passing a mutable reference to the [`JsRuntime`], multiple
/// [`JsRealm`] instances won't overlap.
///
/// # Panics
///
/// Every method of [`JsRealm`] will panic if you call if with a reference to a
/// [`JsRuntime`] other than the one that corresponds to the current context.
///
/// # Lifetime of the realm
///
/// A [`JsRealm`] instance will keep the underlying V8 context alive even if it
/// would have otherwise been garbage collected.
#[derive(Clone)]
pub struct JsRealm(v8::Global<v8::Context>);
impl JsRealm {
  pub fn new(context: v8::Global<v8::Context>) -> Self {
    JsRealm(context)
  }

  pub fn context(&self) -> &v8::Global<v8::Context> {
    &self.0
  }

  pub fn handle_scope<'s>(
    &self,
    runtime: &'s mut JsRuntime,
  ) -> v8::HandleScope<'s> {
    v8::HandleScope::with_context(runtime.v8_isolate(), &self.0)
  }

  pub fn global_object<'s>(
    &self,
    runtime: &'s mut JsRuntime,
  ) -> v8::Local<'s, v8::Object> {
    let scope = &mut self.handle_scope(runtime);
    self.0.open(scope).global(scope)
  }

  /// Executes traditional JavaScript code (traditional = not ES modules) in the
  /// realm's context.
  ///
  /// `name` can be a filepath or any other string, eg.
  ///
  ///   - "/some/file/path.js"
  ///   - "<anon>"
  ///   - "[native code]"
  ///
  /// The same `name` value can be used for multiple executions.
  ///
  /// `Error` can usually be downcast to `JsError`.
  pub fn execute_script(
    &self,
    runtime: &mut JsRuntime,
    name: &str,
    source_code: &str,
  ) -> Result<v8::Global<v8::Value>, Error> {
    let scope = &mut self.handle_scope(runtime);

    let source = v8::String::new(scope, source_code).unwrap();
    let name = v8::String::new(scope, name).unwrap();
    let origin = bindings::script_origin(scope, name);

    let tc_scope = &mut v8::TryCatch::new(scope);

    let script = match v8::Script::compile(tc_scope, source, Some(&origin)) {
      Some(script) => script,
      None => {
        let exception = tc_scope.exception().unwrap();
        return exception_to_err_result(tc_scope, exception, false);
      }
    };

    match script.run(tc_scope) {
      Some(value) => {
        let value_handle = v8::Global::new(tc_scope, value);
        Ok(value_handle)
      }
      None => {
        assert!(tc_scope.has_caught());
        let exception = tc_scope.exception().unwrap();
        exception_to_err_result(tc_scope, exception, false)
      }
    }
  }
}

#[inline]
pub fn queue_async_op(
  scope: &v8::Isolate,
  op: impl Future<Output = (PromiseId, OpId, OpResult)> + 'static,
) {
  let state_rc = JsRuntime::state(scope);
  let mut state = state_rc.borrow_mut();
  state.pending_ops.push(OpCall::eager(op));
  state.have_unpolled_ops = true;
}

#[cfg(test)]
pub mod tests {
  use super::*;
  use crate::error::custom_error;
  use crate::error::AnyError;
  use crate::ZeroCopyBuf;
  use deno_ops::op;
  use futures::future::lazy;
  use std::ops::FnOnce;
  use std::rc::Rc;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::sync::Arc;
  // deno_ops macros generate code assuming deno_core in scope.
  mod deno_core {
    pub use crate::*;
  }

  pub fn run_in_task<F>(f: F)
  where
    F: FnOnce(&mut Context) + Send + 'static,
  {
    futures::executor::block_on(lazy(move |cx| f(cx)));
  }

  #[derive(Copy, Clone)]
  enum Mode {
    Async,
    AsyncZeroCopy(bool),
  }

  struct TestState {
    mode: Mode,
    dispatch_count: Arc<AtomicUsize>,
  }

  #[op]
  async fn op_test(
    rc_op_state: Rc<RefCell<OpState>>,
    control: u8,
    buf: Option<ZeroCopyBuf>,
  ) -> Result<u8, AnyError> {
    let op_state_ = rc_op_state.borrow();
    let test_state = op_state_.borrow::<TestState>();
    test_state.dispatch_count.fetch_add(1, Ordering::Relaxed);
    match test_state.mode {
      Mode::Async => {
        assert_eq!(control, 42);
        Ok(43)
      }
      Mode::AsyncZeroCopy(has_buffer) => {
        assert_eq!(buf.is_some(), has_buffer);
        if let Some(buf) = buf {
          assert_eq!(buf.len(), 1);
        }
        Ok(43)
      }
    }
  }

  fn setup(mode: Mode) -> (JsRuntime, Arc<AtomicUsize>) {
    let dispatch_count = Arc::new(AtomicUsize::new(0));
    let dispatch_count2 = dispatch_count.clone();
    let ext = Extension::builder()
      .ops(vec![op_test::decl()])
      .state(move |state| {
        state.put(TestState {
          mode,
          dispatch_count: dispatch_count2.clone(),
        });
        Ok(())
      })
      .build();
    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![ext],
      get_error_class_fn: Some(&|error| {
        crate::error::get_custom_error_class(error).unwrap()
      }),
      ..Default::default()
    });

    runtime
      .execute_script(
        "setup.js",
        r#"
        function assert(cond) {
          if (!cond) {
            throw Error("assert");
          }
        }
        "#,
      )
      .unwrap();
    assert_eq!(dispatch_count.load(Ordering::Relaxed), 0);
    (runtime, dispatch_count)
  }

  #[test]
  fn test_dispatch() {
    let (mut runtime, dispatch_count) = setup(Mode::Async);
    runtime
      .execute_script(
        "filename.js",
        r#"
        let control = 42;
        Deno.core.opAsync("op_test", control);
        async function main() {
          Deno.core.opAsync("op_test", control);
        }
        main();
        "#,
      )
      .unwrap();
    assert_eq!(dispatch_count.load(Ordering::Relaxed), 2);
  }

  #[test]
  fn test_op_async_promise_id() {
    let (mut runtime, _dispatch_count) = setup(Mode::Async);
    runtime
      .execute_script(
        "filename.js",
        r#"
        const p = Deno.core.opAsync("op_test", 42);
        if (p[Symbol.for("Deno.core.internalPromiseId")] == undefined) {
          throw new Error("missing id on returned promise");
        }
        "#,
      )
      .unwrap();
  }

  #[test]
  fn test_ref_unref_ops() {
    let (mut runtime, _dispatch_count) = setup(Mode::Async);
    runtime
      .execute_script(
        "filename.js",
        r#"
        var promiseIdSymbol = Symbol.for("Deno.core.internalPromiseId");
        var p1 = Deno.core.opAsync("op_test", 42);
        var p2 = Deno.core.opAsync("op_test", 42);
        "#,
      )
      .unwrap();
    {
      let isolate = runtime.v8_isolate();
      let state_rc = JsRuntime::state(isolate);
      let state = state_rc.borrow();
      assert_eq!(state.pending_ops.len(), 2);
      assert_eq!(state.unrefed_ops.len(), 0);
    }
    runtime
      .execute_script(
        "filename.js",
        r#"
        Deno.core.opSync("op_unref_op", p1[promiseIdSymbol]);
        Deno.core.opSync("op_unref_op", p2[promiseIdSymbol]);
        "#,
      )
      .unwrap();
    {
      let isolate = runtime.v8_isolate();
      let state_rc = JsRuntime::state(isolate);
      let state = state_rc.borrow();
      assert_eq!(state.pending_ops.len(), 2);
      assert_eq!(state.unrefed_ops.len(), 2);
    }
    runtime
      .execute_script(
        "filename.js",
        r#"
        Deno.core.opSync("op_ref_op", p1[promiseIdSymbol]);
        Deno.core.opSync("op_ref_op", p2[promiseIdSymbol]);
        "#,
      )
      .unwrap();
    {
      let isolate = runtime.v8_isolate();
      let state_rc = JsRuntime::state(isolate);
      let state = state_rc.borrow();
      assert_eq!(state.pending_ops.len(), 2);
      assert_eq!(state.unrefed_ops.len(), 0);
    }
  }

  #[test]
  fn test_dispatch_no_zero_copy_buf() {
    let (mut runtime, dispatch_count) = setup(Mode::AsyncZeroCopy(false));
    runtime
      .execute_script(
        "filename.js",
        r#"
        Deno.core.opAsync("op_test");
        "#,
      )
      .unwrap();
    assert_eq!(dispatch_count.load(Ordering::Relaxed), 1);
  }

  #[test]
  fn test_dispatch_stack_zero_copy_bufs() {
    let (mut runtime, dispatch_count) = setup(Mode::AsyncZeroCopy(true));
    runtime
      .execute_script(
        "filename.js",
        r#"
        let zero_copy_a = new Uint8Array([0]);
        Deno.core.opAsync("op_test", null, zero_copy_a);
        "#,
      )
      .unwrap();
    assert_eq!(dispatch_count.load(Ordering::Relaxed), 1);
  }

  #[test]
  fn test_execute_script_return_value() {
    let mut runtime = JsRuntime::new(Default::default());
    let value_global = runtime.execute_script("a.js", "a = 1 + 2").unwrap();
    {
      let scope = &mut runtime.handle_scope();
      let value = value_global.open(scope);
      assert_eq!(value.integer_value(scope).unwrap(), 3);
    }
    let value_global = runtime.execute_script("b.js", "b = 'foobar'").unwrap();
    {
      let scope = &mut runtime.handle_scope();
      let value = value_global.open(scope);
      assert!(value.is_string());
      assert_eq!(
        value.to_string(scope).unwrap().to_rust_string_lossy(scope),
        "foobar"
      );
    }
  }

  #[test]
  fn terminate_execution() {
    let (mut isolate, _dispatch_count) = setup(Mode::Async);
    let v8_isolate_handle = isolate.v8_isolate().thread_safe_handle();

    let terminator_thread = std::thread::spawn(move || {
      // allow deno to boot and run
      std::thread::sleep(std::time::Duration::from_millis(100));

      // terminate execution
      let ok = v8_isolate_handle.terminate_execution();
      assert!(ok);
    });

    // Rn an infinite loop, which should be terminated.
    match isolate.execute_script("infinite_loop.js", "for(;;) {}") {
      Ok(_) => panic!("execution should be terminated"),
      Err(e) => {
        assert_eq!(e.to_string(), "Uncaught Error: execution terminated")
      }
    };

    // Cancel the execution-terminating exception in order to allow script
    // execution again.
    let ok = isolate.v8_isolate().cancel_terminate_execution();
    assert!(ok);

    // Verify that the isolate usable again.
    isolate
      .execute_script("simple.js", "1 + 1")
      .expect("execution should be possible again");

    terminator_thread.join().unwrap();
  }

  #[test]
  fn syntax_error() {
    let mut runtime = JsRuntime::new(Default::default());
    let src = "hocuspocus(";
    let r = runtime.execute_script("i.js", src);
    let e = r.unwrap_err();
    let js_error = e.downcast::<JsError>().unwrap();
    let frame = js_error.frames.first().unwrap();
    assert_eq!(frame.column_number, Some(12));
  }

  #[test]
  fn test_encode_decode() {
    run_in_task(|cx| {
      let (mut runtime, _dispatch_count) = setup(Mode::Async);
      runtime
        .execute_script(
          "encode_decode_test.js",
          include_str!("encode_decode_test.js"),
        )
        .unwrap();
      if let Poll::Ready(Err(_)) = runtime.poll_event_loop(cx) {
        unreachable!();
      }
    });
  }

  #[test]
  fn test_serialize_deserialize() {
    run_in_task(|cx| {
      let (mut runtime, _dispatch_count) = setup(Mode::Async);
      runtime
        .execute_script(
          "serialize_deserialize_test.js",
          include_str!("serialize_deserialize_test.js"),
        )
        .unwrap();
      if let Poll::Ready(Err(_)) = runtime.poll_event_loop(cx) {
        unreachable!();
      }
    });
  }

  #[test]
  fn test_error_builder() {
    #[op]
    fn op_err() -> Result<(), Error> {
      Err(custom_error("DOMExceptionOperationError", "abc"))
    }

    pub fn get_error_class_name(_: &Error) -> &'static str {
      "DOMExceptionOperationError"
    }

    run_in_task(|cx| {
      let ext = Extension::builder().ops(vec![op_err::decl()]).build();
      let mut runtime = JsRuntime::new(RuntimeOptions {
        extensions: vec![ext],
        get_error_class_fn: Some(&get_error_class_name),
        ..Default::default()
      });
      runtime
        .execute_script(
          "error_builder_test.js",
          include_str!("error_builder_test.js"),
        )
        .unwrap();
      if let Poll::Ready(Err(_)) = runtime.poll_event_loop(cx) {
        unreachable!();
      }
    });
  }

  #[test]
  fn test_error_without_stack() {
    let mut runtime = JsRuntime::new(RuntimeOptions::default());
    // SyntaxError
    let result = runtime.execute_script(
      "error_without_stack.js",
      r#"
function main() {
  console.log("asdf);
}

main();
"#,
    );
    let expected_error = r#"Uncaught SyntaxError: Invalid or unexpected token
    at error_without_stack.js:3:15"#;
    assert_eq!(result.unwrap_err().to_string(), expected_error);
  }

  #[test]
  fn test_error_stack() {
    let mut runtime = JsRuntime::new(RuntimeOptions::default());
    let result = runtime.execute_script(
      "error_stack.js",
      r#"
function assert(cond) {
  if (!cond) {
    throw Error("assert");
  }
}

function main() {
  assert(false);
}

main();
        "#,
    );
    let expected_error = r#"Error: assert
    at assert (error_stack.js:4:11)
    at main (error_stack.js:9:3)
    at error_stack.js:12:1"#;
    assert_eq!(result.unwrap_err().to_string(), expected_error);
  }

  #[test]
  fn test_error_async_stack() {
    run_in_task(|cx| {
      let mut runtime = JsRuntime::new(RuntimeOptions::default());
      runtime
        .execute_script(
          "error_async_stack.js",
          r#"
(async () => {
  const p = (async () => {
    await Promise.resolve().then(() => {
      throw new Error("async");
    });
  })();

  try {
    await p;
  } catch (error) {
    console.log(error.stack);
    throw error;
  }
})();"#,
        )
        .unwrap();
      let expected_error = r#"Error: async
    at error_async_stack.js:5:13
    at async error_async_stack.js:4:5
    at async error_async_stack.js:10:5"#;

      match runtime.poll_event_loop(cx) {
        Poll::Ready(Err(e)) => {
          assert_eq!(e.to_string(), expected_error);
        }
        _ => panic!(),
      };
    })
  }

  #[test]
  fn test_pump_message_loop() {
    run_in_task(|cx| {
      let mut runtime = JsRuntime::new(RuntimeOptions::default());
      runtime
        .execute_script(
          "pump_message_loop.js",
          r#"
function assertEquals(a, b) {
  if (a === b) return;
  throw a + " does not equal " + b;
}
const sab = new SharedArrayBuffer(16);
const i32a = new Int32Array(sab);
globalThis.resolved = false;

(function() {
  const result = Atomics.waitAsync(i32a, 0, 0);
  result.value.then(
    (value) => { assertEquals("ok", value); globalThis.resolved = true; },
    () => { assertUnreachable();
  });
})();

const notify_return_value = Atomics.notify(i32a, 0, 1);
assertEquals(1, notify_return_value);
"#,
        )
        .unwrap();

      match runtime.poll_event_loop(cx) {
        Poll::Ready(Ok(())) => {}
        _ => panic!(),
      };

      // noop script, will resolve promise from first script
      runtime
        .execute_script("pump_message_loop2.js", r#"assertEquals(1, 1);"#)
        .unwrap();

      // check that promise from `Atomics.waitAsync` has been resolved
      runtime
        .execute_script(
          "pump_message_loop3.js",
          r#"assertEquals(globalThis.resolved, true);"#,
        )
        .unwrap();
    })
  }

  #[test]
  fn test_core_js_stack_frame() {
    let mut runtime = JsRuntime::new(RuntimeOptions::default());
    // Call non-existent op so we get error from `core.js`
    let error = runtime
      .execute_script(
        "core_js_stack_frame.js",
        "Deno.core.opSync('non_existent');",
      )
      .unwrap_err();
    let error_string = error.to_string();
    // Test that the script specifier is a URL: `deno:<repo-relative path>`.
    assert!(error_string.contains("deno:core/01_core.js"));
  }

  #[test]
  fn test_is_proxy() {
    let mut runtime = JsRuntime::new(RuntimeOptions::default());
    let all_true: v8::Global<v8::Value> = runtime
      .execute_script(
        "is_proxy.js",
        r#"
      (function () {
        const o = { a: 1, b: 2};
        const p = new Proxy(o, {});
        return Deno.core.opSync("op_is_proxy", p) && !Deno.core.opSync("op_is_proxy", o) && !Deno.core.opSync("op_is_proxy", 42);
      })()
    "#,
      )
      .unwrap();
    let mut scope = runtime.handle_scope();
    let all_true = v8::Local::<v8::Value>::new(&mut scope, &all_true);
    assert!(all_true.is_true());
  }

  #[tokio::test]
  async fn test_async_opstate_borrow() {
    struct InnerState(u64);

    #[op]
    async fn op_async_borrow(
      op_state: Rc<RefCell<OpState>>,
    ) -> Result<(), Error> {
      let n = {
        let op_state = op_state.borrow();
        let inner_state = op_state.borrow::<InnerState>();
        inner_state.0
      };
      // Future must be Poll::Pending on first call
      tokio::time::sleep(std::time::Duration::from_millis(1)).await;
      if n != 42 {
        unreachable!();
      }
      Ok(())
    }

    let extension = Extension::builder()
      .ops(vec![op_async_borrow::decl()])
      .state(|state| {
        state.put(InnerState(42));
        Ok(())
      })
      .build();

    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![extension],
      ..Default::default()
    });

    runtime
      .execute_script(
        "op_async_borrow.js",
        "Deno.core.opAsync('op_async_borrow')",
      )
      .unwrap();
    runtime.run_event_loop(false).await.unwrap();
  }

  #[tokio::test]
  async fn test_set_macrotask_callback_set_next_tick_callback() {
    #[op]
    async fn op_async_sleep() -> Result<(), Error> {
      // Future must be Poll::Pending on first call
      tokio::time::sleep(std::time::Duration::from_millis(1)).await;
      Ok(())
    }

    let extension = Extension::builder()
      .ops(vec![op_async_sleep::decl()])
      .build();

    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![extension],
      ..Default::default()
    });

    runtime
      .execute_script(
        "macrotasks_and_nextticks.js",
        r#"
        (async function () {
          const results = [];
          Deno.core.opSync("op_set_macrotask_callback", () => {
            results.push("macrotask");
            return true;
          });
          Deno.core.opSync("op_set_next_tick_callback", () => {
            results.push("nextTick");
            Deno.core.opSync("op_set_has_tick_scheduled", false);
          });

          Deno.core.opSync("op_set_has_tick_scheduled", true);
          await Deno.core.opAsync('op_async_sleep');
          if (results[0] != "nextTick") {
            throw new Error(`expected nextTick, got: ${results[0]}`);
          }
          if (results[1] != "macrotask") {
            throw new Error(`expected macrotask, got: ${results[1]}`);
          }
        })();
        "#,
      )
      .unwrap();
    runtime.run_event_loop(false).await.unwrap();
  }

  #[tokio::test]
  async fn test_set_macrotask_callback_set_next_tick_callback_multiple() {
    let mut runtime = JsRuntime::new(Default::default());

    runtime
      .execute_script(
        "multiple_macrotasks_and_nextticks.js",
        r#"
        Deno.core.opSync("op_set_macrotask_callback", () => { return true; });
        Deno.core.opSync("op_set_macrotask_callback", () => { return true; });
        Deno.core.opSync("op_set_next_tick_callback", () => {});
        Deno.core.opSync("op_set_next_tick_callback", () => {});
        "#,
      )
      .unwrap();
    let isolate = runtime.v8_isolate();
    let state_rc = JsRuntime::state(isolate);
    let state = state_rc.borrow();
    assert_eq!(state.js_macrotask_cbs.len(), 2);
    assert_eq!(state.js_nexttick_cbs.len(), 2);
  }

  #[test]
  fn test_has_tick_scheduled() {
    use futures::task::ArcWake;

    static MACROTASK: AtomicUsize = AtomicUsize::new(0);
    static NEXT_TICK: AtomicUsize = AtomicUsize::new(0);

    #[op]
    fn op_macrotask() -> Result<(), AnyError> {
      MACROTASK.fetch_add(1, Ordering::Relaxed);
      Ok(())
    }

    #[op]
    fn op_next_tick() -> Result<(), AnyError> {
      NEXT_TICK.fetch_add(1, Ordering::Relaxed);
      Ok(())
    }

    let extension = Extension::builder()
      .ops(vec![op_macrotask::decl(), op_next_tick::decl()])
      .build();

    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![extension],
      ..Default::default()
    });

    runtime
      .execute_script(
        "has_tick_scheduled.js",
        r#"
          Deno.core.opSync("op_set_macrotask_callback", () => {
            Deno.core.opSync("op_macrotask");
            return true; // We're done.
          });
          Deno.core.opSync("op_set_next_tick_callback", () => Deno.core.opSync("op_next_tick"));
          Deno.core.opSync("op_set_has_tick_scheduled", true);
          "#,
      )
      .unwrap();

    struct ArcWakeImpl(Arc<AtomicUsize>);
    impl ArcWake for ArcWakeImpl {
      fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.0.fetch_add(1, Ordering::Relaxed);
      }
    }

    let awoken_times = Arc::new(AtomicUsize::new(0));
    let waker =
      futures::task::waker(Arc::new(ArcWakeImpl(awoken_times.clone())));
    let cx = &mut Context::from_waker(&waker);

    assert!(matches!(runtime.poll_event_loop(cx, false), Poll::Pending));
    assert_eq!(1, MACROTASK.load(Ordering::Relaxed));
    assert_eq!(1, NEXT_TICK.load(Ordering::Relaxed));
    assert_eq!(awoken_times.swap(0, Ordering::Relaxed), 1);
    assert!(matches!(runtime.poll_event_loop(cx, false), Poll::Pending));
    assert_eq!(awoken_times.swap(0, Ordering::Relaxed), 1);
    assert!(matches!(runtime.poll_event_loop(cx, false), Poll::Pending));
    assert_eq!(awoken_times.swap(0, Ordering::Relaxed), 1);
    assert!(matches!(runtime.poll_event_loop(cx, false), Poll::Pending));
    assert_eq!(awoken_times.swap(0, Ordering::Relaxed), 1);

    let state_rc = JsRuntime::state(runtime.v8_isolate());
    state_rc.borrow_mut().has_tick_scheduled = false;
    assert!(matches!(
      runtime.poll_event_loop(cx, false),
      Poll::Ready(Ok(()))
    ));
    assert_eq!(awoken_times.load(Ordering::Relaxed), 0);
    assert!(matches!(
      runtime.poll_event_loop(cx, false),
      Poll::Ready(Ok(()))
    ));
    assert_eq!(awoken_times.load(Ordering::Relaxed), 0);
  }

  #[tokio::test]
  async fn test_set_promise_reject_callback() {
    static PROMISE_REJECT: AtomicUsize = AtomicUsize::new(0);
    static UNCAUGHT_EXCEPTION: AtomicUsize = AtomicUsize::new(0);

    #[op]
    fn op_promise_reject() -> Result<(), AnyError> {
      PROMISE_REJECT.fetch_add(1, Ordering::Relaxed);
      Ok(())
    }

    #[op]
    fn op_uncaught_exception() -> Result<(), AnyError> {
      UNCAUGHT_EXCEPTION.fetch_add(1, Ordering::Relaxed);
      Ok(())
    }

    let extension = Extension::builder()
      .ops(vec![
        op_promise_reject::decl(),
        op_uncaught_exception::decl(),
      ])
      .build();

    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![extension],
      ..Default::default()
    });

    runtime
      .execute_script(
        "promise_reject_callback.js",
        r#"
        // Note: |promise| is not the promise created below, it's a child.
        Deno.core.opSync("op_set_promise_reject_callback", (type, promise, reason) => {
          if (type !== /* PromiseRejectWithNoHandler */ 0) {
            throw Error("unexpected type: " + type);
          }
          if (reason.message !== "reject") {
            throw Error("unexpected reason: " + reason);
          }
          Deno.core.opSync("op_promise_reject");
          throw Error("promiseReject"); // Triggers uncaughtException handler.
        });

        Deno.core.opSync("op_set_uncaught_exception_callback", (err) => {
          if (err.message !== "promiseReject") throw err;
          Deno.core.opSync("op_uncaught_exception");
        });

        new Promise((_, reject) => reject(Error("reject")));
        "#,
      )
      .unwrap();
    runtime.run_event_loop(false).await.unwrap();

    assert_eq!(1, PROMISE_REJECT.load(Ordering::Relaxed));
    assert_eq!(1, UNCAUGHT_EXCEPTION.load(Ordering::Relaxed));

    runtime
      .execute_script(
        "promise_reject_callback.js",
        r#"
        {
          const prev = Deno.core.opSync("op_set_promise_reject_callback", (...args) => {
            prev(...args);
          });
        }

        {
          const prev = Deno.core.opSync("op_set_uncaught_exception_callback", (...args) => {
            prev(...args);
            throw Error("fail");
          });
        }

        new Promise((_, reject) => reject(Error("reject")));
        "#,
      )
      .unwrap();
    // Exception from uncaughtException handler doesn't bubble up but is
    // printed to stderr.
    runtime.run_event_loop(false).await.unwrap();

    assert_eq!(2, PROMISE_REJECT.load(Ordering::Relaxed));
    assert_eq!(2, UNCAUGHT_EXCEPTION.load(Ordering::Relaxed));
  }

  #[test]
  fn test_op_return_serde_v8_error() {
    #[op]
    fn op_err() -> Result<std::collections::BTreeMap<u64, u64>, anyhow::Error> {
      Ok([(1, 2), (3, 4)].into_iter().collect()) // Maps can't have non-string keys in serde_v8
    }

    let ext = Extension::builder().ops(vec![op_err::decl()]).build();
    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![ext],
      ..Default::default()
    });
    assert!(runtime
      .execute_script(
        "test_op_return_serde_v8_error.js",
        "Deno.core.opSync('op_err')"
      )
      .is_err());
  }

  #[test]
  fn test_op_high_arity() {
    #[op]
    fn op_add_4(
      x1: i64,
      x2: i64,
      x3: i64,
      x4: i64,
    ) -> Result<i64, anyhow::Error> {
      Ok(x1 + x2 + x3 + x4)
    }

    let ext = Extension::builder().ops(vec![op_add_4::decl()]).build();
    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![ext],
      ..Default::default()
    });
    let r = runtime
      .execute_script("test.js", "Deno.core.opSync('op_add_4', 1, 2, 3, 4)")
      .unwrap();
    let scope = &mut runtime.handle_scope();
    assert_eq!(r.open(scope).integer_value(scope), Some(10));
  }

  #[test]
  fn test_op_disabled() {
    #[op]
    fn op_foo() -> Result<i64, anyhow::Error> {
      Ok(42)
    }

    let ext = Extension::builder()
      .ops(vec![op_foo::decl().disable()])
      .build();
    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![ext],
      ..Default::default()
    });
    let r = runtime
      .execute_script("test.js", "Deno.core.opSync('op_foo')")
      .unwrap();
    let scope = &mut runtime.handle_scope();
    assert!(r.open(scope).is_undefined());
  }

  #[test]
  fn test_op_detached_buffer() {
    use serde_v8::DetachedBuffer;

    #[op]
    fn op_sum_take(b: DetachedBuffer) -> Result<u64, anyhow::Error> {
      Ok(b.as_ref().iter().clone().map(|x| *x as u64).sum())
    }

    #[op]
    fn op_boomerang(
      b: DetachedBuffer,
    ) -> Result<DetachedBuffer, anyhow::Error> {
      Ok(b)
    }

    let ext = Extension::builder()
      .ops(vec![op_sum_take::decl(), op_boomerang::decl()])
      .build();

    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![ext],
      ..Default::default()
    });

    runtime
      .execute_script(
        "test.js",
        r#"
        const a1 = new Uint8Array([1,2,3]);
        const a1b = a1.subarray(0, 3);
        const a2 = new Uint8Array([5,10,15]);
        const a2b = a2.subarray(0, 3);


        if (!(a1.length > 0 && a1b.length > 0)) {
          throw new Error("a1 & a1b should have a length");
        }
        let sum = Deno.core.opSync('op_sum_take', a1b);
        if (sum !== 6) {
          throw new Error(`Bad sum: ${sum}`);
        }
        if (a1.length > 0 || a1b.length > 0) {
          throw new Error("expecting a1 & a1b to be detached");
        }

        const a3 = Deno.core.opSync('op_boomerang', a2b);
        if (a3.byteLength != 3) {
          throw new Error(`Expected a3.byteLength === 3, got ${a3.byteLength}`);
        }
        if (a3[0] !== 5 || a3[1] !== 10) {
          throw new Error(`Invalid a3: ${a3[0]}, ${a3[1]}`);
        }
        if (a2.byteLength > 0 || a2b.byteLength > 0) {
          throw new Error("expecting a2 & a2b to be detached, a3 re-attached");
        }

        const wmem = new WebAssembly.Memory({ initial: 1, maximum: 2 });
        const w32 = new Uint32Array(wmem.buffer);
        w32[0] = 1; w32[1] = 2; w32[2] = 3;
        const assertWasmThrow = (() => {
          try {
            let sum = Deno.core.opSync('op_sum_take', w32.subarray(0, 2));
            return false;
          } catch(e) {
            return e.message.includes('ExpectedDetachable');
          }
        });
        if (!assertWasmThrow()) {
          throw new Error("expected wasm mem to not be detachable");
        }
      "#,
      )
      .unwrap();
  }

  #[test]
  fn test_op_unstable_disabling() {
    #[op]
    fn op_foo() -> Result<i64, anyhow::Error> {
      Ok(42)
    }

    #[op(unstable)]
    fn op_bar() -> Result<i64, anyhow::Error> {
      Ok(42)
    }

    let ext = Extension::builder()
      .ops(vec![op_foo::decl(), op_bar::decl()])
      .middleware(|op| if op.is_unstable { op.disable() } else { op })
      .build();
    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![ext],
      ..Default::default()
    });
    runtime
      .execute_script(
        "test.js",
        r#"
        if (Deno.core.opSync('op_foo') !== 42) {
          throw new Error("Exptected op_foo() === 42");
        }
        if (Deno.core.opSync('op_bar') !== undefined) {
          throw new Error("Expected op_bar to be disabled")
        }
      "#,
      )
      .unwrap();
  }
}
