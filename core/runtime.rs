// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

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
use crate::ExtensionFileSource;
use crate::OpMiddlewareFn;
use crate::OpResult;
use crate::OpState;
use crate::PromiseId;
use anyhow::Context as AnyhowContext;
use anyhow::Error;
use futures::channel::oneshot;
use futures::future::poll_fn;
use futures::future::Future;
use futures::stream::FuturesUnordered;
use futures::stream::StreamExt;
use futures::task::AtomicWaker;
use v8::backend::JsBackend;
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::option::Option;
use std::rc::Rc;
use std::task::Context;
use std::task::Poll;
use v8::{Handle, Local, OwnedIsolate, Value};

type PendingOpFuture = OpCall<(RealmIdx, PromiseId, OpId, OpResult)>;

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
  state: Rc<RefCell<JsRuntimeState>>,
  module_map: Option<Rc<RefCell<ModuleMap>>>,
  // This is an Option<OwnedIsolate> instead of just OwnedIsolate to workaround
  // a safety issue with SnapshotCreator. See JsRuntime::drop.
  v8_isolate: Option<v8::OwnedIsolate>,
  extensions: Vec<Extension>,
  extensions_with_js: Vec<Extension>,
  event_loop_middlewares: Vec<Box<OpEventLoopFn>>,
}

#[derive(Default)]
pub(crate) struct ContextState {
  js_recv_cb: Option<v8::Global<v8::Function>>,
  pub(crate) js_build_custom_error_cb: Option<v8::Global<v8::Function>>,
  pub(crate) js_promise_reject_cb: Option<v8::Global<v8::Function>>,
  pub(crate) js_format_exception_cb: Option<v8::Global<v8::Function>>,
  pub(crate) pending_promise_rejections:
    HashMap<v8::Global<v8::Promise>, v8::Global<v8::Value>>,
  pub(crate) unrefed_ops: HashSet<i32>,
  // We don't explicitly re-read this prop but need the slice to live alongside
  // the context
  pub(crate) op_ctxs: Box<[OpCtx]>,
}

/// Internal state for JsRuntime which is stored in one of v8::Isolate's
/// embedder slots.
pub struct JsRuntimeState {
  global_realm: Option<JsRealm>,
  known_realms: Vec<v8::Weak<v8::Context>>,
  pub(crate) js_macrotask_cbs: Vec<v8::Global<v8::Function>>,
  pub(crate) js_nexttick_cbs: Vec<v8::Global<v8::Function>>,
  pub(crate) has_tick_scheduled: bool,
  pub(crate) pending_ops: FuturesUnordered<PendingOpFuture>,
  pub(crate) have_unpolled_ops: bool,
  pub(crate) op_state: Rc<RefCell<OpState>>,
  /// The error that was passed to an `op_dispatch_exception` call.
  /// It will be retrieved by `exception_to_err_result` and used as an error
  /// instead of any other exceptions.
  // TODO(nayeemrmn): This is polled in `exception_to_err_result()` which is
  // flimsy. Try to poll it similarly to `pending_promise_rejections`.
  pub(crate) dispatched_exceptions: VecDeque<v8::Global<v8::Value>>,
  waker: AtomicWaker,
}

pub const V8_WRAPPER_TYPE_INDEX: i32 = 0;
pub const V8_WRAPPER_OBJECT_INDEX: i32 = 1;

pub struct RuntimeOptions {
  // The whole point of minus_v8.
  pub backend: Box<dyn JsBackend>,

  /// Allows to map error type to a string "class" used to represent
  /// error in JavaScript.
  pub get_error_class_fn: Option<GetErrorClassFn>,

  /// JsRuntime extensions, not to be confused with ES modules.
  /// Only ops registered by extensions will be initialized. If you need
  /// to execute JS code from extensions, use `extensions_with_js` options
  /// instead.
  pub extensions: Vec<Extension>,

  /// JsRuntime extensions, not to be confused with ES modules.
  /// Ops registered by extensions will be initialized and JS code will be
  /// executed. If you don't need to execute JS code from extensions, use
  /// `extensions` option instead.
  ///
  /// This is useful when creating snapshots, in such case you would pass
  /// extensions using `extensions_with_js`, later when creating a runtime
  /// from the snapshot, you would pass these extensions using `extensions`
  /// option.
  pub extensions_with_js: Vec<Extension>,
}

#[cfg(test)]
impl Default for RuntimeOptions {
  fn default() -> Self {
    Self {
      backend: Box::new(v8::backend::test::TotallyLegitJSBackend),
      get_error_class_fn: None,
      extensions: vec![],
      extensions_with_js: vec![],
    }
  }
}

impl Drop for JsRuntime {
  fn drop(&mut self) {
    if let Some(v8_isolate) = self.v8_isolate.as_mut() {
      Self::drop_state_and_module_map(v8_isolate);
    }
  }
}

impl JsRuntime {
  const STATE_DATA_OFFSET: u32 = 0;
  const MODULE_MAP_DATA_OFFSET: u32 = 1;

  /// Only constructor, configuration is done through `options`.
  pub fn new(mut options: RuntimeOptions) -> Self {
    // Add builtins extension
    options
      .extensions_with_js
      .insert(0, crate::ops_builtin::init_builtins());

    let ops = Self::collect_ops(
      &mut options.extensions,
      &mut options.extensions_with_js,
    );
    let mut op_state = OpState::new(ops.len());

    if let Some(get_error_class_fn) = options.get_error_class_fn {
      op_state.get_error_class_fn = get_error_class_fn;
    }
    let op_state = Rc::new(RefCell::new(op_state));

    let align = std::mem::align_of::<usize>();
    let layout = std::alloc::Layout::from_size_align(
      std::mem::size_of::<*mut v8::OwnedIsolate>(),
      align,
    )
    .unwrap();
    assert!(layout.size() > 0);
    let isolate_ptr: *mut v8::OwnedIsolate =
      // SAFETY: we just asserted that layout has non-0 size.
      unsafe { std::alloc::alloc(layout) as *mut _ };

    let state_rc = Rc::new(RefCell::new(JsRuntimeState {
      js_macrotask_cbs: vec![],
      js_nexttick_cbs: vec![],
      has_tick_scheduled: false,
      pending_ops: FuturesUnordered::new(),
      op_state: op_state.clone(),
      waker: AtomicWaker::new(),
      have_unpolled_ops: false,
      dispatched_exceptions: Default::default(),
      // Some fields are initialized later after isolate is created
      global_realm: None,
      known_realms: Vec::with_capacity(1),
    }));

    let weak = Rc::downgrade(&state_rc);
    let op_ctxs = ops
      .into_iter()
      .enumerate()
      .map(|(id, decl)| OpCtx {
        id,
        state: op_state.clone(),
        runtime_state: weak.clone(),
        decl: Rc::new(decl),
        realm_idx: 0,
      })
      .collect::<Vec<_>>()
      .into_boxed_slice();

    let global_context;
    let mut isolate = {
      let isolate = v8::Isolate::new(options.backend);
      let mut isolate = JsRuntime::setup_isolate(isolate);
      {
        // SAFETY: this is first use of `isolate_ptr` so we are sure we're
        // not overwriting an existing pointer.
        isolate = unsafe {
          isolate_ptr.write(isolate);
          isolate_ptr.read()
        };
        let scope = &mut v8::HandleScope::new(&mut isolate);
        let context = bindings::initialize_context(scope, &op_ctxs);

        global_context = v8::Global::new(scope, context);
      }

      isolate
    };

    global_context.open(&mut isolate).set_slot(
      &mut isolate,
      Rc::new(RefCell::new(ContextState {
        op_ctxs,
        ..Default::default()
      })),
    );

    op_state.borrow_mut().put(isolate_ptr);

    {
      let mut state = state_rc.borrow_mut();
      state.global_realm = Some(JsRealm(global_context.clone()));
      state
        .known_realms
        .push(v8::Weak::new(&mut isolate, global_context));
    }
    isolate.set_data(
      Self::STATE_DATA_OFFSET,
      Rc::into_raw(state_rc.clone()) as *mut c_void,
    );

    let module_map_rc = Rc::new(RefCell::new(ModuleMap::default()));
    isolate.set_data(
      Self::MODULE_MAP_DATA_OFFSET,
      Rc::into_raw(module_map_rc.clone()) as *mut c_void,
    );

    let mut js_runtime = Self {
      v8_isolate: Some(isolate),
      event_loop_middlewares: Vec::with_capacity(options.extensions.len()),
      extensions: options.extensions,
      extensions_with_js: options.extensions_with_js,
      state: state_rc,
      module_map: Some(module_map_rc),
    };

    // Init resources and ops before extensions to make sure they are
    // available during the initialization process.
    js_runtime.init_extension_ops().unwrap();
    let realm = js_runtime.global_realm();
    js_runtime.init_extension_js(&realm).unwrap();
    // Init callbacks (opresolve)
    let global_realm = js_runtime.global_realm();
    js_runtime.init_cbs(&global_realm);

    js_runtime
  }

  fn drop_state_and_module_map(v8_isolate: &mut OwnedIsolate) {
    let state_ptr = v8_isolate.get_data(Self::STATE_DATA_OFFSET);
    let state_rc =
    // SAFETY: We are sure that it's a valid pointer for whole lifetime of
    // the runtime.
    unsafe { Rc::from_raw(state_ptr as *const RefCell<JsRuntimeState>) };
    drop(state_rc);

    let module_map_ptr = v8_isolate.get_data(Self::MODULE_MAP_DATA_OFFSET);
    let module_map_rc =
    // SAFETY: We are sure that it's a valid pointer for whole lifetime of
    // the runtime.
    unsafe { Rc::from_raw(module_map_ptr as *const RefCell<ModuleMap>) };
    drop(module_map_rc);
  }

