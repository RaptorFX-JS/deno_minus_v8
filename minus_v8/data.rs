use crate::serde::{
  from_backend, from_v8, to_backend, ErasedDeserializer, ErasedSerialize,
};
use crate::{Handle, HandleScope, Local};
use anyhow::{anyhow, Error, Result};
use serde::{Deserialize, Serialize};
use std::prelude::v1::String as StdString;

#[derive(Clone)]
pub enum Value {
  Object,
  String {
    inner: StdString,
  },
  Boolean {
    inner: bool,
  },
  Integer {
    inner: f64,
  },
  Function {
    id: i32,
  },
  Promise {
    id: i32,
  },
  Exception {
    class: StdString,
    message: StdString,
  },
  Null,
  Undefined,
}

impl Value {
  pub fn is_undefined(&self) -> bool {
    matches!(self, Value::Undefined)
  }

  pub fn is_null_or_undefined(&self) -> bool {
    matches!(self, Value::Null | Value::Undefined)
  }

  pub fn is_true(&self) -> bool {
    // note: per v8 docs this is `=== true` rather than a truthy check
    matches!(self, Value::Boolean { inner } if inner == &true)
  }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Object {}

#[derive(Serialize, Deserialize)]
#[serde(transparent)]
#[derive(Clone)]
pub struct String(pub StdString);

impl String {
  /// For compatibility we impose V8's limit on the length of a string.
  /// See the comment in [`crate::ops_builtin_v8::op_decode`] for details.
  // https://github.com/v8/v8/blob/d68fb4733e39525f9ff0a9222107c02c28096e2a/include/v8.h#L3030
  fn check_len(buffer: &[u8]) -> bool {
    const MAX_LENGTH: usize = if std::mem::size_of::<usize>() == 4 {
      (1 << 28) - 16
    } else {
      (1 << 29) - 24
    };

    buffer.len() <= MAX_LENGTH
  }

  pub fn new<'s>(
    scope: &mut HandleScope<'s, ()>,
    value: &str,
  ) -> Option<Local<'s, String>> {
    Self::new_from_utf8(scope, value.as_ref())
      .map(|x| unsafe { Local::from_raw(x).unwrap() })
  }

  // WARNING(minus_v8) doesn't match Deno's API
  pub fn new_from_utf8<'s>(
    _scope: &mut HandleScope<'s, ()>,
    buffer: &[u8],
  ) -> Option<Self> {
    if Self::check_len(&buffer) {
      Some(Self(StdString::from_utf8(buffer.to_vec()).ok()?))
    } else {
      None
    }
  }
}

#[derive(Serialize, Deserialize)]
#[serde(transparent)]
#[derive(Clone)]
pub struct Boolean(pub bool);

#[derive(Serialize, Deserialize)]
#[serde(transparent)]
#[derive(Clone)]
pub struct Integer(pub f64);

impl Integer {
  pub fn new(_scope: &mut HandleScope<()>, inner: i32) -> Self {
    Self(inner as f64)
  }

  pub fn new_from_unsigned(_scope: &mut HandleScope<()>, inner: u32) -> Self {
    Self(inner as f64)
  }
}

#[derive(Serialize, Deserialize)]
#[serde(transparent)]
#[derive(Clone)]
pub struct Function(pub i32);

impl Function {
  pub fn call<'s>(
    &self,
    scope: &mut HandleScope<'s>,
    recv: Local<Value>,
    args: &[Local<Value>],
  ) -> Option<Local<'s, Value>> {
    if !matches!(recv.open(scope), Value::Undefined) {
      todo!("minus_v8 currently only supports calling functions with an undefined receiver");
    }
    let serialized_args = args
      .iter()
      .map(|x| {
        let val: crate::serde::Value = from_v8(scope, x.clone()).unwrap();
        to_backend(scope, val).unwrap()
      })
      .collect::<Vec<_>>();
    let mut serialized_result =
      self.call_with_serialized(scope, recv, serialized_args);
    match serialized_result {
      Some(ref mut x) => {
        let serde_result: crate::serde::Value<'s> =
          from_backend(scope, x).ok()?;
        Some(unsafe {
          Local::from_raw(serde_result.try_into().ok().clone()?).unwrap()
        })
      }
      None => None,
    }
  }

  /// minus_v8 specific function
  pub fn call_with_serialized<'s>(
    &self,
    scope: &mut HandleScope<'s>,
    _recv: Local<Value>,
    args: Vec<Box<dyn ErasedSerialize + 's>>,
  ) -> Option<Box<dyn ErasedDeserializer<'static>>> {
    futures::executor::block_on((scope.backend.invoke_function())(scope, &self, args))
  }
}

