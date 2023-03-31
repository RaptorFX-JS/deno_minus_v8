// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;
use std::fmt::Debug;
use std::fmt::Display;
use std::fmt::Formatter;

use anyhow::Error;
use v8::Handle;

use crate::runtime::GetErrorClassFn;
use crate::runtime::JsRealm;
use crate::url::Url;

/// A generic wrapper that can encapsulate any concrete error type.
// TODO(ry) Deprecate AnyError and encourage deno_core::anyhow::Error instead.
pub type AnyError = anyhow::Error;

/// Creates a new error with a caller-specified error class name and message.
pub fn custom_error(
  class: &'static str,
  message: impl Into<Cow<'static, str>>,
) -> Error {
  CustomError {
    class,
    message: message.into(),
  }
  .into()
}

pub fn generic_error(message: impl Into<Cow<'static, str>>) -> Error {
  custom_error("Error", message)
}

pub fn type_error(message: impl Into<Cow<'static, str>>) -> Error {
  custom_error("TypeError", message)
}

pub fn range_error(message: impl Into<Cow<'static, str>>) -> Error {
  custom_error("RangeError", message)
}

pub fn invalid_hostname(hostname: &str) -> Error {
  type_error(format!("Invalid hostname: '{hostname}'"))
}

pub fn uri_error(message: impl Into<Cow<'static, str>>) -> Error {
  custom_error("URIError", message)
}

pub fn bad_resource(message: impl Into<Cow<'static, str>>) -> Error {
  custom_error("BadResource", message)
}

pub fn bad_resource_id() -> Error {
  custom_error("BadResource", "Bad resource ID")
}

pub fn not_supported() -> Error {
  custom_error("NotSupported", "The operation is not supported")
}

pub fn resource_unavailable() -> Error {
  custom_error(
    "Busy",
    "Resource is unavailable because it is in use by a promise",
  )
}

/// A simple error type that lets the creator specify both the error message and
/// the error class name. This type is private; externally it only ever appears
/// wrapped in an `anyhow::Error`. To retrieve the error class name from a wrapped
/// `CustomError`, use the function `get_custom_error_class()`.
#[derive(Debug)]
struct CustomError {
  class: &'static str,
  message: Cow<'static, str>,
}

impl Display for CustomError {
  fn fmt(&self, f: &mut Formatter) -> fmt::Result {
    f.write_str(&self.message)
  }
}

impl std::error::Error for CustomError {}

/// If this error was crated with `custom_error()`, return the specified error
/// class name. In all other cases this function returns `None`.
pub fn get_custom_error_class(error: &Error) -> Option<&'static str> {
  error.downcast_ref::<CustomError>().map(|e| e.class)
}

pub fn to_v8_error<'a>(
  scope: &mut v8::HandleScope<'a>,
  get_class: GetErrorClassFn,
  error: &Error,
) -> v8::Local<'a, v8::Value> {
  let tc_scope = &mut v8::TryCatch::new(scope);
  let cb = JsRealm::state_from_scope(tc_scope)
    .borrow()
    .js_build_custom_error_cb
    .clone()
    .expect("Custom error builder must be set");
  let cb = cb.open(tc_scope);
  let this = v8::undefined(tc_scope).into();
  let class = v8::String::new(tc_scope, get_class(error)).unwrap();
  let message = v8::String::new(tc_scope, &format!("{error:#}")).unwrap();
  let mut args = vec![class.into_value(), message.into_value()];
  if let Some(code) = crate::error_codes::get_error_code(error) {
    args.push(v8::String::new(tc_scope, code).unwrap().into_value());
  }
  let maybe_exception = cb.call(tc_scope, this, &args);

  match maybe_exception {
    Some(exception) => exception,
    None => {
      let mut msg =
        "Custom error class must have a builder registered".to_string();
      if tc_scope.has_caught() {
        let e = tc_scope.exception().unwrap();
        let js_error = JsError::from_v8_exception(tc_scope, e);
        msg = format!("{}: {}", msg, js_error.exception_message);
      }
      panic!("{}", msg);
    }
  }
}