  #[inline]
  fn get_module_map(&mut self) -> &Rc<RefCell<ModuleMap>> {
    self.module_map.as_ref().unwrap()
  }

  #[inline]
  pub fn global_context(&mut self) -> v8::Global<v8::Context> {
    self.global_realm().0
  }

  #[inline]
  pub fn v8_isolate(&mut self) -> &mut v8::OwnedIsolate {
    self.v8_isolate.as_mut().unwrap()
  }

  #[inline]
  pub fn global_realm(&mut self) -> JsRealm {
    let state = self.state.borrow();
    state.global_realm.clone().unwrap()
  }

  /// Creates a new realm (V8 context) in this JS execution context,
  /// pre-initialized with all of the extensions that were passed in
  /// [`RuntimeOptions::extensions_with_js`] when the [`JsRuntime`] was
  /// constructed.
  pub fn create_realm(&mut self) -> Result<JsRealm, Error> {
    let realm = {
      let realm_idx = self.state.borrow().known_realms.len();

      let op_ctxs: Box<[OpCtx]> = self
        .global_realm()
        .state(self.v8_isolate())
        .borrow()
        .op_ctxs
        .iter()
        .map(|op_ctx| OpCtx {
          id: op_ctx.id,
          state: op_ctx.state.clone(),
          decl: op_ctx.decl.clone(),
          runtime_state: op_ctx.runtime_state.clone(),
          realm_idx,
        })
        .collect();

      // SAFETY: Having the scope tied to self's lifetime makes it impossible to
      // reference JsRuntimeState::op_ctxs while the scope is alive. Here we
      // turn it into an unbound lifetime, which is sound because 1. it only
      // lives until the end of this block, and 2. the HandleScope only has
      // access to the isolate, and nothing else we're accessing from self does.
      let scope = &mut v8::HandleScope::new(unsafe {
        &mut *(self.v8_isolate() as *mut v8::OwnedIsolate)
      });
      let context =
        bindings::initialize_context(scope, &op_ctxs);
      context.set_slot(
        scope,
        Rc::new(RefCell::new(ContextState {
          op_ctxs,
          ..Default::default()
        })),
      );

      self
        .state
        .borrow_mut()
        .known_realms
        .push(v8::Weak::new(scope, context.clone()));

      JsRealm::new(v8::Global::new(scope, context))
    };

    self.init_extension_js(&realm)?;
    self.init_cbs(&realm);
    Ok(realm)
  }

  #[inline]
  pub fn handle_scope(&mut self) -> v8::HandleScope {
    self.global_realm().handle_scope(self.v8_isolate())
  }

  fn setup_isolate(mut isolate: v8::OwnedIsolate) -> v8::OwnedIsolate {
    isolate
  }

  pub(crate) fn state(isolate: &v8::Isolate) -> Rc<RefCell<JsRuntimeState>> {
    let state_ptr = isolate.get_data(Self::STATE_DATA_OFFSET);
    let state_rc =
      // SAFETY: We are sure that it's a valid pointer for whole lifetime of
      // the runtime.
      unsafe { Rc::from_raw(state_ptr as *const RefCell<JsRuntimeState>) };
    let state = state_rc.clone();
    Rc::into_raw(state_rc);
    state
  }

  pub(crate) fn module_map(isolate: &v8::Isolate) -> Rc<RefCell<ModuleMap>> {
    let module_map_ptr = isolate.get_data(Self::MODULE_MAP_DATA_OFFSET);
    let module_map_rc =
      // SAFETY: We are sure that it's a valid pointer for whole lifetime of
      // the runtime.
      unsafe { Rc::from_raw(module_map_ptr as *const RefCell<ModuleMap>) };
    let module_map = module_map_rc.clone();
    Rc::into_raw(module_map_rc);
    module_map
  }

  /// Initializes JS of provided Extensions in the given realm
  fn init_extension_js(&mut self, realm: &JsRealm) -> Result<(), Error> {
    fn load_and_evaluate_module(
      runtime: &mut JsRuntime,
      file_source: &ExtensionFileSource,
    ) -> Result<(), Error> {
      futures::executor::block_on(async {
        let id = runtime
          .load_side_module(
            &ModuleSpecifier::parse(&file_source.specifier)?,
            Some(file_source.code.to_string()),
          )
          .await?;
        let receiver = runtime.mod_evaluate(id).await;
        poll_fn(|cx| {
          let r = runtime.poll_event_loop(cx, false);
          // TODO(bartlomieju): some code in readable-stream polyfill in `ext/node`
          // is calling `nextTick()` during snapshotting, which causes infinite loop
          runtime.state.borrow_mut().has_tick_scheduled = false;
          r
        })
        .await?;
        receiver.await?
      })
      .with_context(|| format!("Couldn't execute '{}'", file_source.specifier))
    }

    // Take extensions to avoid double-borrow
    let extensions = std::mem::take(&mut self.extensions_with_js);
    for ext in &extensions {
      {
        let esm_files = ext.get_esm_sources();
        if let Some(entry_point) = ext.get_esm_entry_point() {
          let file_source = esm_files
            .iter()
            .find(|file| file.specifier == entry_point)
            .unwrap();
          load_and_evaluate_module(self, file_source)?;
        } else {
          for file_source in esm_files {
            load_and_evaluate_module(self, file_source)?;
          }
        }
      }

      {
        let js_files = ext.get_js_sources();
        for file_source in js_files {
          // TODO(@AaronO): use JsRuntime::execute_static() here to move src off heap
          realm.execute_script(
            self.v8_isolate(),
            &file_source.specifier,
            file_source.code,
          )?;
        }
      }
    }
    // Restore extensions
    self.extensions_with_js = extensions;

    Ok(())
  }

  /// Collects ops from extensions & applies middleware
  fn collect_ops(
    extensions: &mut [Extension],
    extensions_with_js: &mut [Extension],
  ) -> Vec<OpDecl> {
    let mut exts = vec![];
    exts.extend(extensions);
    exts.extend(extensions_with_js);

    for (ext, previous_exts) in
      exts.iter().enumerate().map(|(i, ext)| (ext, &exts[..i]))
    {
      ext.check_dependencies(previous_exts);
    }

    // Middleware
    let middleware: Vec<Box<OpMiddlewareFn>> = exts
      .iter_mut()
      .filter_map(|e| e.init_middleware())
      .collect();

    // macroware wraps an opfn in all the middleware
    let macroware = move |d| middleware.iter().fold(d, |d, m| m(d));

    // Flatten ops, apply middlware & override disabled ops
    exts
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
    {
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
    }
    {
      let mut extensions: Vec<Extension> =
        std::mem::take(&mut self.extensions_with_js);

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
      self.extensions_with_js = extensions;
    }
    Ok(())
  }

  /// Grabs a reference to core.js' opresolve & syncOpsCache()
  fn init_cbs(&mut self, realm: &JsRealm) {
    let (recv_cb, build_custom_error_cb) = {
      let scope = &mut realm.handle_scope(self.v8_isolate());
      let recv_cb = {
        let cb = futures::executor::block_on((scope.backend.grab_function())(scope, "Deno.core.opresolve")).unwrap();
        unsafe { v8::Local::from_raw(cb).unwrap() }
      };
      let build_custom_error_cb = {
        let cb = futures::executor::block_on((scope.backend.grab_function())(scope, "Deno.core.buildCustomError"))
          .expect("Deno.core.buildCustomError is undefined in the realm");
        unsafe { v8::Local::from_raw(cb).unwrap() }
      };
      (
        v8::Global::new(scope, recv_cb),
        v8::Global::new(scope, build_custom_error_cb),
      )
    };

    // Put global handles in the realm's ContextState
    let state_rc = realm.state(self.v8_isolate());
    let mut state = state_rc.borrow_mut();
    state.js_recv_cb.replace(recv_cb);
    state
      .js_build_custom_error_cb
      .replace(build_custom_error_cb);
  }

  /// Returns the runtime's op state, which can be used to maintain ops
  /// and access resources between op calls.
  pub fn op_state(&mut self) -> Rc<RefCell<OpState>> {
    let state = self.state.borrow();
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
    self
      .global_realm()
      .execute_script(self.v8_isolate(), name, source_code)
  }

  /// Runs event loop to completion
  ///
  /// This future resolves when:
  ///  - there are no more pending dynamic imports
  ///  - there are no more pending ops
  ///  - there are no more active inspector sessions (only if `wait_for_inspector` is set to true)
  pub async fn run_event_loop(
    &mut self,
    wait_for_inspector: bool,
  ) -> Result<(), Error> {
    poll_fn(|cx| self.poll_event_loop(cx, wait_for_inspector)).await
  }

