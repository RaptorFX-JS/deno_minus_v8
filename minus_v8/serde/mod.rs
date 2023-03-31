mod wrappers;
pub use wrappers::*;

use crate::Handle;
use anyhow::Context;
use serde::{
  de::Error as DeError, ser::Error as SerError, Deserialize, Deserializer,
  Serialize, Serializer,
};
use std::fmt::Formatter;
use std::marker::PhantomData;

pub use erased_serde::Deserializer as ErasedDeserializer;
pub use erased_serde::Serialize as ErasedSerialize;
pub use erased_serde::Serializer as ErasedSerializer;

pub use anyhow::Error;
pub use anyhow::Result;

pub type SerializablePkg = Box<dyn ErasedSerialize>;

#[allow(unused)]
type JsValue<'s> = crate::Local<'s, crate::Value>;

type JsResult<'s> = Result<Box<dyn ErasedSerialize + 's>>;

pub enum Value<'bogus_lifetime_for_compat> {
  // WARNING(minus_v8) THIS DOES NOT MATCH ORIGINAL API
  // Since we get values from the backend as `ErasedDeserializer`s
  // but send them as `ErasedSerialize` this needs to be an enum
  // with two states, similarly to how the real `serde_v8`
  // implemented `ZeroCopyBuf`.
  //
  // Note: Migrate all usages of `v8_value` to a `try_into()` call!
  // pub v8_value: JsValue<'s>,
  FromBackend(crate::Value, PhantomData<&'bogus_lifetime_for_compat ()>),
  ToBackend(Box<dyn ErasedSerialize>),
}

impl<'s> Serialize for Value<'s> {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    match self {
      Value::FromBackend(..) => Err(S::Error::custom(
        "serializing a Value::FromBackend is unsupported",
      )),
      Value::ToBackend(data) => erased_serde::serialize(data, serializer),
    }
  }
}

impl<'de: 's, 's> Deserialize<'de> for Value<'s> {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    // TODO(minus_v8) is it possible for us to support non-self describing serializers?

    #[derive(Default)]
    struct ValueVisitor<'s> {
      phantom: PhantomData<&'s ()>,
    }

    macro_rules! visit_integers {
      ($($method:ident => $ty:ty),*$(,)?) => {
        $(
          fn $method<E: DeError>(self, v: $ty) -> Result<Self::Value, E> {
            Ok(Value::FromBackend(crate::Value::Integer { inner: v as f64 }, PhantomData))
          }
        )*
      }
    }

    impl<'v: 's, 's> serde::de::Visitor<'v> for ValueVisitor<'s> {
      type Value = Value<'s>;

      fn expecting(&self, formatter: &mut Formatter) -> std::fmt::Result {
        write!(
          formatter,
          "expecting a boolean, integer, string, or exception"
        )
      }

      fn visit_bool<E: DeError>(self, v: bool) -> Result<Self::Value, E> {
        Ok(Value::FromBackend(
          crate::Value::Boolean { inner: v },
          PhantomData,
        ))
      }

      visit_integers![
        visit_i8 => i8,
        visit_i16 => i16,
        visit_i32 => i32,
        visit_i64 => i64,
        visit_i128 => i128,
        visit_u8 => u8,
        visit_u16 => u16,
        visit_u32 => u32,
        visit_u64 => u64,
        visit_u128 => u128,
        visit_f32 => f32,
        visit_f64 => f64,
      ];

      fn visit_str<E: DeError>(self, v: &str) -> Result<Self::Value, E> {
        Ok(Value::FromBackend(
          crate::Value::String {
            inner: v.to_string(),
          },
          PhantomData,
        ))
      }

      fn visit_none<E: DeError>(self) -> Result<Self::Value, E> {
        Ok(Value::FromBackend(crate::Value::Null, PhantomData))
      }

      fn visit_map<M: serde::de::MapAccess<'v>>(
        self,
        mut v: M,
      ) -> Result<Self::Value, M::Error> {
        let mut class = None;
        let mut message = None;
        loop {
          match v.next_key::<&'v str>()? {
            Some(key) => match key {
              "class" => class = v.next_value()?,
              "message" => message = v.next_value()?,
              _ => continue,
            },
            None => break,
          }
        }
        if let (Some(class), Some(message)) = (class, message) {
          Ok(Value::FromBackend(
            crate::Value::Exception { class, message },
            PhantomData,
          ))
        } else {
          Err(M::Error::custom("expected Exception { class, message }"))
        }
      }
    }

    deserializer.deserialize_any(ValueVisitor::default())
  }
}

