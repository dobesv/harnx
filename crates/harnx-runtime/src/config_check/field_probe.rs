//! The field names a type expects at a path inside it, read from the type's
//! own `Deserialize` impl so the list can't drift from the struct.
//!
//! [`FieldProbe`] is a deserializer with no data. It walks the impl down the
//! path one key at a time, and when the struct at the end calls
//! `deserialize_struct` it records the field list serde passes and stops.
//! Anything it can't walk into (a map with a custom impl, an untagged or
//! internally tagged enum, a `flatten`ed struct, all of which go through
//! `deserialize_any`) stops it with no fields, so callers get no suggestion
//! rather than a wrong one.

use std::cell::Cell;

use serde::de::value::{Error as ProbeEnd, StrDeserializer};
use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};

type Fields = &'static [&'static str];

/// The field names `deserialize` asks for at `path`, empty when the value
/// there isn't a plain struct. `deserialize` runs some type's `Deserialize`
/// impl with the probe it is given and ignores the result.
pub(super) fn fields_at(path: &[&str], deserialize: impl FnOnce(FieldProbe<'_>)) -> Fields {
    let found = Cell::new(None);
    deserialize(FieldProbe {
        path,
        found: &found,
    });
    found.get().unwrap_or_default()
}

pub(super) struct FieldProbe<'a> {
    /// What is left to walk.
    path: &'a [&'a str],
    found: &'a Cell<Option<Fields>>,
}

impl<'a> FieldProbe<'a> {
    fn next(&self) -> Option<(&'a str, FieldProbe<'a>)> {
        let (key, rest) = self.path.split_first()?;
        Some((
            key,
            FieldProbe {
                path: rest,
                found: self.found,
            },
        ))
    }
}

/// Every probe ends in an error: once the fields are recorded, or as soon as
/// the walk can't go on.
// qual:allow(test_quality, untested) reason: "reached through serde's Deserializer methods, which the call graph doesn't follow; the module's tests cover it"
fn probe_end() -> ProbeEnd {
    de::Error::custom("field probe finished")
}

impl<'de> Deserializer<'de> for FieldProbe<'_> {
    type Error = ProbeEnd;

    fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, ProbeEnd> {
        Err(probe_end())
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: Fields,
        visitor: V,
    ) -> Result<V::Value, ProbeEnd> {
        match self.next() {
            Some((key, value)) => visitor.visit_map(OneEntry {
                key: Some(key),
                value: Some(value),
            }),
            None => {
                self.found.set(Some(fields));
                Err(probe_end())
            }
        }
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ProbeEnd> {
        let (key, value) = self.next().ok_or_else(probe_end)?;
        visitor.visit_map(OneEntry {
            key: Some(key),
            value: Some(value),
        })
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ProbeEnd> {
        // Any element will do: they share a type.
        let (_index, element) = self.next().ok_or_else(probe_end)?;
        visitor.visit_seq(OneElement {
            element: Some(element),
        })
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ProbeEnd> {
        visitor.visit_some(self)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, ProbeEnd> {
        visitor.visit_newtype_struct(self)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf unit unit_struct tuple tuple_struct enum identifier
        ignored_any
    }
}

/// A map holding the one key the probe walks into.
struct OneEntry<'a> {
    key: Option<&'a str>,
    value: Option<FieldProbe<'a>>,
}

impl<'de> MapAccess<'de> for OneEntry<'_> {
    type Error = ProbeEnd;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, ProbeEnd> {
        self.key
            .take()
            .map(|key| seed.deserialize(StrDeserializer::<ProbeEnd>::new(key)))
            .transpose()
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, ProbeEnd> {
        seed.deserialize(self.value.take().ok_or_else(probe_end)?)
    }
}

/// A sequence holding the one element the probe walks into.
struct OneElement<'a> {
    element: Option<FieldProbe<'a>>,
}

impl<'de> SeqAccess<'de> for OneElement<'_> {
    type Error = ProbeEnd;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, ProbeEnd> {
        self.element
            .take()
            .map(|element| seed.deserialize(element))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct Outer {
        #[serde(rename = "first")]
        one: u8,
        inner: Option<Box<Inner>>,
        list: Vec<Inner>,
        by_name: std::collections::HashMap<String, Inner>,
    }

    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct Inner {
        tool_calls: bool,
    }

    fn fields_of_outer(path: &[&str]) -> Fields {
        fields_at(path, |probe| {
            let _ = Outer::deserialize(probe);
        })
    }

    #[test]
    fn top_level_fields_use_serialized_names() {
        assert_eq!(fields_of_outer(&[]), &["first", "inner", "list", "by_name"]);
    }

    #[test]
    fn walks_through_options_boxes_sequences_and_maps() {
        assert_eq!(fields_of_outer(&["inner"]), &["tool_calls"]);
        assert_eq!(fields_of_outer(&["list", "3"]), &["tool_calls"]);
        assert_eq!(fields_of_outer(&["by_name", "x"]), &["tool_calls"]);
    }

    #[test]
    fn non_struct_levels_have_no_fields() {
        assert!(fields_of_outer(&["by_name"]).is_empty());
        assert!(fields_of_outer(&["first"]).is_empty());
    }
}