  /// Runs a single tick of event loop
  ///
  /// If `wait_for_inspector` is set to true event loop
  /// will return `Poll::Pending` if there are active inspector sessions.
  pub fn poll_event_loop(
    &mut self,
    cx: &mut Context,
    #[allow(unused)]
    wait_for_inspector: bool,
  ) -> Poll<Result<(), Error>> {
    {
      let state = self.state.borrow();
      state.waker.register(cx.waker());
    }

    // Ops
    self.resolve_async_ops(cx)?;

    // Run all next tick callbacks and macrotasks callbacks and only then
    // check for any promise exceptions (`unhandledrejection` handlers are
    // run in macrotasks callbacks so we need to let them run first).
    self.drain_nexttick()?;
    self.drain_macrotasks()?;
    // TODO(minus_v8) promise rejection callback
    // self.check_promise_rejections()?;

    // Event loop middlewares
    let mut maybe_scheduling = false;
    {
      let op_state = self.state.borrow().op_state.clone();
      for f in &self.event_loop_middlewares {
        if f(op_state.clone(), cx) {
          maybe_scheduling = true;
        }
      }
    }

    // Top level module
    // self.evaluate_pending_module();

    let pending_state = self.event_loop_pending_state();
    if !pending_state.is_pending() && !maybe_scheduling {
      return Poll::Ready(Ok(()));
    }

    let state = self.state.borrow();

    // Check if more async ops have been dispatched
    // during this turn of event loop.
    // If there are any pending background tasks, we also wake the runtime to
    // make sure we don't miss them.
    // TODO(andreubotella) The event loop will spin as long as there are pending
    // background tasks. We should look into having V8 notify us when a
    // background task is done.
    if state.have_unpolled_ops
      || pending_state.has_tick_scheduled
      || maybe_scheduling
    {
      state.waker.wake();
    }

    drop(state);

    /*
    if pending_state.has_pending_module_evaluation {
      if pending_state.has_pending_refed_ops
        || pending_state.has_pending_dyn_imports
        || pending_state.has_pending_dyn_module_evaluation
        || pending_state.has_pending_background_tasks
        || pending_state.has_tick_scheduled
        || maybe_scheduling
      {
        // pass, will be polled again
      } else {
        let scope = &mut self.handle_scope();
        let messages = find_stalled_top_level_await(scope);
        // We are gonna print only a single message to provide a nice formatting
        // with source line of offending promise shown. Once user fixed it, then
        // they will get another error message for the next promise (but this
        // situation is gonna be very rare, if ever happening).
        assert!(!messages.is_empty());
        let msg = v8::Local::new(scope, messages[0].clone());
        let js_error = JsError::from_v8_message(scope, msg);
        return Poll::Ready(Err(js_error.into()));
      }
    }

    if pending_state.has_pending_dyn_module_evaluation {
      if pending_state.has_pending_refed_ops
        || pending_state.has_pending_dyn_imports
        || pending_state.has_pending_background_tasks
        || pending_state.has_tick_scheduled
      {
        // pass, will be polled again
      } else if self.state.borrow().dyn_module_evaluate_idle_counter >= 1 {
        let scope = &mut self.handle_scope();
        let messages = find_stalled_top_level_await(scope);
        // We are gonna print only a single message to provide a nice formatting
        // with source line of offending promise shown. Once user fixed it, then
        // they will get another error message for the next promise (but this
        // situation is gonna be very rare, if ever happening).
        assert!(!messages.is_empty());
        let msg = v8::Local::new(scope, messages[0].clone());
        let js_error = JsError::from_v8_message(scope, msg);
        return Poll::Ready(Err(js_error.into()));
      } else {
        let mut state = self.state.borrow_mut();
        // Delay the above error by one spin of the event loop. A dynamic import
        // evaluation may complete during this, in which case the counter will
        // reset.
        state.dyn_module_evaluate_idle_counter += 1;
        state.waker.wake();
      }
    }
    */

    Poll::Pending
  }

  fn event_loop_pending_state(&mut self) -> EventLoopPendingState {
    EventLoopPendingState::new(
      self.v8_isolate.as_mut().unwrap(),
      &mut self.state.borrow_mut(),
      &self.module_map.as_ref().unwrap().borrow(),
    )
  }

  pub(crate) fn event_loop_pending_state_from_isolate(
    isolate: &mut v8::Isolate,
  ) -> EventLoopPendingState {
    EventLoopPendingState::new(
      isolate,
      &mut Self::state(isolate).borrow_mut(),
      &Self::module_map(isolate).borrow(),
    )
  }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct EventLoopPendingState {
  has_pending_refed_ops: bool,
  has_tick_scheduled: bool,
}
impl EventLoopPendingState {
  pub fn new(
    isolate: &mut v8::Isolate,
    state: &mut JsRuntimeState,
    _module_map: &ModuleMap,
  ) -> EventLoopPendingState {
    let mut num_unrefed_ops = 0;
    for weak_context in &state.known_realms {
      #[allow(irrefutable_let_patterns)]
      if let context = weak_context.clone() {
        let realm = JsRealm(context);
        num_unrefed_ops += realm.state(isolate).borrow().unrefed_ops.len();
      }
    }

    EventLoopPendingState {
      has_pending_refed_ops: state.pending_ops.len() > num_unrefed_ops,
      has_tick_scheduled: state.has_tick_scheduled,
    }
  }

