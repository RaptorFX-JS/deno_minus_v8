// Copyright 2018-2022 the Deno authors. All rights reserved. MIT license.

use crate::{serde_v8, v8};
use crate::bindings;
use crate::error::generic_error;
use crate::error::JsError;
use crate::extensions::OpDecl;
use crate::extensions::OpEventLoopFn;
use crate::module_specifier::ModuleSpecifier;
use crate::modules::ModuleId;
use crate::modules::ModuleMap;
use crate::op_void_async;
use crate::op_void_sync;
use crate::ops::*;
use crate::Extension;
use crate::OpMiddlewareFn;
use crate::OpResult;
use crate::OpState;
use crate::PromiseId;
use anyhow::Error;
use futures::channel::oneshot;
use futures::future::poll_fn;
use futures::future::Future;
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
use v8::backend::JsBackend;
use v8::Local;
use v8::Handle;

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
  // This is an Option<OwnedIsolate> instead of just OwnedIsolate to workaround
  // a safety issue with SnapshotCreator. See JsRuntime::drop.
  v8_isolate: Option<v8::OwnedIsolate>,
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

pub struct RuntimeOptions {
  // The whole point of minus_v8.
  pub backend: Box<dyn JsBackend>,

  /// Allows to map error type to a string "class" used to represent
  /// error in JavaScript.
  pub get_error_class_fn: Option<GetErrorClassFn>,

  /// JsRuntime extensions, not to be confused with ES modules
  /// these are sets of ops and other JS code to be initialized.
  pub extensions: Vec<Extension>,
}

#[cfg(test)]
impl Default for RuntimeOptions {
  fn default() -> Self {
    Self {
      backend: Box::new(crate::minus_v8::backend::test::TotallyLegitJSBackend),
      get_error_class_fn: None,
      extensions: vec![],
    }
  }
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
      let isolate = v8::Isolate::new(options.backend);
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

    let module_map = ModuleMap {
      next_module_id: 0,
      by_id: HashMap::new(),
    };
    isolate.set_slot(Rc::new(RefCell::new(module_map)));

