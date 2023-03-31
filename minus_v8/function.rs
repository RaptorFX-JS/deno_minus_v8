use crate::serde::{ErasedDeserializer, ErasedSerialize};
use crate::{HandleScope, Isolate};
use std::ffi::c_void;
use std::sync::Arc;

pub struct FunctionCallbackInfo<'s> {
  pub isolate: &'s mut Isolate,
  pub data: *const c_void,
  pub values: Vec<Box<dyn ErasedDeserializer<'s>>>,
}

pub struct FunctionCallbackArguments<'s> {
  pub data: *const c_void,
  values: Vec<Box<dyn ErasedDeserializer<'s>>>,
}

impl<'s> FunctionCallbackArguments<'s> {
  pub fn get(&mut self, i: i32) -> &mut Box<dyn ErasedDeserializer<'s>> {
    &mut self.values[i as usize]
  }
}

pub type FunctionCallback = Arc<
  dyn for<'s> Fn(FunctionCallbackInfo<'s>) -> Box<dyn ErasedSerialize + 's>,
>;

pub struct ReturnValue<'s>(Option<Box<dyn ErasedSerialize + 's>>);

impl<'s> ReturnValue<'s> {
  pub fn set(&mut self, value: Box<dyn ErasedSerialize + 's>) {
    self.0 = Some(value);
  }

  pub fn set_uint32(&mut self, i: u32) {
    self.0 = Some(Box::new(i));
  }
}

pub trait MapFnTo {
  fn map_fn_to(self) -> FunctionCallback;
}

impl<F> MapFnTo for F
where
  F: for<'s> Fn(
      &mut HandleScope<'s>,
      FunctionCallbackArguments<'s>,
      &mut ReturnValue<'s>,
    ) + 'static,
{
  fn map_fn_to(self) -> FunctionCallback {
    let inner = self;
    Arc::new(
      move |info: FunctionCallbackInfo| -> Box<dyn ErasedSerialize> {
        let mut return_val = ReturnValue(None);
        inner(
          &mut HandleScope::new(info.isolate),
          FunctionCallbackArguments {
            data: info.data,
            values: info.values,
          },
          &mut return_val,
        );
        return_val.0.unwrap_or_else(|| Box::new(Option::<()>::None))
      },
    )
  }
}
