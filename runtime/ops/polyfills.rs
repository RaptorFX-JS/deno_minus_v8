use deno_core::{include_js_files, Extension};

pub fn init() -> Vec<Extension> {
  vec![
    Extension::builder("deno_console")
      .esm(
        include_js_files!(
            "../../ext/console/01_colors.js" as "01_colors.js",
            "../js/polyfills/console_02_console.js" as "02_console.js",
        ),
      )
      .build(),
    Extension::builder("deno_url")
      .dependencies(vec!["deno_webidl"])
      .esm(include_js_files!("../js/polyfills/url_00_url.js" as "00_url.js",))
      .ops(vec![deno_url::op_url_parse_search_params::decl()])
      .build(),
    Extension::builder("deno_web")
      .dependencies(vec!["deno_webidl", "deno_console", "deno_url"])
      .esm(include_js_files!(
        "../../ext/web/00_infra.js" as "00_infra.js",
        "../js/polyfills/web_01_dom_exception.js" as "01_dom_exception.js",
        "../../ext/web/01_mimesniff.js" as "01_mimesniff.js",
        "../js/polyfills/web_02_event.js" as "02_event.js",
        "../js/polyfills/web_03_abort_signal.js" as "03_abort_signal.js",
        "../js/polyfills/web_06_streams.js" as "06_streams.js",
        "../js/polyfills/web_09_file.js" as "09_file.js",
        "../js/polyfills/web_12_location.js" as "12_location.js",
      ))
      .ops(vec![
        deno_web::op_base64_decode::decl(),
        deno_web::op_base64_encode::decl(),
      ])
      .build(),
  ]
}