    let mut js_runtime = Self {
      v8_isolate: Some(isolate),
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

  /// minus-v8 specific function. Temporarily gets mutable access to
  /// the [`JsBackend`] as its original type.
  ///
  /// # Panics
  /// If the given backend type does not match the real backend type.
  /// This will leave the runtime in an inconsistent state.
  pub fn with_backend<B, F, R>(&mut self, callback: F) -> R
  where
    B: JsBackend + 'static,
    F: FnOnce(&mut B) -> R,
  {
    let mut owned_isolate = self.v8_isolate.take().unwrap();
    let backend_box: Box<B> = match owned_isolate.backend.downcast() {
      Ok(backend_box) => backend_box,
      Err(_) => panic!("Couldn't downcast Box<dyn JsBackend> to Box<{}>", std::any::type_name::<B>()),
    };
    let mut backend = *backend_box;
    let ret = callback(&mut backend);
    owned_isolate.backend = Box::new(backend);
    self.v8_isolate = Some(owned_isolate);
    ret
  }

  fn setup_isolate(mut isolate: v8::OwnedIsolate) -> v8::OwnedIsolate {
    // TODO(minus_v8) promise reject callback
    // isolate.set_promise_reject_callback(bindings::promise_reject_callback);
    isolate
  }

  pub(crate) fn state(isolate: &v8::Isolate) -> Rc<RefCell<JsRuntimeState>> {
    let s = isolate.get_slot::<Rc<RefCell<JsRuntimeState>>>().unwrap();
    s.clone()
  }

  pub(crate) fn module_map(isolate: &v8::Isolate) -> Rc<RefCell<ModuleMap>> {
    let module_map = isolate.get_slot::<Rc<RefCell<ModuleMap>>>().unwrap();
    module_map.clone()
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

  /// Grabs a reference to core.js' opresolve & syncOpsCache()
  fn init_cbs(&mut self) {
    let scope = &mut self.handle_scope();
    let recv_cb = {
      let cb = (scope.backend.grab_function())(scope, "Deno.core.opresolve").unwrap();
      unsafe { Local::from_raw(cb).unwrap() }
    };
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
      // TODO(minus_v8) promise rejection callback
      // self.check_promise_exceptions()?;
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
  // let state_rc = JsRuntime::state(scope);

  let is_terminating_exception = scope.is_execution_terminating();
  let mut exception = exception;

  // TODO(minus_v8) promise rejection callback
  /*
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
  */

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
  // TODO(bartlomieju): make it return `ModuleEvaluationFuture`?
  /// Evaluates an already instantiated ES module.
  ///
  /// Returns a receiver handle that resolves when module promise resolves.
  /// Implementors must manually call [`JsRuntime::run_event_loop`] to drive
  /// module evaluation future.
  ///
  /// `Error` can usually be downcast to `JsError` and should be awaited and
  /// checked after [`JsRuntime::run_event_loop`] completion.
  ///
  /// This function panics if module has not been instantiated.
  pub fn mod_evaluate(
    &mut self,
    id: ModuleId,
  ) -> oneshot::Receiver<Result<(), Error>> {
    let state_rc = Self::state(self.v8_isolate());
    let module_map_rc = Self::module_map(self.v8_isolate());

    let module_map = module_map_rc.borrow();
    let module = module_map
      .by_id
      .get(&id)
      .expect("ModuleInfo not found");

    let (sender, receiver) = oneshot::channel();

    // IMPORTANT: Top-level-await is enabled, which means that return value
    // of module evaluation is a promise.
    //
    // Because that promise is created internally by V8, when error occurs during
    // module evaluation the promise is rejected, and since the promise has no rejection
    // handler it will result in call to `bindings::promise_reject_callback` adding
    // the promise to pending promise rejection table - meaning JsRuntime will return
    // error on next poll().
    //
    // This situation is not desirable as we want to manually return error at the
    // end of this function to handle it further. It means we need to manually
    // remove this promise from pending promise rejection table.
    //
    // For more details see:
    // https://github.com/denoland/deno/issues/4908
    // https://v8.dev/features/top-level-await#module-execution-order
    // TODO(minus_v8) above details are blocked on implementing the promise reject callback
    let specifier_escaped = serde_json::to_string(&module.name).unwrap();
    self
      .execute_script(
        &module.name,
        &format!("(async () => {{ await import({}) }})()", specifier_escaped)
      )
      .expect("minus_v8: failed to import module");

    let explicit_terminate_exception =
      state_rc.borrow_mut().explicit_terminate_exception.take();
    let scope = &mut self.handle_scope();
    let tc_scope = &mut v8::TryCatch::new(scope);

    if let Some(exception) = explicit_terminate_exception {
      let exception = v8::Local::new(tc_scope, exception);
      sender
        .send(exception_to_err_result(tc_scope, exception, false))
        .expect("Failed to send module evaluation error.");
    } else if tc_scope.has_terminated() || tc_scope.is_execution_terminating() {
      sender.send(Err(
        generic_error("Cannot evaluate module, because JavaScript execution has been terminated.")
      )).expect("Failed to send module evaluation error.");
    }

    receiver
  }

  /// Asynchronously load specified module and all of its dependencies.
  ///
  /// The module will be marked as "main", and because of that
  /// "import.meta.main" will return true when checked inside that module.
  ///
  /// User must call [`JsRuntime::mod_evaluate`] with returned `ModuleId`
  /// manually after load is finished.
  pub async fn load_main_module(
    &mut self,
    specifier: &ModuleSpecifier,
    code: Option<String>,
  ) -> Result<ModuleId, Error> {
    let module_map_rc = Self::module_map(self.v8_isolate());
    if let Some(_) = code {
      todo!("minus_v8: support loading modules with code");
    }
    ModuleMap::load_main(module_map_rc, specifier.as_str()).await
  }

  /// Asynchronously load specified ES module and all of its dependencies.
  ///
  /// This method is meant to be used when loading some utility code that
  /// might be later imported by the main module (ie. an entry point module).
  ///
  /// User must call [`JsRuntime::mod_evaluate`] with returned `ModuleId`
  /// manually after load is finished.
  pub async fn load_side_module(
    &mut self,
    specifier: &ModuleSpecifier,
    code: Option<String>,
  ) -> Result<ModuleId, Error> {
    let module_map_rc = Self::module_map(self.v8_isolate());
    if let Some(_) = code {
      todo!("minus_v8: support loading modules with code");
    }
    ModuleMap::load_side(module_map_rc, specifier.as_str()).await
  }

  // TODO(minus_v8) promise rejection callback
  /*
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
  */

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
    let mut args: Vec<Box<dyn serde_v8::ErasedSerialize>> = vec![];

    // Now handle actual ops.
    {
      let mut state = state_rc.borrow_mut();
      state.have_unpolled_ops = false;

      while let Poll::Ready(Some(item)) = state.pending_ops.poll_next_unpin(cx)
      {
        let (promise_id, op_id, resp) = item;
        state.unrefed_ops.remove(&promise_id);
        state.op_state.borrow().tracker.track_async_completed(op_id);
        args.push(Box::new(promise_id as i32));
        args.push(resp.to_v8(scope).unwrap());
      }
    }

    if args.is_empty() {
      return Ok(());
    }

    let tc_scope = &mut v8::TryCatch::new(scope);
    let js_recv_cb = js_recv_cb_handle.open(tc_scope);
    let this = v8::undefined(tc_scope).into();
    js_recv_cb.call_with_serialized(tc_scope, this, args);

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
      let this = v8::undefined(tc_scope);
      loop {
        let is_done = js_macrotask_cb.call(tc_scope, this.clone(), &[]);

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

    let tc_scope = &mut v8::TryCatch::new(scope);

    match (tc_scope.backend.execute_script())(tc_scope, name, source_code) {
      Some(value) => {
        let value_handle = unsafe {
          v8::Global::from_raw(scope, value).unwrap()
        };
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

    assert!(matches!(runtime.poll_event_loop(cx), Poll::Pending));
    assert_eq!(1, MACROTASK.load(Ordering::Relaxed));
    assert_eq!(1, NEXT_TICK.load(Ordering::Relaxed));
    assert_eq!(awoken_times.swap(0, Ordering::Relaxed), 1);
    assert!(matches!(runtime.poll_event_loop(cx), Poll::Pending));
    assert_eq!(awoken_times.swap(0, Ordering::Relaxed), 1);
    assert!(matches!(runtime.poll_event_loop(cx), Poll::Pending));
    assert_eq!(awoken_times.swap(0, Ordering::Relaxed), 1);
    assert!(matches!(runtime.poll_event_loop(cx), Poll::Pending));
    assert_eq!(awoken_times.swap(0, Ordering::Relaxed), 1);

    let state_rc = JsRuntime::state(runtime.v8_isolate());
    state_rc.borrow_mut().has_tick_scheduled = false;
    assert!(matches!(
      runtime.poll_event_loop(cx),
      Poll::Ready(Ok(()))
    ));
    assert_eq!(awoken_times.load(Ordering::Relaxed), 0);
    assert!(matches!(
      runtime.poll_event_loop(cx),
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
