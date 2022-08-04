// Copyright 2018-2022 the Deno authors. All rights reserved. MIT license.

use crate::js;
use crate::ops;
use crate::ops::io::Stdio;
use crate::permissions::Permissions;
use crate::BootstrapOptions;
use deno_core::error::AnyError;
use deno_core::error::JsError;
use deno_core::futures::Future;
use deno_core::located_script_name;
use deno_core::Extension;
use deno_core::GetErrorClassFn;
use deno_core::JsRuntime;
use deno_core::ModuleId;
use deno_core::ModuleSpecifier;
use deno_core::RuntimeOptions;
use deno_tls::rustls::RootCertStore;
use log::debug;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use deno_core::v8::backend::JsBackend;

pub type FormatJsErrorFn = dyn Fn(&JsError) -> String + Sync + Send;

#[derive(Clone)]
pub struct ExitCode(Arc<AtomicI32>);

impl ExitCode {
  pub fn get(&self) -> i32 {
    self.0.load(Relaxed)
  }

  pub fn set(&mut self, code: i32) {
    self.0.store(code, Relaxed);
  }
}
/// This worker is created and used by almost all
/// subcommands in Deno executable.
///
/// It provides ops available in the `Deno` namespace.
///
/// All `WebWorker`s created during program execution
/// are descendants of this worker.
pub struct MainWorker {
  pub js_runtime: JsRuntime,
  should_break_on_first_statement: bool,
  exit_code: ExitCode,
}

pub struct WorkerOptions {
  pub backend: Box<dyn JsBackend>,
  pub bootstrap: BootstrapOptions,
  pub extensions: Vec<Extension>,
  pub unsafely_ignore_certificate_errors: Option<Vec<String>>,
  pub root_cert_store: Option<RootCertStore>,
  pub seed: Option<u64>,
  // Callbacks invoked when creating new instance of WebWorker
  pub format_js_error_fn: Option<Arc<FormatJsErrorFn>>,
  pub should_break_on_first_statement: bool,
  pub get_error_class_fn: Option<GetErrorClassFn>,
  pub origin_storage_dir: Option<std::path::PathBuf>,
  pub stdio: Stdio,
}

impl MainWorker {
  pub fn bootstrap_from_options(
    main_module: ModuleSpecifier,
    permissions: Permissions,
    options: WorkerOptions,
  ) -> Self {
    let bootstrap_options = options.bootstrap.clone();
    let mut worker = Self::from_options(main_module, permissions, options);
    worker.bootstrap(&bootstrap_options);
    worker
  }

  pub fn from_options(
    main_module: ModuleSpecifier,
    permissions: Permissions,
    mut options: WorkerOptions,
  ) -> Self {
    // Permissions: many ops depend on this
    let unstable = options.bootstrap.unstable;
    let enable_testing_features = options.bootstrap.enable_testing_features;
    let perm_ext = Extension::builder()
      .state(move |state| {
        state.put::<Permissions>(permissions.clone());
        state.put(ops::UnstableChecker { unstable });
        state.put(ops::TestingFeaturesEnabled(enable_testing_features));
        Ok(())
      })
      .build();
    let exit_code = ExitCode(Arc::new(AtomicI32::new(0)));

    // Internal modules
    let mut extensions: Vec<Extension> = vec![
      // minus_v8 polyfills
      ops::polyfills::init(),
      // Web APIs
      deno_webidl::init(),
      deno_fetch::init::<Permissions>(deno_fetch::Options {
        user_agent: options.bootstrap.user_agent.clone(),
        root_cert_store: options.root_cert_store.clone(),
        unsafely_ignore_certificate_errors: options
          .unsafely_ignore_certificate_errors
          .clone(),
        file_fetch_handler: Rc::new(deno_fetch::FsFetchHandler),
        ..Default::default()
      }),
      deno_websocket::init::<Permissions>(
        options.bootstrap.user_agent.clone(),
        options.root_cert_store.clone(),
        options.unsafely_ignore_certificate_errors.clone(),
      ),
      // Runtime ops
      ops::runtime::init(main_module.clone()),
      ops::spawn::init(),
      ops::fs_events::init(),
      ops::fs::init(),
      ops::io::init(),
      ops::io::init_stdio(options.stdio),
      deno_tls::init(),
      deno_net::init::<Permissions>(
        options.root_cert_store.clone(),
        unstable,
        options.unsafely_ignore_certificate_errors.clone(),
      ),
      ops::os::init(exit_code.clone()),
      ops::permissions::init(),
      ops::process::init(),
      ops::signal::init(),
      ops::tty::init(),
      deno_http::init(),
      ops::http::init(),
      // Runtime JS
      js::init(),
      // Permissions ext (worker specific state)
      perm_ext,
    ];
    extensions.extend(std::mem::take(&mut options.extensions));

    let mut js_runtime = JsRuntime::new(RuntimeOptions {
      backend: options.backend,
      get_error_class_fn: options.get_error_class_fn,
      extensions,
    });

    Self {
      js_runtime,
      should_break_on_first_statement: options.should_break_on_first_statement,
      exit_code,
    }
  }