  pub fn is_pending(&self) -> bool {
    self.has_pending_refed_ops
      || self.has_tick_scheduled
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

pub(crate) fn exception_to_err_result<T>(
  scope: &mut v8::HandleScope,
  exception: v8::Local<v8::Value>,
  in_promise: bool,
) -> Result<T, Error> {
  let state_rc = JsRuntime::state(scope);

  let was_terminating_execution = scope.is_execution_terminating();
  // If TerminateExecution was called, cancel isolate termination so that the
  // exception can be created. Note that `scope.is_execution_terminating()` may
  // have returned false if TerminateExecution was indeed called but there was
  // no JS to execute after the call.

  // TODO(minus_v8) properly implement terminating exceptions
  // scope.cancel_terminate_execution();
  let mut exception = exception;
  {
    // If termination is the result of a `op_dispatch_exception` call, we want
    // to use the exception that was passed to it rather than the exception that
    // was passed to this function.
    let state = state_rc.borrow();
    exception = state
      .dispatched_exceptions
      .back()
      .map(|exception| v8::Local::new(scope, exception.clone()))
      .unwrap_or_else(|| {
        // Maybe make a new exception object.
        if was_terminating_execution && exception.is_null_or_undefined() {
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

  if was_terminating_execution {
    // Resume exception termination.
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
  pub async fn mod_evaluate(
    &mut self,
    id: ModuleId,
  ) -> oneshot::Receiver<Result<(), Error>> {
    let state_rc = self.state.clone();
    let module_map_rc = Self::module_map(self.v8_isolate());

    let module_map_borrow = module_map_rc.borrow();
    if !module_map_borrow.by_id.contains_key(&id) {
      panic!("ModuleInfo not found");
    }

    let (sender, receiver) = oneshot::channel();

    self
      .v8_isolate()
      .backend
      .execute_module()(self.v8_isolate(), id, sender)
      .await
      .expect("minus_v8: failed to import module");

    /*
    let scope = &mut self.handle_scope();
    let tc_scope = &mut v8::TryCatch::new(scope);
    let has_dispatched_exception =
      !state_rc.borrow_mut().dispatched_exceptions.is_empty();
    if has_dispatched_exception {
      // This will be overrided in `exception_to_err_result()`.
      let exception = v8::undefined(tc_scope).into();
      sender
        .send(exception_to_err_result(tc_scope, exception, false))
        .expect("Failed to send module evaluation error.");
    } else if tc_scope.has_terminated() || tc_scope.is_execution_terminating() {
      sender.send(Err(
        generic_error("Cannot evaluate module, because JavaScript execution has been terminated.")
      )).expect("Failed to send module evaluation error.");
    }
    */

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
    let id = ModuleMap::load_main(module_map_rc, specifier.as_str()).await?;
    self.v8_isolate().backend.load_module()(self.v8_isolate(), id, specifier.as_str(), code).await;
    Ok(id)
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
    let id = ModuleMap::load_side(module_map_rc, specifier.as_str()).await?;
    self.v8_isolate().backend.load_module()(self.v8_isolate(), id, specifier.as_str(), code).await;
    Ok(id)
  }

  // TODO(minus_v8) promise rejection callback
  /*
  fn check_promise_rejections(&mut self) -> Result<(), Error> {
    let known_realms = self.state.borrow().known_realms.clone();
    let isolate = self.v8_isolate();
    for weak_context in known_realms {
      if let Some(context) = weak_context.to_global(isolate) {
        JsRealm(context).check_promise_rejections(isolate)?;
      }
    }
    Ok(())
  }
  */

  // Send finished responses to JS
  fn resolve_async_ops(&mut self, cx: &mut Context) -> Result<(), Error> {
    // We have a specialized implementation of this method for the common case
    // where there is only one realm.
    let num_realms = self.state.borrow().known_realms.len();
    if num_realms == 1 {
      return self.resolve_single_realm_async_ops(cx);
    }

    // `responses_per_realm[idx]` is a vector containing the promise ID and
    // response for all promises in realm `self.state.known_realms[idx]`.
    let mut responses_per_realm: Vec<Vec<(PromiseId, OpResult)>> =
      (0..num_realms).map(|_| vec![]).collect();

    // Now handle actual ops.
    {
      let mut state = self.state.borrow_mut();
      state.have_unpolled_ops = false;

      while let Poll::Ready(Some(item)) = state.pending_ops.poll_next_unpin(cx)
      {
        let (realm_idx, promise_id, op_id, resp) = item;
        state.op_state.borrow().tracker.track_async_completed(op_id);
        responses_per_realm[realm_idx].push((promise_id, resp));
      }
    }

    // Handle responses for each realm.
    let isolate = self.v8_isolate.as_mut().unwrap();
    for (realm_idx, responses) in responses_per_realm.into_iter().enumerate() {
      if responses.is_empty() {
        continue;
      }

      let realm = {
        let context = self.state.borrow().known_realms[realm_idx].clone();
        JsRealm(context)
      };
      let context_state_rc = realm.state(isolate);
      let mut context_state = context_state_rc.borrow_mut();
      let scope = &mut realm.handle_scope(isolate);

      // We return async responses to JS in unbounded batches (may change),
      // each batch is a flat vector of tuples:
      // `[promise_id1, op_result1, promise_id2, op_result2, ...]`
      // promise_id is a simple integer, op_result is an ops::OpResult
      // which contains a value OR an error, encoded as a tuple.
      // This batch is received in JS via the special `arguments` variable
      // and then each tuple is used to resolve or reject promises
      //
      // This can handle 16 promises (32 / 2) futures in a single batch without heap
      // allocations.
      let mut args: Vec<Box<dyn serde_v8::ErasedSerialize>> =
        Vec::with_capacity(responses.len() * 2);

      for (promise_id, mut resp) in responses {
        context_state.unrefed_ops.remove(&promise_id);
        args.push(Box::new(promise_id as i32));
        args.push(match resp.to_v8(scope) {
          Ok(v) => v,
          Err(e) => OpResult::Err(OpError::new(&|_| "TypeError", e.into()))
            .to_v8(scope)
            .unwrap(),
        });
      }

      let js_recv_cb_handle = context_state.js_recv_cb.clone().unwrap();
      let tc_scope = &mut v8::TryCatch::new(scope);
      let js_recv_cb = js_recv_cb_handle.open(tc_scope);
      let this = v8::undefined(tc_scope).into();
      drop(context_state);
      js_recv_cb.call_with_serialized(tc_scope, this, args);

      if let Some(exception) = tc_scope.exception() {
        // TODO(@andreubotella): Returning here can cause async ops in other
        // realms to never resolve.
        return exception_to_err_result(tc_scope, exception, false);
      }
    }

    Ok(())
  }

  fn resolve_single_realm_async_ops(
    &mut self,
    cx: &mut Context,
  ) -> Result<(), Error> {
    let isolate = self.v8_isolate.as_mut().unwrap();
    let scope = &mut self
      .state
      .borrow()
      .global_realm
      .as_ref()
      .unwrap()
      .handle_scope(isolate);

    // We return async responses to JS in unbounded batches (may change),
    // each batch is a flat vector of tuples:
    // `[promise_id1, op_result1, promise_id2, op_result2, ...]`
    // promise_id is a simple integer, op_result is an ops::OpResult
    // which contains a value OR an error, encoded as a tuple.
    // This batch is received in JS via the special `arguments` variable
    // and then each tuple is used to resolve or reject promises
    //
    // This can handle 16 promises (32 / 2) futures in a single batch without heap
    // allocations.
    let mut args: Vec<Box<dyn serde_v8::ErasedSerialize>> = vec![];

    // Now handle actual ops.
    {
      let mut state = self.state.borrow_mut();
      state.have_unpolled_ops = false;

      let realm_state_rc = state.global_realm.as_ref().unwrap().state(scope);
      let mut realm_state = realm_state_rc.borrow_mut();

      while let Poll::Ready(Some(item)) = state.pending_ops.poll_next_unpin(cx)
      {
        let (_realm_idx, promise_id, op_id, mut resp) = item;
        // debug_assert_eq!(
        //   state.known_realms[realm_idx],
        //   state.global_realm.as_ref().unwrap().context()
        // );
        realm_state.unrefed_ops.remove(&promise_id);
        state.op_state.borrow().tracker.track_async_completed(op_id);
        args.push(Box::new(promise_id as i32));
        args.push(match resp.to_v8(scope) {
          Ok(v) => v,
          Err(e) => OpResult::Err(OpError::new(&|_| "TypeError", e.into()))
            .to_v8(scope)
            .unwrap(),
        });
      }
    }

    if args.is_empty() {
      return Ok(());
    }

    let js_recv_cb_handle = {
      let state = self.state.borrow_mut();
      let realm_state_rc = state.global_realm.as_ref().unwrap().state(scope);
      let handle = realm_state_rc.borrow().js_recv_cb.clone().unwrap();
      handle
    };
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
    if self.state.borrow().js_macrotask_cbs.is_empty() {
      return Ok(());
    }

    let js_macrotask_cb_handles = self.state.borrow().js_macrotask_cbs.clone();
    let scope = &mut self.handle_scope();

    for js_macrotask_cb_handle in js_macrotask_cb_handles {
      let js_macrotask_cb = js_macrotask_cb_handle.open(scope);

      // Repeatedly invoke macrotask callback until it returns true (done),
      // such that ready microtasks would be automatically run before
      // next macrotask is processed.
      let tc_scope = &mut v8::TryCatch::new(scope);
      let this: Local<Value> = v8::undefined(tc_scope).into();
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
    if self.state.borrow().js_nexttick_cbs.is_empty() {
      return Ok(());
    }

    let state = self.state.clone();

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
/// A [`JsRealm`] instance is a reference to an already existing realm, which
/// does not hold ownership of it, so instances can be created and dropped as
/// needed. As such, calling [`JsRealm::new`] doesn't create a new realm, and
/// cloning a [`JsRealm`] only creates a new reference. See
/// [`JsRuntime::create_realm`] to create new realms instead.
///
/// Despite [`JsRealm`] instances being references, multiple instances that
/// point to the same realm won't overlap because every operation requires
/// passing a mutable reference to the [`v8::Isolate`]. Therefore, no operation
/// on two [`JsRealm`] instances tied to the same isolate can be run at the same
/// time, regardless of whether they point to the same realm.
///
/// # Panics
///
/// Every method of [`JsRealm`] will panic if you call it with a reference to a
/// [`v8::Isolate`] other than the one that corresponds to the current context.
///
/// # Lifetime of the realm
///
/// As long as the corresponding isolate is alive, a [`JsRealm`] instance will
/// keep the underlying V8 context alive even if it would have otherwise been
/// garbage collected.
#[derive(Clone)]
pub struct JsRealm(v8::Global<v8::Context>);
impl JsRealm {
  pub fn new(context: v8::Global<v8::Context>) -> Self {
    JsRealm(context)
  }

  pub fn context(&self) -> &v8::Global<v8::Context> {
    &self.0
  }

  fn state(&self, isolate: &mut v8::Isolate) -> Rc<RefCell<ContextState>> {
    isolate
      .get_slot::<Rc<RefCell<ContextState>>>()
      .unwrap()
      .clone()
  }

  pub(crate) fn state_from_scope(
    scope: &mut v8::HandleScope,
  ) -> Rc<RefCell<ContextState>> {
    scope
      .get_slot::<Rc<RefCell<ContextState>>>()
      .unwrap()
      .clone()
  }

  pub fn handle_scope<'s>(
    &self,
    isolate: &'s mut v8::Isolate,
  ) -> v8::HandleScope<'s> {
    v8::HandleScope::with_context(isolate, &self.0)
  }

  pub fn global_object<'s>(
    &self,
    isolate: &'s mut v8::Isolate,
  ) -> v8::Local<'s, v8::Object> {
    let scope = &mut self.handle_scope(isolate);
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
    isolate: &mut v8::Isolate,
    name: &str,
    source_code: &str,
  ) -> Result<v8::Global<v8::Value>, Error> {
    let scope = &mut self.handle_scope(isolate);

    let tc_scope = &mut v8::TryCatch::new(scope);

    match futures::executor::block_on((tc_scope.backend.execute_script())(tc_scope, name, source_code)) {
      Some(value) => {
        let value_handle = unsafe {
          v8::Global::from_raw(tc_scope, value).unwrap()
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

  // TODO(andreubotella): `mod_evaluate`, `load_main_module`, `load_side_module`

  // TODO(minus_v8) promise rejection
  /*
  fn check_promise_rejections(
    &self,
    isolate: &mut v8::Isolate,
  ) -> Result<(), Error> {
    let context_state_rc = self.state(isolate);
    let mut context_state = context_state_rc.borrow_mut();

    if context_state.pending_promise_rejections.is_empty() {
      return Ok(());
    }

    let key = {
      context_state
        .pending_promise_rejections
        .keys()
        .next()
        .unwrap()
        .clone()
    };
    let handle = context_state
      .pending_promise_rejections
      .remove(&key)
      .unwrap();
    drop(context_state);

    let scope = &mut self.handle_scope(isolate);
    let exception = v8::Local::new(scope, handle);
    exception_to_err_result(scope, exception, true)
  }
  */
}

#[inline]
pub fn queue_fast_async_op(
  ctx: &OpCtx,
  op: impl Future<Output = (RealmIdx, PromiseId, OpId, OpResult)> + 'static,
) {
  let runtime_state = match ctx.runtime_state.upgrade() {
    Some(rc_state) => rc_state,
    // atleast 1 Rc is held by the JsRuntime.
    None => unreachable!(),
  };

  let mut state = runtime_state.borrow_mut();
  state.pending_ops.push(OpCall::lazy(op));
  state.have_unpolled_ops = true;
}

#[inline]
pub fn queue_async_op(
  ctx: &OpCtx,
  scope: &mut v8::HandleScope,
  deferred: bool,
  op: impl Future<Output = (RealmIdx, PromiseId, OpId, OpResult)> + 'static,
) {
  let runtime_state = match ctx.runtime_state.upgrade() {
    Some(rc_state) => rc_state,
    // atleast 1 Rc is held by the JsRuntime.
    None => unreachable!(),
  };

  // An op's realm (as given by `OpCtx::realm_idx`) must match the realm in
  // which it is invoked. Otherwise, we might have cross-realm object exposure.
  // deno_core doesn't currently support such exposure, even though embedders
  // can cause them, so we panic in debug mode (since the check is expensive).
  // debug_assert_eq!(
  //   runtime_state.borrow().known_realms[ctx.realm_idx].to_local(scope),
  //   Some(scope.get_current_context())
  // );

  match OpCall::eager(op) {
    // This calls promise.resolve() before the control goes back to userland JS. It works something
    // along the lines of:
    //
    // function opresolve(promiseId, ...) {
    //   getPromise(promiseId).resolve(...);
    // }
    // const p = setPromise();
    // op.op_async(promiseId, ...); // Calls `opresolve`
    // return p;
    EagerPollResult::Ready((_, promise_id, op_id, mut resp)) if !deferred => {
      let context_state_rc = JsRealm::state_from_scope(scope);
      let context_state = context_state_rc.borrow();

      let args: Vec<Box<dyn serde_v8::ErasedSerialize>> = vec![
        Box::new(promise_id),
        resp.to_v8(scope).unwrap(),
      ];

      ctx.state.borrow_mut().tracker.track_async_completed(op_id);

      let tc_scope = &mut v8::TryCatch::new(scope);
      let js_recv_cb =
        context_state.js_recv_cb.as_ref().unwrap().open(tc_scope);
      let this = v8::undefined(tc_scope).into();
      js_recv_cb.call_with_serialized(tc_scope, this, args);
    }
    EagerPollResult::Ready(op) => {
      let ready = OpCall::ready(op);
      let mut state = runtime_state.borrow_mut();
      state.pending_ops.push(ready);
      state.have_unpolled_ops = true;
    }
    EagerPollResult::Pending(op) => {
      let mut state = runtime_state.borrow_mut();
      state.pending_ops.push(op);
      state.have_unpolled_ops = true;
    }
  }
}

/*
#[cfg(test)]
pub mod tests {
  use super::*;
  use crate::error::custom_error;
  use crate::error::AnyError;
  use crate::modules::AssertedModuleType;
  use crate::modules::ModuleInfo;
  use crate::modules::ModuleSource;
  use crate::modules::ModuleSourceFuture;
  use crate::modules::ModuleType;
  use crate::modules::ResolutionKind;
  use crate::modules::SymbolicModule;
  use crate::ZeroCopyBuf;
  use deno_ops::op;
  use futures::future::lazy;
  use std::ops::FnOnce;
  use std::pin::Pin;
  use std::rc::Rc;
  use std::sync::atomic::AtomicUsize;
  use std::sync::atomic::Ordering;
  use std::sync::Arc;

  // deno_ops macros generate code assuming deno_core in scope.
  mod deno_core {
    pub use crate::*;
  }

  pub fn run_in_task<F>(f: F)
  where
    F: FnOnce(&mut Context) + 'static,
  {
    futures::executor::block_on(lazy(move |cx| f(cx)));
  }

  #[derive(Copy, Clone)]
  enum Mode {
    Async,
    AsyncDeferred,
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
    #![allow(clippy::await_holding_refcell_ref)] // False positive.
    let op_state_ = rc_op_state.borrow();
    let test_state = op_state_.borrow::<TestState>();
    test_state.dispatch_count.fetch_add(1, Ordering::Relaxed);
    let mode = test_state.mode;
    drop(op_state_);
    match mode {
      Mode::Async => {
        assert_eq!(control, 42);
        Ok(43)
      }
      Mode::AsyncDeferred => {
        tokio::task::yield_now().await;
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
    let ext = Extension::builder("test_ext")
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
  fn test_ref_unref_ops() {
    let (mut runtime, _dispatch_count) = setup(Mode::AsyncDeferred);
    runtime
      .execute_script(
        "filename.js",
        r#"
        Deno.core.initializeAsyncOps();
        var promiseIdSymbol = Symbol.for("Deno.core.internalPromiseId");
        var p1 = Deno.core.ops.op_test(42);
        var p2 = Deno.core.ops.op_test(42);
        "#,
      )
      .unwrap();
    {
      let realm = runtime.global_realm();
      let isolate = runtime.v8_isolate();
      let state_rc = JsRuntime::state(isolate);
      assert_eq!(state_rc.borrow().pending_ops.len(), 2);
      assert_eq!(realm.state(isolate).borrow().unrefed_ops.len(), 0);
    }
    runtime
      .execute_script(
        "filename.js",
        r#"
        Deno.core.ops.op_unref_op(p1[promiseIdSymbol]);
        Deno.core.ops.op_unref_op(p2[promiseIdSymbol]);
        "#,
      )
      .unwrap();
    {
      let realm = runtime.global_realm();
      let isolate = runtime.v8_isolate();
      let state_rc = JsRuntime::state(isolate);
      assert_eq!(state_rc.borrow().pending_ops.len(), 2);
      assert_eq!(realm.state(isolate).borrow().unrefed_ops.len(), 2);
    }
    runtime
      .execute_script(
        "filename.js",
        r#"
        Deno.core.ops.op_ref_op(p1[promiseIdSymbol]);
        Deno.core.ops.op_ref_op(p2[promiseIdSymbol]);
        "#,
      )
      .unwrap();
    {
      let realm = runtime.global_realm();
      let isolate = runtime.v8_isolate();
      let state_rc = JsRuntime::state(isolate);
      assert_eq!(state_rc.borrow().pending_ops.len(), 2);
      assert_eq!(realm.state(isolate).borrow().unrefed_ops.len(), 0);
    }
  }

  #[test]
  fn test_dispatch() {
    let (mut runtime, dispatch_count) = setup(Mode::Async);
    runtime
      .execute_script(
        "filename.js",
        r#"
        let control = 42;
        Deno.core.initializeAsyncOps();
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
        Deno.core.initializeAsyncOps();
        const p = Deno.core.opAsync("op_test", 42);
        if (p[Symbol.for("Deno.core.internalPromiseId")] == undefined) {
          throw new Error("missing id on returned promise");
        }
        "#,
      )
      .unwrap();
  }

  #[test]
  fn test_dispatch_no_zero_copy_buf() {
    let (mut runtime, dispatch_count) = setup(Mode::AsyncZeroCopy(false));
    runtime
      .execute_script(
        "filename.js",
        r#"
        Deno.core.initializeAsyncOps();
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
        Deno.core.initializeAsyncOps();
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

  #[tokio::test]
  async fn test_poll_value() {
    let mut runtime = JsRuntime::new(Default::default());
    run_in_task(move |cx| {
      let value_global = runtime
        .execute_script("a.js", "Promise.resolve(1 + 2)")
        .unwrap();
      let v = runtime.poll_value(&value_global, cx);
      {
        let scope = &mut runtime.handle_scope();
        assert!(
          matches!(v, Poll::Ready(Ok(v)) if v.open(scope).integer_value(scope).unwrap() == 3)
        );
      }

      let value_global = runtime
        .execute_script(
          "a.js",
          "Promise.resolve(new Promise(resolve => resolve(2 + 2)))",
        )
        .unwrap();
      let v = runtime.poll_value(&value_global, cx);
      {
        let scope = &mut runtime.handle_scope();
        assert!(
          matches!(v, Poll::Ready(Ok(v)) if v.open(scope).integer_value(scope).unwrap() == 4)
        );
      }

      let value_global = runtime
        .execute_script("a.js", "Promise.reject(new Error('fail'))")
        .unwrap();
      let v = runtime.poll_value(&value_global, cx);
      assert!(
        matches!(v, Poll::Ready(Err(e)) if e.downcast_ref::<JsError>().unwrap().exception_message == "Uncaught Error: fail")
      );

      let value_global = runtime
        .execute_script("a.js", "new Promise(resolve => {})")
        .unwrap();
      let v = runtime.poll_value(&value_global, cx);
      matches!(v, Poll::Ready(Err(e)) if e.to_string() == "Promise resolution is still pending but the event loop has already resolved.");
    });
  }

  #[tokio::test]
  async fn test_resolve_value() {
    let mut runtime = JsRuntime::new(Default::default());
    let value_global = runtime
      .execute_script("a.js", "Promise.resolve(1 + 2)")
      .unwrap();
    let result_global = runtime.resolve_value(value_global).await.unwrap();
    {
      let scope = &mut runtime.handle_scope();
      let value = result_global.open(scope);
      assert_eq!(value.integer_value(scope).unwrap(), 3);
    }

    let value_global = runtime
      .execute_script(
        "a.js",
        "Promise.resolve(new Promise(resolve => resolve(2 + 2)))",
      )
      .unwrap();
    let result_global = runtime.resolve_value(value_global).await.unwrap();
    {
      let scope = &mut runtime.handle_scope();
      let value = result_global.open(scope);
      assert_eq!(value.integer_value(scope).unwrap(), 4);
    }

    let value_global = runtime
      .execute_script("a.js", "Promise.reject(new Error('fail'))")
      .unwrap();
    let err = runtime.resolve_value(value_global).await.unwrap_err();
    assert_eq!(
      "Uncaught Error: fail",
      err.downcast::<JsError>().unwrap().exception_message
    );

    let value_global = runtime
      .execute_script("a.js", "new Promise(resolve => {})")
      .unwrap();
    let error_string = runtime
      .resolve_value(value_global)
      .await
      .unwrap_err()
      .to_string();
    assert_eq!(
      "Promise resolution is still pending but the event loop has already resolved.",
      error_string,
    );
  }

  #[test]
  fn terminate_execution_webassembly() {
    let (mut runtime, _dispatch_count) = setup(Mode::Async);
    let v8_isolate_handle = runtime.v8_isolate().thread_safe_handle();

    // Run an infinite loop in Webassemby code, which should be terminated.
    let promise = runtime.execute_script("infinite_wasm_loop.js",
                                 r#"
                                 (async () => {
                                  const wasmCode = new Uint8Array([
                                      0,    97,   115,  109,  1,    0,    0,    0,    1,   4,    1,
                                      96,   0,    0,    3,    2,    1,    0,    7,    17,  1,    13,
                                      105,  110,  102,  105,  110,  105,  116,  101,  95,  108,  111,
                                      111,  112,  0,    0,    10,   9,    1,    7,    0,   3,    64,
                                      12,   0,    11,   11,
                                  ]);
                                  const wasmModule = await WebAssembly.compile(wasmCode);
                                  globalThis.wasmInstance = new WebAssembly.Instance(wasmModule);
                                  })()
                                      "#).unwrap();
    futures::executor::block_on(runtime.resolve_value(promise)).unwrap();
    let terminator_thread = std::thread::spawn(move || {
      std::thread::sleep(std::time::Duration::from_millis(1000));

      // terminate execution
      let ok = v8_isolate_handle.terminate_execution();
      assert!(ok);
    });
    let err = runtime
      .execute_script(
        "infinite_wasm_loop2.js",
        "globalThis.wasmInstance.exports.infinite_loop();",
      )
      .unwrap_err();
    assert_eq!(err.to_string(), "Uncaught Error: execution terminated");
    // Cancel the execution-terminating exception in order to allow script
    // execution again.
    let ok = runtime.v8_isolate().cancel_terminate_execution();
    assert!(ok);

    // Verify that the isolate usable again.
    runtime
      .execute_script("simple.js", "1 + 1")
      .expect("execution should be possible again");

    terminator_thread.join().unwrap();
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
  fn dangling_shared_isolate() {
    let v8_isolate_handle = {
      // isolate is dropped at the end of this block
      let (mut runtime, _dispatch_count) = setup(Mode::Async);
      runtime.v8_isolate().thread_safe_handle()
    };

    // this should not SEGFAULT
    v8_isolate_handle.terminate_execution();
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
    let (mut runtime, _dispatch_count) = setup(Mode::Async);
    run_in_task(move |cx| {
      runtime
        .execute_script(
          "encode_decode_test.js",
          include_str!("encode_decode_test.js"),
        )
        .unwrap();
      if let Poll::Ready(Err(_)) = runtime.poll_event_loop(cx, false) {
        unreachable!();
      }
    });
  }

  #[test]
  fn test_serialize_deserialize() {
    let (mut runtime, _dispatch_count) = setup(Mode::Async);
    run_in_task(move |cx| {
      runtime
        .execute_script(
          "serialize_deserialize_test.js",
          include_str!("serialize_deserialize_test.js"),
        )
        .unwrap();
      if let Poll::Ready(Err(_)) = runtime.poll_event_loop(cx, false) {
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

    let ext = Extension::builder("test_ext")
      .ops(vec![op_err::decl()])
      .build();
    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![ext],
      get_error_class_fn: Some(&get_error_class_name),
      ..Default::default()
    });
    run_in_task(move |cx| {
      runtime
        .execute_script(
          "error_builder_test.js",
          include_str!("error_builder_test.js"),
        )
        .unwrap();
      if let Poll::Ready(Err(_)) = runtime.poll_event_loop(cx, false) {
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
    at main (error_stack.js:8:3)
    at error_stack.js:10:1"#;
    assert_eq!(result.unwrap_err().to_string(), expected_error);
  }

  #[test]
  fn test_error_async_stack() {
    let mut runtime = JsRuntime::new(RuntimeOptions::default());
    run_in_task(move |cx| {
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
    at async error_async_stack.js:9:5"#;

      match runtime.poll_event_loop(cx, false) {
        Poll::Ready(Err(e)) => {
          assert_eq!(e.to_string(), expected_error);
        }
        _ => panic!(),
      };
    })
  }

  #[test]
  fn test_error_context() {
    use anyhow::anyhow;

    #[op]
    fn op_err_sync() -> Result<(), Error> {
      Err(anyhow!("original sync error").context("higher-level sync error"))
    }

    #[op]
    async fn op_err_async() -> Result<(), Error> {
      Err(anyhow!("original async error").context("higher-level async error"))
    }

    let ext = Extension::builder("test_ext")
      .ops(vec![op_err_sync::decl(), op_err_async::decl()])
      .build();
    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![ext],
      ..Default::default()
    });

    run_in_task(move |cx| {
      runtime
        .execute_script(
          "test_error_context_sync.js",
          r#"
let errMessage;
try {
  Deno.core.ops.op_err_sync();
} catch (err) {
  errMessage = err.message;
}
if (errMessage !== "higher-level sync error: original sync error") {
  throw new Error("unexpected error message from op_err_sync: " + errMessage);
}
"#,
        )
        .unwrap();

      let promise = runtime
        .execute_script(
          "test_error_context_async.js",
          r#"
Deno.core.initializeAsyncOps();
(async () => {
  let errMessage;
  try {
    await Deno.core.opAsync("op_err_async");
  } catch (err) {
    errMessage = err.message;
  }
  if (errMessage !== "higher-level async error: original async error") {
    throw new Error("unexpected error message from op_err_async: " + errMessage);
  }
})()
"#,
        )
        .unwrap();

      match runtime.poll_value(&promise, cx) {
        Poll::Ready(Ok(_)) => {}
        Poll::Ready(Err(err)) => panic!("{err:?}"),
        _ => panic!(),
      }
    })
  }

  #[test]
  fn test_pump_message_loop() {
    let mut runtime = JsRuntime::new(RuntimeOptions::default());
    run_in_task(move |cx| {
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

      match runtime.poll_event_loop(cx, false) {
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
        "Deno.core.opAsync('non_existent');",
      )
      .unwrap_err();
    let error_string = error.to_string();
    // Test that the script specifier is a URL: `internal:<repo-relative path>`.
    assert!(error_string.contains("internal:core/01_core.js"));
  }

  #[test]
  fn test_v8_platform() {
    let options = RuntimeOptions {
      v8_platform: Some(v8::new_default_platform(0, false).make_shared()),
      ..Default::default()
    };
    let mut runtime = JsRuntime::new(options);
    runtime.execute_script("<none>", "").unwrap();
  }

  #[ignore] // TODO(@littledivy): Fast API ops when snapshot is not loaded.
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
        return Deno.core.ops.op_is_proxy(p) && !Deno.core.ops.op_is_proxy(o) && !Deno.core.ops.op_is_proxy(42);
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

    let extension = Extension::builder("test_ext")
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
        "Deno.core.initializeAsyncOps(); Deno.core.ops.op_async_borrow()",
      )
      .unwrap();
    runtime.run_event_loop(false).await.unwrap();
  }

  #[tokio::test]
  async fn test_sync_op_serialize_object_with_numbers_as_keys() {
    #[op]
    fn op_sync_serialize_object_with_numbers_as_keys(
      value: serde_json::Value,
    ) -> Result<(), Error> {
      assert_eq!(
        value.to_string(),
        r#"{"lines":{"100":{"unit":"m"},"200":{"unit":"cm"}}}"#
      );
      Ok(())
    }

    let extension = Extension::builder("test_ext")
      .ops(vec![op_sync_serialize_object_with_numbers_as_keys::decl()])
      .build();

    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![extension],
      ..Default::default()
    });

    runtime
      .execute_script(
        "op_sync_serialize_object_with_numbers_as_keys.js",
        r#"
Deno.core.ops.op_sync_serialize_object_with_numbers_as_keys({
  lines: {
    100: {
      unit: "m"
    },
    200: {
      unit: "cm"
    }
  }
})
"#,
      )
      .unwrap();
    runtime.run_event_loop(false).await.unwrap();
  }

  #[tokio::test]
  async fn test_async_op_serialize_object_with_numbers_as_keys() {
    #[op]
    async fn op_async_serialize_object_with_numbers_as_keys(
      value: serde_json::Value,
    ) -> Result<(), Error> {
      assert_eq!(
        value.to_string(),
        r#"{"lines":{"100":{"unit":"m"},"200":{"unit":"cm"}}}"#
      );
      Ok(())
    }

    let extension = Extension::builder("test_ext")
      .ops(vec![op_async_serialize_object_with_numbers_as_keys::decl()])
      .build();

    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![extension],
      ..Default::default()
    });

    runtime
      .execute_script(
        "op_async_serialize_object_with_numbers_as_keys.js",
        r#"
Deno.core.initializeAsyncOps();
Deno.core.ops.op_async_serialize_object_with_numbers_as_keys({
  lines: {
    100: {
      unit: "m"
    },
    200: {
      unit: "cm"
    }
  }
})
"#,
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

    let extension = Extension::builder("test_ext")
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
        Deno.core.initializeAsyncOps();
        (async function () {
          const results = [];
          Deno.core.ops.op_set_macrotask_callback(() => {
            results.push("macrotask");
            return true;
          });
          Deno.core.ops.op_set_next_tick_callback(() => {
            results.push("nextTick");
            Deno.core.ops.op_set_has_tick_scheduled(false);
          });
          Deno.core.ops.op_set_has_tick_scheduled(true);
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
        Deno.core.ops.op_set_macrotask_callback(() => { return true; });
        Deno.core.ops.op_set_macrotask_callback(() => { return true; });
        Deno.core.ops.op_set_next_tick_callback(() => {});
        Deno.core.ops.op_set_next_tick_callback(() => {});
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

    let extension = Extension::builder("test_ext")
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
          Deno.core.ops.op_set_macrotask_callback(() => {
            Deno.core.ops.op_macrotask();
            return true; // We're done.
          });
          Deno.core.ops.op_set_next_tick_callback(() => Deno.core.ops.op_next_tick());
          Deno.core.ops.op_set_has_tick_scheduled(true);
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

  #[test]
  fn terminate_during_module_eval() {
    #[derive(Default)]
    struct ModsLoader;

    impl ModuleLoader for ModsLoader {
      fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
      ) -> Result<ModuleSpecifier, Error> {
        assert_eq!(specifier, "file:///main.js");
        assert_eq!(referrer, ".");
        let s = crate::resolve_import(specifier, referrer).unwrap();
        Ok(s)
      }

      fn load(
        &self,
        _module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<ModuleSpecifier>,
        _is_dyn_import: bool,
      ) -> Pin<Box<ModuleSourceFuture>> {
        async move {
          Ok(ModuleSource {
            code: b"console.log('hello world');".to_vec().into_boxed_slice(),
            module_url_specified: "file:///main.js".to_string(),
            module_url_found: "file:///main.js".to_string(),
            module_type: ModuleType::JavaScript,
          })
        }
        .boxed_local()
      }
    }

    let loader = std::rc::Rc::new(ModsLoader::default());
    let mut runtime = JsRuntime::new(RuntimeOptions {
      module_loader: Some(loader),
      ..Default::default()
    });

    let specifier = crate::resolve_url("file:///main.js").unwrap();
    let source_code = "Deno.core.print('hello\\n')".to_string();

    let module_id = futures::executor::block_on(
      runtime.load_main_module(&specifier, Some(source_code)),
    )
    .unwrap();

    runtime.v8_isolate().terminate_execution();

    let mod_result =
      futures::executor::block_on(runtime.mod_evaluate(module_id)).unwrap();
    assert!(mod_result
      .unwrap_err()
      .to_string()
      .contains("JavaScript execution has been terminated"));
  }

  #[tokio::test]
  async fn test_set_promise_reject_callback() {
    static PROMISE_REJECT: AtomicUsize = AtomicUsize::new(0);

    #[op]
    fn op_promise_reject() -> Result<(), AnyError> {
      PROMISE_REJECT.fetch_add(1, Ordering::Relaxed);
      Ok(())
    }

    let extension = Extension::builder("test_ext")
      .ops(vec![op_promise_reject::decl()])
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
        Deno.core.ops.op_set_promise_reject_callback((type, promise, reason) => {
          if (type !== /* PromiseRejectWithNoHandler */ 0) {
            throw Error("unexpected type: " + type);
          }
          if (reason.message !== "reject") {
            throw Error("unexpected reason: " + reason);
          }
          Deno.core.ops.op_store_pending_promise_rejection(promise);
          Deno.core.ops.op_promise_reject();
        });
        new Promise((_, reject) => reject(Error("reject")));
        "#,
      )
      .unwrap();
    runtime.run_event_loop(false).await.unwrap_err();

    assert_eq!(1, PROMISE_REJECT.load(Ordering::Relaxed));

    runtime
      .execute_script(
        "promise_reject_callback.js",
        r#"
        {
          const prev = Deno.core.ops.op_set_promise_reject_callback((...args) => {
            prev(...args);
          });
        }
        new Promise((_, reject) => reject(Error("reject")));
        "#,
      )
      .unwrap();
    runtime.run_event_loop(false).await.unwrap_err();

    assert_eq!(2, PROMISE_REJECT.load(Ordering::Relaxed));
  }

  #[tokio::test]
  async fn test_set_promise_reject_callback_realms() {
    let mut runtime = JsRuntime::new(RuntimeOptions::default());
    let global_realm = runtime.global_realm();
    let realm1 = runtime.create_realm().unwrap();
    let realm2 = runtime.create_realm().unwrap();

    let realm_expectations = &[
      (&global_realm, "global_realm", 42),
      (&realm1, "realm1", 140),
      (&realm2, "realm2", 720),
    ];

    // Set up promise reject callbacks.
    for (realm, realm_name, number) in realm_expectations {
      realm
        .execute_script(
          runtime.v8_isolate(),
          "",
          &format!(
            r#"
              Deno.core.initializeAsyncOps();
              globalThis.rejectValue = undefined;
              Deno.core.setPromiseRejectCallback((_type, _promise, reason) => {{
                globalThis.rejectValue = `{realm_name}/${{reason}}`;
              }});
              Deno.core.ops.op_void_async().then(() => Promise.reject({number}));
            "#
          ),
        )
        .unwrap();
    }

    runtime.run_event_loop(false).await.unwrap();

    for (realm, realm_name, number) in realm_expectations {
      let reject_value = realm
        .execute_script(runtime.v8_isolate(), "", "globalThis.rejectValue")
        .unwrap();
      let scope = &mut realm.handle_scope(runtime.v8_isolate());
      let reject_value = v8::Local::new(scope, reject_value);
      assert!(reject_value.is_string());
      let reject_value_string = reject_value.to_rust_string_lossy(scope);
      assert_eq!(reject_value_string, format!("{realm_name}/{number}"));
    }
  }

  #[tokio::test]
  async fn test_set_promise_reject_callback_top_level_await() {
    static PROMISE_REJECT: AtomicUsize = AtomicUsize::new(0);

    #[op]
    fn op_promise_reject() -> Result<(), AnyError> {
      PROMISE_REJECT.fetch_add(1, Ordering::Relaxed);
      Ok(())
    }

    let extension = Extension::builder("test_ext")
      .ops(vec![op_promise_reject::decl()])
      .build();

    #[derive(Default)]
    struct ModsLoader;

    impl ModuleLoader for ModsLoader {
      fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
      ) -> Result<ModuleSpecifier, Error> {
        assert_eq!(specifier, "file:///main.js");
        assert_eq!(referrer, ".");
        let s = crate::resolve_import(specifier, referrer).unwrap();
        Ok(s)
      }

      fn load(
        &self,
        _module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<ModuleSpecifier>,
        _is_dyn_import: bool,
      ) -> Pin<Box<ModuleSourceFuture>> {
        let source = r#"
        Deno.core.ops.op_set_promise_reject_callback((type, promise, reason) => {
          Deno.core.ops.op_promise_reject();
        });
        throw new Error('top level throw');
        "#;

        async move {
          Ok(ModuleSource {
            code: source.as_bytes().to_vec().into_boxed_slice(),
            module_url_specified: "file:///main.js".to_string(),
            module_url_found: "file:///main.js".to_string(),
            module_type: ModuleType::JavaScript,
          })
        }
        .boxed_local()
      }
    }

    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![extension],
      module_loader: Some(Rc::new(ModsLoader)),
      ..Default::default()
    });

    let id = runtime
      .load_main_module(&crate::resolve_url("file:///main.js").unwrap(), None)
      .await
      .unwrap();
    let receiver = runtime.mod_evaluate(id);
    runtime.run_event_loop(false).await.unwrap();
    receiver.await.unwrap().unwrap_err();

    assert_eq!(1, PROMISE_REJECT.load(Ordering::Relaxed));
  }

  #[test]
  fn test_op_return_serde_v8_error() {
    #[op]
    fn op_err() -> Result<std::collections::BTreeMap<u64, u64>, anyhow::Error> {
      Ok([(1, 2), (3, 4)].into_iter().collect()) // Maps can't have non-string keys in serde_v8
    }

    let ext = Extension::builder("test_ext")
      .ops(vec![op_err::decl()])
      .build();
    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![ext],
      ..Default::default()
    });
    assert!(runtime
      .execute_script(
        "test_op_return_serde_v8_error.js",
        "Deno.core.ops.op_err()"
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

    let ext = Extension::builder("test_ext")
      .ops(vec![op_add_4::decl()])
      .build();
    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![ext],
      ..Default::default()
    });
    let r = runtime
      .execute_script("test.js", "Deno.core.ops.op_add_4(1, 2, 3, 4)")
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

