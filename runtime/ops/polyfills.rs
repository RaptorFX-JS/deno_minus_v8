use deno_core::{Extension, include_js_files};

pub fn init() -> Vec<Extension> {
  vec![
    Extension::builder("deno_console")
      .esm(include_js_files!("../../ext/console/01_colors.js"))
      .build(),

    Extension::builder("deno_url")
      .dependencies(vec!["deno_webidl"])
      .esm(include_js_files!("../js/polyfills/url_00_url.js"))
      .ops(vec![
        deno_url::op_url_parse_search_params::decl(),
      ])
      .build(),

    Extension::builder("deno_web")
      .dependencies(vec!["deno_webidl", "deno_console", "deno_url"])
      .esm(include_js_files!(
        "../../ext/web/00_infra.js",
        "../js/polyfills/web_01_dom_exception.js",
        "../../ext/web/01_mimesniff.js",
        "../js/polyfills/web_02_event.js",
        "../js/polyfills/web_03_abort_signal.js",
        "../js/polyfills/web_06_streams.js",
        "../js/polyfills/web_09_file.js",
        "../js/polyfills/web_12_location.js",
      ))
      .ops(vec![
        deno_web::op_base64_decode::decl(),
        deno_web::op_base64_encode::decl(),
      ])
      .build(),
  ]
}
