// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

use deno_core::include_js_files_dir;
use std::env;

// This is a shim that allows to generate documentation on docs.rs
pub mod not_docs {
  use std::path::Path;

  use super::*;
  use deno_core::Extension;

  /*
  use deno_ast::MediaType;
  use deno_ast::ParseParams;
  use deno_ast::SourceTextInfo;
  */
  use deno_core::error::AnyError;
  use deno_core::ExtensionFileSource;

  /*
  fn transpile_ts_for_snapshotting(
    file_source: &ExtensionFileSource,
  ) -> Result<String, AnyError> {
    let media_type = MediaType::from(Path::new(&file_source.specifier));

    let should_transpile = match media_type {
      MediaType::JavaScript => false,
      MediaType::Mjs => false,
      MediaType::TypeScript => true,
      _ => panic!(
        "Unsupported media type for snapshotting {media_type:?} for file {}",
        file_source.specifier
      ),
    };

    if !should_transpile {
      return Ok(file_source.code.to_string());
    }

    let parsed = deno_ast::parse_module(ParseParams {
      specifier: file_source.specifier.to_string(),
      text_info: SourceTextInfo::from_string(file_source.code.to_string()),
      media_type,
      capture_tokens: false,
      scope_analysis: false,
      maybe_syntax: None,
    })?;
    let transpiled_source = parsed.transpile(&deno_ast::EmitOptions {
      imports_not_used_as_values: deno_ast::ImportsNotUsedAsValues::Remove,
      inline_source_map: false,
      ..Default::default()
    })?;

    Ok(transpiled_source.text)
  }
  */

  struct Permissions;

  impl deno_fetch::FetchPermissions for Permissions {
    fn check_net_url(
      &mut self,
      _url: &deno_core::url::Url,
      _api_name: &str,
    ) -> Result<(), deno_core::error::AnyError> {
      unreachable!("snapshotting!")
    }

    fn check_read(
      &mut self,
      _p: &Path,
      _api_name: &str,
    ) -> Result<(), deno_core::error::AnyError> {
      unreachable!("snapshotting!")
    }
  }

  impl deno_websocket::WebSocketPermissions for Permissions {
    fn check_net_url(
      &mut self,
      _url: &deno_core::url::Url,
      _api_name: &str,
    ) -> Result<(), deno_core::error::AnyError> {
      unreachable!("snapshotting!")
    }
  }

  /*
  impl deno_web::TimersPermission for Permissions {
    fn allow_hrtime(&mut self) -> bool {
      unreachable!("snapshotting!")
    }

    fn check_unstable(
      &self,
      _state: &deno_core::OpState,
      _api_name: &'static str,
    ) {
      unreachable!("snapshotting!")
    }
  }

  impl deno_ffi::FfiPermissions for Permissions {
    fn check(
      &mut self,
      _path: Option<&Path>,
    ) -> Result<(), deno_core::error::AnyError> {
      unreachable!("snapshotting!")
    }
  }

  impl deno_napi::NapiPermissions for Permissions {
    fn check(
      &mut self,
      _path: Option<&Path>,
    ) -> Result<(), deno_core::error::AnyError> {
      unreachable!("snapshotting!")
    }
  }

  impl deno_flash::FlashPermissions for Permissions {
    fn check_net<T: AsRef<str>>(
      &mut self,
      _host: &(T, Option<u16>),
      _api_name: &str,
    ) -> Result<(), deno_core::error::AnyError> {
      unreachable!("snapshotting!")
    }
  }

  impl deno_node::NodePermissions for Permissions {
    fn check_read(
      &mut self,
      _p: &Path,
    ) -> Result<(), deno_core::error::AnyError> {
      unreachable!("snapshotting!")
    }
  }
  */

  impl deno_net::NetPermissions for Permissions {
    fn check_net<T: AsRef<str>>(
      &mut self,
      _host: &(T, Option<u16>),
      _api_name: &str,
    ) -> Result<(), deno_core::error::AnyError> {
      unreachable!("snapshotting!")
    }

    fn check_read(
      &mut self,
      _p: &Path,
      _api_name: &str,
    ) -> Result<(), deno_core::error::AnyError> {
      unreachable!("snapshotting!")
    }