impl<'s, T> TryFrom<Value<'s>> for crate::Local<'s, T>
where
  T: TryFrom<crate::Value, Error = Error>,
{
  type Error = Error;

  fn try_from(value: Value) -> Result<Self, Self::Error> {
    match value {
      Value::FromBackend(data, ..) => unsafe {
        Ok(crate::Local::from_raw(data.try_into()?).unwrap())
      },
      Value::ToBackend(_) => Err(anyhow::anyhow!(
        "converting a Value::ToBackend to v8 is unsupported"
      )),
    }
  }
}

impl<'s> TryFrom<Value<'s>> for crate::Value {
  type Error = Error;

  fn try_from(value: Value) -> Result<Self, Self::Error> {
    match value {
      Value::FromBackend(data, ..) => Ok(data),
      Value::ToBackend(_) => Err(anyhow::anyhow!(
        "converting a Value::ToBackend to v8 is unsupported"
      )),
    }
  }
}

pub trait FromV8: Sized {
  fn from_v8(
    scope: &mut crate::HandleScope,
    value: crate::Local<crate::Value>,
  ) -> Result<Self, Error>;
}

// WARNING(minus_v8) slightly doesn't match Deno's API
// We allow input to be an `Into<crate::Value>` so that we don't need to implement
// a cross-handle `From`, which currently doesn't work because specialization
// hasn't been stabilized yet
pub fn from_v8<T, V>(
  scope: &mut crate::HandleScope,
  input: crate::Local<V>,
) -> Result<T>
where
  T: FromV8,
  V: Into<crate::Value> + Clone,
{
  let converted = input.open(scope).clone().into();
  let converted_local = unsafe { crate::Local::from_raw(converted).unwrap() };
  T::from_v8(scope, converted_local)
}

impl<'s> FromV8 for Value<'s> {
  fn from_v8(
    scope: &mut crate::HandleScope,
    value: crate::Local<crate::Value>,
  ) -> Result<Self, Error> {
    Ok(Value::ToBackend(match value.open(scope) {
      crate::Value::Object => Box::new(crate::Object {}),
      crate::Value::String { inner } => Box::new(crate::String(inner.clone())),
      crate::Value::Boolean { inner } => {
        Box::new(crate::Boolean(inner.clone()))
      }
      crate::Value::Integer { inner } => {
        Box::new(crate::Integer(inner.clone()))
      }
      crate::Value::Function { id } => Box::new(crate::Function(id.clone())),
      crate::Value::Promise { id } => Box::new(crate::Promise(id.clone())),
      crate::Value::Exception { .. } => unimplemented!(),
      crate::Value::Null | crate::Value::Undefined => {
        Box::new(Option::<()>::None)
      }
    }))
  }
}

/// minus_v8 #[op] implementation
pub trait FromBackend<'r, 's>: Sized {
  fn from_backend(
    scope: &mut crate::HandleScope,
    value: &'r mut Box<dyn ErasedDeserializer<'s>>,
  ) -> Result<Self>;
}

pub fn from_backend<'r, 's, T>(
  scope: &mut crate::HandleScope,
  input: &'r mut Box<dyn ErasedDeserializer<'s>>,
) -> Result<T>
where
  T: FromBackend<'r, 's>,
{
  T::from_backend(scope, input)
}

impl<'r, 's, T> FromBackend<'r, 's> for T
where
  T: Deserialize<'s>,
{
  fn from_backend(
    _scope: &mut crate::HandleScope,
    value: &'r mut Box<dyn ErasedDeserializer<'s>>,
  ) -> Result<Self> {
    erased_serde::deserialize(value)
      .context("minus_v8: failed to deserialize value")
  }
}

/// minus_v8 #[op] implementation
pub trait ToBackend<'s>: Sized {
  fn to_backend(self, scope: &mut crate::HandleScope<'s>) -> JsResult<'s>;
}

impl<'s, T> ToBackend<'s> for T
where
  T: ErasedSerialize + 's,
{
  fn to_backend(self, _scope: &mut crate::HandleScope<'s>) -> JsResult<'s> {
    Ok(Box::new(self))
  }
}

pub fn to_backend<'s, T>(
  scope: &mut crate::HandleScope<'s>,
  input: T,
) -> JsResult<'s>
where
  T: ToBackend<'s>,
{
  input.to_backend(scope)
}