  pub fn bootstrap(&mut self, options: &BootstrapOptions) {
    let script = format!("bootstrap.mainRuntime({})", options.as_json());
    self
      .execute_script(&located_script_name!(), &script)
      .expect("Failed to execute bootstrap script");
  }

  /// See [JsRuntime::execute_script](deno_core::JsRuntime::execute_script)
  pub fn execute_script(
    &mut self,
    script_name: &str,
    source_code: &str,
  ) -> Result<(), AnyError> {
    self.js_runtime.execute_script(script_name, source_code)?;
    Ok(())
  }

  /// Loads and instantiates specified JavaScript module
  /// as "main" or "side" module.
  pub async fn preload_module(
    &mut self,
    module_specifier: &ModuleSpecifier,
    main: bool,
  ) -> Result<ModuleId, AnyError> {
    if main {
      self
        .js_runtime
        .load_main_module(module_specifier, None)
        .await
    } else {
      self
        .js_runtime
        .load_side_module(module_specifier, None)
        .await
    }
  }

  /// Executes specified JavaScript module.
  pub async fn evaluate_module(
    &mut self,
    id: ModuleId,
  ) -> Result<(), AnyError> {
    let mut receiver = self.js_runtime.mod_evaluate(id);
    tokio::select! {
      // Not using biased mode leads to non-determinism for relatively simple
      // programs.
      biased;

      maybe_result = &mut receiver => {
        debug!("received module evaluate {:#?}", maybe_result);
        maybe_result.expect("Module evaluation result not provided.")
      }

      event_loop_result = self.run_event_loop(false) => {
        event_loop_result?;
        let maybe_result = receiver.await;
        maybe_result.expect("Module evaluation result not provided.")
      }
    }
  }

  /// Loads, instantiates and executes specified JavaScript module.
  pub async fn execute_side_module(
    &mut self,
    module_specifier: &ModuleSpecifier,
  ) -> Result<(), AnyError> {
    let id = self.preload_module(module_specifier, false).await?;
    self.wait_for_inspector_session();
    self.evaluate_module(id).await
  }

  /// Loads, instantiates and executes specified JavaScript module.
  ///
  /// This module will have "import.meta.main" equal to true.
  pub async fn execute_main_module(
    &mut self,
    module_specifier: &ModuleSpecifier,
  ) -> Result<(), AnyError> {
    let id = self.preload_module(module_specifier, true).await?;
    self.wait_for_inspector_session();
    self.evaluate_module(id).await
  }

  fn wait_for_inspector_session(&mut self) {
    // minus_v8: there is no inspector
  }

  pub fn poll_event_loop(
    &mut self,
    cx: &mut Context,
    _wait_for_inspector: bool,
  ) -> Poll<Result<(), AnyError>> {
    self.js_runtime.poll_event_loop(cx)
  }

  pub async fn run_event_loop(
    &mut self,
    wait_for_inspector: bool,
  ) -> Result<(), AnyError> {
    self.js_runtime.run_event_loop(wait_for_inspector).await
  }

