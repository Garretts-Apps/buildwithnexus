//! Pieces for reading a stream event into a typed struct whose strings
//! borrow from the line, instead of building a `serde_json::Value` tree for
//! every delta.
//!
//! The typed read is deliberately strict: a field must have the type the
//! wire format gives it (or be null or absent). An event that does not fit
//! is read again as a `Value` by the caller, which tolerates any shape the
//! way the parsers always have, so no event is treated differently — the
//! common ones are just cheaper.

use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;

use serde::de::{self, Deserialize, Deserializer, MapAccess, Visitor};

/// A JSON object read as `T`, or `None` for null. Anything else (an array
/// in particular, which a derived struct would read by position) is an
/// error.
pub(super) struct Obj<T>(pub(super) Option<T>);

impl<T> Default for Obj<T> {
    fn default() -> Self {
        Obj(None)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Obj<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = Obj<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object or null")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Obj<T>, E> {
                Ok(Obj(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Obj<T>, A::Error> {
                T::deserialize(de::value::MapAccessDeserializer::new(map)).map(|t| Obj(Some(t)))
            }
        }
        d.deserialize_any(V(PhantomData))
    }
}

/// A string borrowed from the input when it has no escapes, or `None` for
/// null. (`Option<Cow<str>>` on its own always copies.)
pub(super) fn opt_str<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Cow<'de, str>>, D::Error> {
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = Option<Cow<'de, str>>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a string or null")
        }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_borrowed_str<E: de::Error>(self, s: &'de str) -> Result<Self::Value, E> {
            Ok(Some(Cow::Borrowed(s)))
        }
        fn visit_str<E: de::Error>(self, s: &str) -> Result<Self::Value, E> {
            Ok(Some(Cow::Owned(s.to_owned())))
        }
        fn visit_string<E: de::Error>(self, s: String) -> Result<Self::Value, E> {
            Ok(Some(Cow::Owned(s)))
        }
    }
    d.deserialize_any(V)
}

/// A string field of a `Value`, as the typed read would hold it.
pub(super) fn value_str(v: &serde_json::Value) -> Option<Cow<'_, str>> {
    v.as_str().map(Cow::Borrowed)
}
