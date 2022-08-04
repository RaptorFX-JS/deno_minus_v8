use std::fmt::Formatter;
use std::ops::Deref;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde::de::{Error, Visitor};

/// minus_v8: added optional extra meta items
/// keep everything else in sync with serde_v8/magic/transl8.rs
macro_rules! impl_wrapper {
  (#[$($attr:meta),+$(,)?] ($rest:tt)*) => {
    $(#[$($attr),*])*
    impl_wrapper!($(rest)*)
  };

  ($i:item) => {
    #[derive(
      PartialEq,
      Eq,
      Clone,
      Debug,
      Default,
      derive_more::Deref,
      derive_more::DerefMut,
      derive_more::AsRef,
      derive_more::AsMut,
      derive_more::From,
    )]
    #[as_mut(forward)]
    #[as_ref(forward)]
    #[from(forward)]
    $i
  };
}

impl_wrapper! {
  pub struct ByteString(Vec<u8>);
}

impl Serialize for ByteString {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer
  {
    // SAFETY: ByteString should only ever contain valid Latin-1
    serializer.serialize_str(std::str::from_utf8(&self.0).unwrap())
  }
}

impl<'de> Deserialize<'de> for ByteString {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let string = String::deserialize(deserializer)?;
    if encoding_rs::mem::is_utf8_latin1(string.as_bytes()) {
      Ok(ByteString(string.into_bytes()))
    } else {
      Err(D::Error::custom("minus_v8: ExpectedLatin1"))
    }
  }
}

impl Into<Vec<u8>> for ByteString {
  fn into(self) -> Vec<u8> {
    self.0
  }
}

impl_wrapper! {
  /// TODO(minus_v8) can we make this actually zero-copy given an unknown JS backend?
  #[derive(Serialize, Deserialize)]
  #[serde(transparent)]
  pub struct ZeroCopyBuf(Vec<u8>);
}

impl ZeroCopyBuf {
  pub fn empty() -> Self {
    ZeroCopyBuf(vec![0_u8; 0])
  }

  pub fn new_temp(vec: Vec<u8>) -> Self {
    ZeroCopyBuf(vec)
  }

  // TODO(@littledivy): Temporary, this needs a refactor.
  // TODO(minus_v8) watch out for eventual refactoring ^
  pub fn to_temp(self) -> Vec<u8> {
    self.0
  }
}

impl Into<Vec<u8>> for ZeroCopyBuf {
  fn into(self) -> Vec<u8> {
    self.0
  }
}

impl From<ZeroCopyBuf> for bytes::Bytes {
  fn from(zbuf: ZeroCopyBuf) -> bytes::Bytes {
    zbuf.0.into()
  }
}

#[derive(Debug)]
pub enum StringOrBuffer {
  Buffer(ZeroCopyBuf),
  String(String),
}

impl Deref for StringOrBuffer {
  type Target = [u8];
  fn deref(&self) -> &Self::Target {
    match self {
      Self::Buffer(b) => b.as_ref(),
      Self::String(s) => s.as_bytes(),
    }
  }
}

impl Serialize for StringOrBuffer {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer
  {
    match self {
      Self::Buffer(buf) => buf.serialize(serializer),
      Self::String(s) => s.serialize(serializer),
    }
  }
}

impl<'de> Deserialize<'de> for StringOrBuffer {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    struct StringOrBufferVisitor;

    impl<'v> Visitor<'v> for StringOrBufferVisitor {
      type Value = StringOrBuffer;

      fn expecting(&self, formatter: &mut Formatter) -> std::fmt::Result {
        write!(formatter, "expecting a string or byte buffer")
      }

      fn visit_str<E: Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(StringOrBuffer::String(v.into()))
      }

      fn visit_string<E: Error>(self, v: String) -> Result<Self::Value, E> {
        Ok(StringOrBuffer::String(v))
      }

      fn visit_bytes<E: Error>(self, v: &[u8]) -> Result<Self::Value, E> {
        Ok(StringOrBuffer::Buffer(v.into()))
      }

      fn visit_byte_buf<E: Error>(self, v: Vec<u8>) -> Result<Self::Value, E> {
        Ok(StringOrBuffer::Buffer(v.into()))
      }
    }

    deserializer.deserialize_any(StringOrBufferVisitor)
  }
}

impl From<StringOrBuffer> for bytes::Bytes {
  fn from(sob: StringOrBuffer) -> Self {
    match sob {
      StringOrBuffer::Buffer(b) => b.into(),
      StringOrBuffer::String(s) => s.into_bytes().into(),
    }
  }
}