/// A `JsError` represents an exception coming from V8, with stack frames and
/// line numbers. The deno_cli crate defines another `JsError` type, which wraps
/// the one defined here, that adds source map support and colorful formatting.
/// When updating this struct, also update errors_are_equal_without_cause() in
/// fmt_error.rs.
#[derive(Debug, PartialEq, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JsError {
  pub name: Option<String>,
  pub message: Option<String>,
  pub stack: Option<String>,
  pub cause: Option<Box<JsError>>,
  pub exception_message: String,
  pub frames: Vec<JsStackFrame>,
  pub source_line: Option<String>,
  pub source_line_frame_index: Option<usize>,
  pub aggregated: Option<Vec<JsError>>,
}

#[derive(Debug, Eq, PartialEq, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JsStackFrame {
  pub type_name: Option<String>,
  pub function_name: Option<String>,
  pub method_name: Option<String>,
  pub file_name: Option<String>,
  pub line_number: Option<i64>,
  pub column_number: Option<i64>,
  pub eval_origin: Option<String>,
  // Warning! isToplevel has inconsistent snake<>camel case, "typo" originates in v8:
  // https://source.chromium.org/search?q=isToplevel&sq=&ss=chromium%2Fchromium%2Fsrc:v8%2F
  #[serde(rename = "isToplevel")]
  pub is_top_level: Option<bool>,
  pub is_eval: bool,
  pub is_native: bool,
  pub is_constructor: bool,
  pub is_async: bool,
  pub is_promise_all: bool,
  pub promise_index: Option<i64>,
}

impl JsStackFrame {
  pub fn from_location(
    file_name: Option<String>,
    line_number: Option<i64>,
    column_number: Option<i64>,
  ) -> Self {
    Self {
      type_name: None,
      function_name: None,
      method_name: None,
      file_name,
      line_number,
      column_number,
      eval_origin: None,
      is_top_level: None,
      is_eval: false,
      is_native: false,
      is_constructor: false,
      is_async: false,
      is_promise_all: false,
      promise_index: None,
    }
  }
}

impl JsError {
  pub fn from_v8_exception(
    scope: &mut v8::HandleScope,
    exception: v8::Local<v8::Value>,
  ) -> Self {
    Self::inner_from_v8_exception(scope, exception, Default::default())
  }

  /*
  pub fn from_v8_message<'a>(
    scope: &'a mut v8::HandleScope,
    msg: v8::Local<'a, v8::Message>,
  ) -> Self {
    // Create a new HandleScope because we're creating a lot of new local
    // handles below.
    let scope = &mut v8::HandleScope::new(scope);

    let exception_message = msg.get(scope).to_rust_string_lossy(scope);
    let state_rc = JsRuntime::state(scope);

    // Convert them into Vec<JsStackFrame>
    let mut frames: Vec<JsStackFrame> = vec![];

    let mut source_line = None;
    let mut source_line_frame_index = None;
    {
      let state = &mut *state_rc.borrow_mut();

      let script_resource_name = msg
        .get_script_resource_name(scope)
        .and_then(|v| v8::Local::<v8::String>::try_from(v).ok())
        .map(|v| v.to_rust_string_lossy(scope));
      let line_number: Option<i64> =
        msg.get_line_number(scope).and_then(|v| v.try_into().ok());
      let column_number: Option<i64> = msg.get_start_column().try_into().ok();
      if let (Some(f), Some(l), Some(c)) =
        (script_resource_name, line_number, column_number)
      {
        // V8's column numbers are 0-based, we want 1-based.
        let c = c + 1;
        if let Some(source_map_getter) = &state.source_map_getter {
          let (f, l, c) = apply_source_map(
            f,
            l,
            c,
            &mut state.source_map_cache,
            source_map_getter.as_ref(),
          );
          frames = vec![JsStackFrame::from_location(Some(f), Some(l), Some(c))];
        } else {
          frames = vec![JsStackFrame::from_location(Some(f), Some(l), Some(c))];
        }
      }

      if let Some(source_map_getter) = &state.source_map_getter {
        for (i, frame) in frames.iter().enumerate() {
          if let (Some(file_name), Some(line_number)) =
            (&frame.file_name, frame.line_number)
          {
            if !file_name.trim_start_matches('[').starts_with("internal:") {
              source_line = get_source_line(
                file_name,
                line_number,
                &mut state.source_map_cache,
                source_map_getter.as_ref(),
              );
              source_line_frame_index = Some(i);
              break;
            }
          }
        }
      }
    }

    Self {
      name: None,
      message: None,
      exception_message,
      cause: None,
      source_line,
      source_line_frame_index,
      frames,
      stack: None,
      aggregated: None,
    }
  }
  */

