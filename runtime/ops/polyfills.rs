use deno_core::{Extension, include_js_files};

pub fn init() -> Extension {
  Extension::builder("deno_minus_v8:polyfills")
    .esm(include_js_files!(
      "../../ext/web/00_infra.js",
      "../../ext/web/01_mimesniff.js",
      // TODO(minus_v8) support console
      "../../ext/console/01_colors.js",
      "../js/00_polyfills.js",
    ))
    .ops(vec![
      // yoinked from deno_url for implementing parseUrlEncoded
      deno_url::op_url_parse_search_params::decl(),
    ])
    .build()
}