#[derive(Serialize, Deserialize)]
#[serde(transparent)]
#[derive(Clone)]
pub struct Promise(pub i32);

#[derive(Clone)]
pub struct Exception {
  pub class: StdString,
  pub message: StdString,
}

impl Exception {
  pub fn error<'s>(
    scope: &mut HandleScope<'s>,
    message: Local<String>,
  ) -> Local<'s, Value> {
    unsafe {
      Local::from_raw(Value::Exception {
        class: "Error".to_string(),
        message: message.open(scope).0.clone(),
      })
        .unwrap()
    }
  }

  pub fn type_error<'s>(
    scope: &mut HandleScope<'s>,
    message: Local<String>,
  ) -> Local<'s, Value> {
    unsafe {
      Local::from_raw(Value::Exception {
        class: "TypeError".to_string(),
        message: message.open(scope).0.clone(),
      })
      .unwrap()
    }
  }
}

macro_rules! impl_try_froms {
  ($(
    $(#[$doc:meta])*
    Value::$variant:ident $({ $($param:ident),*$(,)? })?
    $(| Value::$other:ident $({ $($other_param:ident as $other_name:ident),*$(,)? })?)*
    => $ctor:expr
  ),*$(,)?) => {
    $(
      impl TryFrom<Value> for $variant {
        type Error = Error;

        fn try_from(value: Value) -> Result<Self> {
          match value {
            Value::$variant $({ $($param),* })? => {
              Ok($ctor)
            },
            $(
              Value::$other $({ $($other_param),* })? => {
                $($(let $other_name = $other_param;)*)?
                Ok($ctor)
              },
            )*
            _ => Err(anyhow!("cannot convert Value type")),
          }
        }
      }
    )*
  }
}

impl_try_froms! {
  // one-to-one conversions
  Value::Object => Object {},
  Value::String { inner } => String(inner),
  Value::Boolean { inner } => Boolean(inner),
  Value::Integer { inner } => Integer(inner),
  Value::Exception { class, message } => Exception { class, message },

  // since the serde::Value deserializer never produces functions or promises
  // we handle their coercions as well
  Value::Function { id } | Value::Integer { inner as id } => Function(id as i32),
  Value::Promise { id } | Value::Integer { inner as id } => Promise(id as i32),
}

macro_rules! impl_reverse_froms {
  ($(
    $variant:ident
    $({ $($named_param:ident),*$(,)? })?
    $(( $($tuple_param:ident),*$(,)? ))?
    => $ctor:expr
  ),*$(,)?) => {
    $(
      impl From<$variant> for Value {
        #[allow(unused_variables)]
        fn from(value: $variant) -> Self {
          #[allow(unused_parens)]
          $(let $variant { $($named_param),* } = value;)?
          $(let $variant ( $($tuple_param),* ) = value;)?
          $ctor
        }
      }

      impl<'s> Local<'s, $variant> {
        #[allow(unused)]
        #[allow(unused_variables)]
        pub fn into_value(&self) -> Local<'s, Value> {
          #[allow(unused_parens)]
          $(let $variant { $($named_param),* } = (*self.data).clone();)?
          $(let $variant ( $($tuple_param),* ) = (*self.data).clone();)?
          unsafe { Local::from_raw($ctor).unwrap() }
        }
      }
    )*
  }
}

impl_reverse_froms! {
  Object => Value::Object,
  String(inner) => Value::String { inner },
  Boolean(inner) => Value::Boolean { inner },
  Integer(inner) => Value::Integer { inner },
  Function(id) => Value::Function { id },
  Promise(id) => Value::Promise { id },
  Exception { class, message } => Value::Exception { class, message },
}

pub fn undefined<'s>(_scope: &mut HandleScope<'s, ()>) -> Local<'s, Value> {
  unsafe { Local::from_raw(Value::Undefined).unwrap() }
}