  /// A utility function that runs provided future concurrently with the event loop.
  ///
  /// Useful when using a local inspector session.
  pub async fn with_event_loop<'a, T>(
    &mut self,
    mut fut: Pin<Box<dyn Future<Output = T> + 'a>>,
  ) -> T {
    loop {
      tokio::select! {
        biased;
        result = &mut fut => {
          return result;
        }
        _ = self.run_event_loop(false) => {}
      };
    }
  }

  /// Return exit code set by the executed code (either in main worker
  /// or one of child web workers).
  pub fn get_exit_code(&self) -> i32 {
    self.exit_code.get()
  }

  /// Dispatches "load" event to the JavaScript runtime.
  ///
  /// Does not poll event loop, and thus not await any of the "load" event handlers.
  pub fn dispatch_load_event(
    &mut self,
    script_name: &str,
  ) -> Result<(), AnyError> {
    self.execute_script(
      script_name,
      // NOTE(@bartlomieju): not using `globalThis` here, because user might delete
      // it. Instead we're using global `dispatchEvent` function which will
      // used a saved reference to global scope.
      "dispatchEvent(new Event('load'))",
    )
  }

  /// Dispatches "unload" event to the JavaScript runtime.
  ///
  /// Does not poll event loop, and thus not await any of the "unload" event handlers.
  pub fn dispatch_unload_event(
    &mut self,
    script_name: &str,
  ) -> Result<(), AnyError> {
    self.execute_script(
      script_name,
      // NOTE(@bartlomieju): not using `globalThis` here, because user might delete
      // it. Instead we're using global `dispatchEvent` function which will
      // used a saved reference to global scope.
      "dispatchEvent(new Event('unload'))",
    )
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use deno_core::resolve_url_or_path;

  fn create_test_worker() -> MainWorker {
    let main_module = resolve_url_or_path("./hello.js").unwrap();
    let permissions = Permissions::default();

    let options = WorkerOptions {
      backend: RuntimeOptions::default().backend,
      bootstrap: BootstrapOptions {
        args: vec![],
        cpu_count: 1,
        debug_flag: false,
        enable_testing_features: false,
        location: None,
        no_color: true,
        is_tty: false,
        runtime_version: "x".to_string(),
        ts_version: "x".to_string(),
        unstable: false,
        user_agent: "x".to_string(),
      },
      extensions: vec![],
      unsafely_ignore_certificate_errors: None,
      root_cert_store: None,
      seed: None,
      format_js_error_fn: None,
      should_break_on_first_statement: false,
      get_error_class_fn: None,
      origin_storage_dir: None,
      stdio: Default::default(),
    };

    MainWorker::bootstrap_from_options(main_module, permissions, options)
  }

  #[tokio::test]
  async fn execute_mod_esm_imports_a() {
    let p = test_util::testdata_path().join("esm_imports_a.js");
    let module_specifier = resolve_url_or_path(&p.to_string_lossy()).unwrap();
    let mut worker = create_test_worker();
    let result = worker.execute_main_module(&module_specifier).await;
    if let Err(err) = result {
      eprintln!("execute_mod err {:?}", err);
    }
    if let Err(e) = worker.run_event_loop(false).await {
      panic!("Future got unexpected error: {:?}", e);
    }
  }

  #[tokio::test]
  async fn execute_mod_circular() {
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .parent()
      .unwrap()
      .join("tests/circular1.js");
    let module_specifier = resolve_url_or_path(&p.to_string_lossy()).unwrap();
    let mut worker = create_test_worker();
    let result = worker.execute_main_module(&module_specifier).await;
    if let Err(err) = result {
      eprintln!("execute_mod err {:?}", err);
    }
    if let Err(e) = worker.run_event_loop(false).await {
      panic!("Future got unexpected error: {:?}", e);
    }
  }

  #[tokio::test]
  async fn execute_mod_resolve_error() {
    // "foo" is not a valid module specifier so this should return an error.
    let mut worker = create_test_worker();
    let module_specifier = resolve_url_or_path("does-not-exist").unwrap();
    let result = worker.execute_main_module(&module_specifier).await;
    assert!(result.is_err());
  }

  #[tokio::test]
  async fn execute_mod_002_hello() {
    // This assumes cwd is project root (an assumption made throughout the
    // tests).
    let mut worker = create_test_worker();
    let p = test_util::testdata_path().join("001_hello.js");
    let module_specifier = resolve_url_or_path(&p.to_string_lossy()).unwrap();
    let result = worker.execute_main_module(&module_specifier).await;
    assert!(result.is_ok());
  }
}