  fn inner_from_v8_exception<'a>(
    scope: &'a mut v8::HandleScope,
    exception: v8::Local<'a, v8::Value>,
    mut seen: HashSet<v8::Local<'a, v8::Object>>,
  ) -> Self {
    // TODO(minus_v8) research to what extent can we populate JsError
    Self {
      name: None,
      message: None,
      stack: None,
      cause: None,
      exception_message: "".into(),
      frames: vec![],
      source_line: None,
      source_line_frame_index: None,
      aggregated: None,
    }
  }
}

impl std::error::Error for JsError {}

fn format_source_loc(
  file_name: &str,
  line_number: i64,
  column_number: i64,
) -> String {
  let line_number = line_number;
  let column_number = column_number;
  format!("{file_name}:{line_number}:{column_number}")
}

impl Display for JsError {
  fn fmt(&self, f: &mut Formatter) -> fmt::Result {
    if let Some(stack) = &self.stack {
      let stack_lines = stack.lines();
      if stack_lines.count() > 1 {
        return write!(f, "{stack}");
      }
    }
    write!(f, "{}", self.exception_message)?;
    let frame = self.frames.first();
    if let Some(frame) = frame {
      if let (Some(f_), Some(l), Some(c)) =
        (&frame.file_name, frame.line_number, frame.column_number)
      {
        let source_loc = format_source_loc(f_, l, c);
        write!(f, "\n    at {source_loc}")?;
      }
    }
    Ok(())
  }
}

// TODO(piscisaureus): rusty_v8 should implement the Error trait on
// values of type v8::Global<T>.
pub(crate) fn to_v8_type_error(
  scope: &mut v8::HandleScope,
  err: Error,
) -> v8::Global<v8::Value> {
  let err_string = err.to_string();
  let error_chain = err
    .chain()
    .skip(1)
    .filter(|e| e.to_string() != err_string)
    .map(|e| e.to_string())
    .collect::<Vec<_>>();

  let message = if !error_chain.is_empty() {
    format!(
      "{}\n  Caused by:\n    {}",
      err_string,
      error_chain.join("\n    ")
    )
  } else {
    err_string
  };

  let message = v8::String::new(scope, &message).unwrap();
  let exception = v8::Exception::type_error(scope, message);
  v8::Global::new(scope, exception)
}

const DATA_URL_ABBREV_THRESHOLD: usize = 150;

pub fn format_file_name(file_name: &str) -> String {
  abbrev_file_name(file_name).unwrap_or_else(|| file_name.to_string())
}

fn abbrev_file_name(file_name: &str) -> Option<String> {
  if file_name.len() <= DATA_URL_ABBREV_THRESHOLD {
    return None;
  }
  let url = Url::parse(file_name).ok()?;
  if url.scheme() != "data" {
    return None;
  }
  let (head, tail) = url.path().split_once(',')?;
  let len = tail.len();
  let start = tail.get(0..20)?;
  let end = tail.get(len - 20..)?;
  Some(format!("{}:{},{}......{}", url.scheme(), head, start, end))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_bad_resource() {
    let err = bad_resource("Resource has been closed");
    assert_eq!(err.to_string(), "Resource has been closed");
  }

  #[test]
  fn test_bad_resource_id() {
    let err = bad_resource_id();
    assert_eq!(err.to_string(), "Bad resource ID");
  }
}