    fn check_write(
      &mut self,
      _p: &Path,
      _api_name: &str,
    ) -> Result<(), deno_core::error::AnyError> {
      unreachable!("snapshotting!")
    }
  }

  #[allow(unreachable_code)]
  fn create_runtime_snapshot(
    maybe_additional_extension: Option<Extension>,
  ) -> Vec<Extension> {
    let runtime_extension = Extension::builder("runtime")
      .dependencies(vec![
        "deno_webidl",
        "deno_console",
        "deno_url",
        "deno_tls",
        "deno_web",
        "deno_fetch",
        "deno_cache",
        "deno_websocket",
        "deno_webstorage",
        "deno_crypto",
        "deno_webgpu",
        "deno_broadcast_channel",
        // FIXME(bartlomieju): this should be reenabled
        // "deno_node",
        "deno_ffi",
        "deno_net",
        "deno_napi",
        "deno_http",
        "deno_flash",
      ])
      .esm(include_js_files_dir!(
        dir "js",
        "01_build.js",
        "01_errors.js",
        // "01_version.ts",
        "polyfills/runtime_01_version.js" as "01_version.ts",
        "06_util.js",
        "10_permissions.js",
        // minus_v8: we don't support workers
        // "11_workers.js",
        "12_io.js",
        "13_buffer.js",
        "30_fs.js",
        "30_os.js",
        "40_diagnostics.js",
        "40_files.js",
        "40_fs_events.js",
        "40_http.js",
        "40_process.js",
        "40_read_file.js",
        "40_signals.js",
        "40_spawn.js",
        "40_tty.js",
        "40_write_file.js",
        "41_prompt.js",
        "90_deno_ns.js",
        "98_global_scope.js",
      ))
      .build();

    /*
    let mut extensions_with_js: Vec<Extension> = vec![
      deno_webidl::init(),
      deno_console::init(),
      deno_url::init(),
      deno_tls::init(),
      deno_web::init::<Permissions>(
        deno_web::BlobStore::default(),
        Default::default(),
      ),
      deno_fetch::init::<Permissions>(Default::default()),
      deno_cache::init::<SqliteBackedCache>(None),
      deno_websocket::init::<Permissions>("".to_owned(), None, None),
      deno_webstorage::init(None),
      deno_crypto::init(None),
      deno_webgpu::init(false),
      deno_broadcast_channel::init(
        deno_broadcast_channel::InMemoryBroadcastChannel::default(),
        false, // No --unstable.
      ),
      deno_ffi::init::<Permissions>(false),
      deno_net::init::<Permissions>(
        None, false, // No --unstable.
        None,
      ),
      deno_napi::init::<Permissions>(),
      deno_http::init(),
      deno_flash::init::<Permissions>(false), // No --unstable
      runtime_extension,
      // FIXME(bartlomieju): these extensions are specified last, because they
      // depend on `runtime`, even though it should be other way around
      deno_node::init::<Permissions>(None),
      deno_node::init_polyfill(),
    ];
    */

    let mut extensions_with_js: Vec<Extension> = vec![runtime_extension];

    if let Some(additional_extension) = maybe_additional_extension {
      extensions_with_js.push(additional_extension);
    }

    extensions_with_js
  }

  pub fn build_snapshot() -> Vec<Extension> {
    #[allow(unused_mut, unused_assignments)]
    let mut maybe_additional_extension = None;

    // #[cfg(not(feature = "snapshot_from_snapshot"))]
    {
      maybe_additional_extension = Some(
        Extension::builder("runtime_main")
          .dependencies(vec!["runtime"])
          .esm(vec![ExtensionFileSource {
            specifier: "js/99_main.js".to_string(),
            code: include_str!("js/99_main.js"),
          }])
          .build(),
      );
    }

    create_runtime_snapshot(maybe_additional_extension)
  }
}

fn main() {
  // To debug snapshot issues uncomment:
  // op_fetch_asset::trace_serializer();

  println!("cargo:rustc-env=TARGET={}", env::var("TARGET").unwrap());
  println!("cargo:rustc-env=PROFILE={}", env::var("PROFILE").unwrap());

  // minus_v8: since we don't have the luxury of snapshots nothing needs to be done in build.rs
  // for easier merging with upstream the list of extensions is include!'d from this file in
  // runtime/js.rs
}