    let ext = Extension::builder("test_ext")
      .ops(vec![op_foo::decl().disable()])
      .build();
    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![ext],
      ..Default::default()
    });
    let r = runtime
      .execute_script("test.js", "Deno.core.ops.op_foo()")
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

    let ext = Extension::builder("test_ext")
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
        let sum = Deno.core.ops.op_sum_take(a1b);
        if (sum !== 6) {
          throw new Error(`Bad sum: ${sum}`);
        }
        if (a1.length > 0 || a1b.length > 0) {
          throw new Error("expecting a1 & a1b to be detached");
        }
        const a3 = Deno.core.ops.op_boomerang(a2b);
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
            let sum = Deno.core.ops.op_sum_take(w32.subarray(0, 2));
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

    let ext = Extension::builder("test_ext")
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
        if (Deno.core.ops.op_foo() !== 42) {
          throw new Error("Exptected op_foo() === 42");
        }
        if (Deno.core.ops.op_bar() !== undefined) {
          throw new Error("Expected op_bar to be disabled")
        }
      "#,
      )
      .unwrap();
  }

  #[test]
  fn js_realm_simple() {
    let mut runtime = JsRuntime::new(Default::default());
    let main_context = runtime.global_context();
    let main_global = {
      let scope = &mut runtime.handle_scope();
      let local_global = main_context.open(scope).global(scope);
      v8::Global::new(scope, local_global)
    };

    let realm = runtime.create_realm().unwrap();
    assert_ne!(realm.context(), &main_context);
    assert_ne!(realm.global_object(runtime.v8_isolate()), main_global);

    let main_object = runtime.execute_script("", "Object").unwrap();
    let realm_object = realm
      .execute_script(runtime.v8_isolate(), "", "Object")
      .unwrap();
    assert_ne!(main_object, realm_object);
  }

  #[test]
  fn js_realm_init() {
    #[op]
    fn op_test() -> Result<String, Error> {
      Ok(String::from("Test"))
    }

    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![Extension::builder("test_ext")
        .ops(vec![op_test::decl()])
        .build()],
      ..Default::default()
    });
    let realm = runtime.create_realm().unwrap();
    let ret = realm
      .execute_script(runtime.v8_isolate(), "", "Deno.core.ops.op_test()")
      .unwrap();

    let scope = &mut realm.handle_scope(runtime.v8_isolate());
    assert_eq!(ret, serde_v8::to_v8(scope, "Test").unwrap());
  }

  #[test]
  fn js_realm_init_snapshot() {
    let snapshot = {
      let runtime = JsRuntime::new(RuntimeOptions {
        will_snapshot: true,
        ..Default::default()
      });
      let snap: &[u8] = &runtime.snapshot();
      Vec::from(snap).into_boxed_slice()
    };

    #[op]
    fn op_test() -> Result<String, Error> {
      Ok(String::from("Test"))
    }

    let mut runtime = JsRuntime::new(RuntimeOptions {
      startup_snapshot: Some(Snapshot::Boxed(snapshot)),
      extensions: vec![Extension::builder("test_ext")
        .ops(vec![op_test::decl()])
        .build()],
      ..Default::default()
    });
    let realm = runtime.create_realm().unwrap();
    let ret = realm
      .execute_script(runtime.v8_isolate(), "", "Deno.core.ops.op_test()")
      .unwrap();

    let scope = &mut realm.handle_scope(runtime.v8_isolate());
    assert_eq!(ret, serde_v8::to_v8(scope, "Test").unwrap());
  }

  #[test]
  fn js_realm_sync_ops() {
    // Test that returning a ZeroCopyBuf and throwing an exception from a sync
    // op result in objects with prototypes from the right realm. Note that we
    // don't test the result of returning structs, because they will be
    // serialized to objects with null prototype.

    #[op]
    fn op_test(fail: bool) -> Result<ZeroCopyBuf, Error> {
      if !fail {
        Ok(ZeroCopyBuf::empty())
      } else {
        Err(crate::error::type_error("Test"))
      }
    }

    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![Extension::builder("test_ext")
        .ops(vec![op_test::decl()])
        .build()],
      get_error_class_fn: Some(&|error| {
        crate::error::get_custom_error_class(error).unwrap()
      }),
      ..Default::default()
    });
    let new_realm = runtime.create_realm().unwrap();

    // Test in both realms
    for realm in [runtime.global_realm(), new_realm].into_iter() {
      let ret = realm
        .execute_script(
          runtime.v8_isolate(),
          "",
          r#"
            const buf = Deno.core.ops.op_test(false);
            try {
              Deno.core.ops.op_test(true);
            } catch(e) {
              err = e;
            }
            buf instanceof Uint8Array && buf.byteLength === 0 &&
            err instanceof TypeError && err.message === "Test"
          "#,
        )
        .unwrap();
      assert!(ret.open(runtime.v8_isolate()).is_true());
    }
  }

  #[tokio::test]
  async fn js_realm_async_ops() {
    // Test that returning a ZeroCopyBuf and throwing an exception from a async
    // op result in objects with prototypes from the right realm. Note that we
    // don't test the result of returning structs, because they will be
    // serialized to objects with null prototype.

    #[op]
    async fn op_test(fail: bool) -> Result<ZeroCopyBuf, Error> {
      if !fail {
        Ok(ZeroCopyBuf::empty())
      } else {
        Err(crate::error::type_error("Test"))
      }
    }

    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![Extension::builder("test_ext")
        .ops(vec![op_test::decl()])
        .build()],
      get_error_class_fn: Some(&|error| {
        crate::error::get_custom_error_class(error).unwrap()
      }),
      ..Default::default()
    });

    let global_realm = runtime.global_realm();
    let new_realm = runtime.create_realm().unwrap();

    let mut rets = vec![];

    // Test in both realms
    for realm in [global_realm, new_realm].into_iter() {
      let ret = realm
        .execute_script(
          runtime.v8_isolate(),
          "",
          r#"
            Deno.core.initializeAsyncOps();
            (async function () {
              const buf = await Deno.core.ops.op_test(false);
              let err;
              try {
                await Deno.core.ops.op_test(true);
              } catch(e) {
                err = e;
              }
              return buf instanceof Uint8Array && buf.byteLength === 0 &&
                      err instanceof TypeError && err.message === "Test" ;
            })();
          "#,
        )
        .unwrap();
      rets.push((realm, ret));
    }

    runtime.run_event_loop(false).await.unwrap();

    for ret in rets {
      let scope = &mut ret.0.handle_scope(runtime.v8_isolate());
      let value = v8::Local::new(scope, ret.1);
      let promise = v8::Local::<v8::Promise>::try_from(value).unwrap();
      let result = promise.result(scope);

      assert!(result.is_boolean() && result.is_true());
    }
  }

  #[tokio::test]
  async fn js_realm_ref_unref_ops() {
    // Never resolves.
    #[op]
    async fn op_pending() {
      futures::future::pending().await
    }

    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![Extension::builder("test_ext")
        .ops(vec![op_pending::decl()])
        .build()],
      ..Default::default()
    });

    run_in_task(move |cx| {
      let main_realm = runtime.global_realm();
      let other_realm = runtime.create_realm().unwrap();

      main_realm
        .execute_script(
          runtime.v8_isolate(),
          "",
          r#"
            Deno.core.initializeAsyncOps();
            var promise = Deno.core.ops.op_pending();
          "#,
        )
        .unwrap();
      other_realm
        .execute_script(
          runtime.v8_isolate(),
          "",
          r#"
            Deno.core.initializeAsyncOps();
            var promise = Deno.core.ops.op_pending();
          "#,
        )
        .unwrap();
      assert!(matches!(runtime.poll_event_loop(cx, false), Poll::Pending));

      main_realm
        .execute_script(
          runtime.v8_isolate(),
          "",
          r#"
            let promiseIdSymbol = Symbol.for("Deno.core.internalPromiseId");
            Deno.core.unrefOp(promise[promiseIdSymbol]);
          "#,
        )
        .unwrap();
      assert!(matches!(runtime.poll_event_loop(cx, false), Poll::Pending));

      other_realm
        .execute_script(
          runtime.v8_isolate(),
          "",
          r#"
            let promiseIdSymbol = Symbol.for("Deno.core.internalPromiseId");
            Deno.core.unrefOp(promise[promiseIdSymbol]);
          "#,
        )
        .unwrap();
      assert!(matches!(
        runtime.poll_event_loop(cx, false),
        Poll::Ready(Ok(()))
      ));
    });
  }

  #[test]
  fn test_array_by_copy() {
    // Verify that "array by copy" proposal is enabled (https://github.com/tc39/proposal-change-array-by-copy)
    let mut runtime = JsRuntime::new(Default::default());
    assert!(runtime
      .execute_script(
        "test_array_by_copy.js",
        "const a = [1, 2, 3];
        const b = a.toReversed();
        if (!(a[0] === 1 && a[1] === 2 && a[2] === 3)) {
          throw new Error('Expected a to be intact');
        }
        if (!(b[0] === 3 && b[1] === 2 && b[2] === 1)) {
          throw new Error('Expected b to be reversed');
        }",
      )
      .is_ok());
  }

  #[tokio::test]
  async fn cant_load_internal_module_when_snapshot_is_loaded_and_not_snapshotting(
  ) {
    #[derive(Default)]
    struct ModsLoader;

    impl ModuleLoader for ModsLoader {
      fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
      ) -> Result<ModuleSpecifier, Error> {
        assert_eq!(specifier, "file:///main.js");
        assert_eq!(referrer, ".");
        let s = crate::resolve_import(specifier, referrer).unwrap();
        Ok(s)
      }

      fn load(
        &self,
        _module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<ModuleSpecifier>,
        _is_dyn_import: bool,
      ) -> Pin<Box<ModuleSourceFuture>> {
        let source = r#"
        // This module doesn't really exist, just verifying that we'll get
        // an error when specifier starts with "internal:".
        import { core } from "internal:core.js";
        "#;

        async move {
          Ok(ModuleSource {
            code: source.as_bytes().to_vec().into_boxed_slice(),
            module_url_specified: "file:///main.js".to_string(),
            module_url_found: "file:///main.js".to_string(),
            module_type: ModuleType::JavaScript,
          })
        }
        .boxed_local()
      }
    }

    let snapshot = {
      let runtime = JsRuntime::new(RuntimeOptions {
        will_snapshot: true,
        ..Default::default()
      });
      let snap: &[u8] = &runtime.snapshot();
      Vec::from(snap).into_boxed_slice()
    };

    let mut runtime2 = JsRuntime::new(RuntimeOptions {
      module_loader: Some(Rc::new(ModsLoader)),
      startup_snapshot: Some(Snapshot::Boxed(snapshot)),
      ..Default::default()
    });

    let err = runtime2
      .load_main_module(&crate::resolve_url("file:///main.js").unwrap(), None)
      .await
      .unwrap_err();
    assert_eq!(
      err.to_string(),
      "Cannot load internal module from external code"
    );
  }
}
*/
