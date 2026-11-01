//! Serialize command outcomes directly into [`jaq_json::Val`] for `--filter`.
//!
//! Building JSON bytes and re-parsing with [`jaq_json::read::parse_single`] duplicates
//! every object key string thousands of times on large diffs. This module walks the
//! serde structure once and interns repeated field names so the jaq input tree stays
//! close to the serialized JSON size.

use std::collections::HashMap;
use std::fmt::{self, Display};
use std::rc::Rc;

use bytes::Bytes;
use jaq_json::{Map, Num, Val};
use serde::ser::{
    Impossible, Serialize, SerializeMap as SerMap, SerializeSeq, SerializeStruct,
    SerializeStructVariant as SerStructVariant, SerializeTuple, SerializeTupleStruct,
    SerializeTupleVariant as SerTupleVariant, Serializer,
};

/// Serialize `value` into a jaq value, interning repeated object keys.
pub fn serialize_value<T: Serialize>(value: T) -> Result<Val, SerializeError> {
    let mut interner = StringInterner::default();
    value.serialize(JaqSerializer {
        interner: &mut interner,
    })
}

/// Error while building a jaq value from serde output.
#[derive(Debug)]
pub struct SerializeError(String);

impl Display for SerializeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SerializeError {}

impl serde::ser::Error for SerializeError {
    fn custom<T: Display>(msg: T) -> Self {
        Self(msg.to_string())
    }
}

#[derive(Default)]
struct StringInterner {
    keys: HashMap<Box<str>, Val>,
}

impl StringInterner {
    fn intern_key(&mut self, key: &str) -> Val {
        if let Some(existing) = self.keys.get(key) {
            return existing.clone();
        }
        let val = Val::utf8_str(Bytes::copy_from_slice(key.as_bytes()));
        self.keys.insert(key.into(), val.clone());
        val
    }
}

struct JaqSerializer<'a> {
    interner: &'a mut StringInterner,
}

macro_rules! tri {
    ($expr:expr) => {
        match $expr {
            Ok(v) => v,
            Err(e) => return Err(e),
        }
    };
}

impl<'a> Serializer for JaqSerializer<'a> {
    type Ok = Val;
    type Error = SerializeError;

    type SerializeSeq = VecSer<'a>;
    type SerializeTuple = VecSer<'a>;
    type SerializeTupleStruct = VecSer<'a>;
    type SerializeTupleVariant = TupleVariantSer<'a>;
    type SerializeMap = MapSer<'a>;
    type SerializeStruct = MapSer<'a>;
    type SerializeStructVariant = StructVariantSer<'a>;

    fn serialize_bool(self, v: bool) -> Result<Val, SerializeError> {
        Ok(Val::Bool(v))
    }

    fn serialize_i8(self, v: i8) -> Result<Val, SerializeError> {
        self.serialize_i64(i64::from(v))
    }

    fn serialize_i16(self, v: i16) -> Result<Val, SerializeError> {
        self.serialize_i64(i64::from(v))
    }

    fn serialize_i32(self, v: i32) -> Result<Val, SerializeError> {
        self.serialize_i64(i64::from(v))
    }

    fn serialize_i64(self, v: i64) -> Result<Val, SerializeError> {
        Ok(Val::Num(Num::from_integral(v)))
    }

    fn serialize_i128(self, v: i128) -> Result<Val, SerializeError> {
        Ok(Val::Num(Num::from_integral(v)))
    }

    fn serialize_u8(self, v: u8) -> Result<Val, SerializeError> {
        self.serialize_u64(u64::from(v))
    }

    fn serialize_u16(self, v: u16) -> Result<Val, SerializeError> {
        self.serialize_u64(u64::from(v))
    }

    fn serialize_u32(self, v: u32) -> Result<Val, SerializeError> {
        self.serialize_u64(u64::from(v))
    }

    fn serialize_u64(self, v: u64) -> Result<Val, SerializeError> {
        Ok(Val::Num(Num::from_integral(v)))
    }

    fn serialize_u128(self, v: u128) -> Result<Val, SerializeError> {
        Ok(Val::Num(Num::from_integral(v)))
    }

    fn serialize_f32(self, v: f32) -> Result<Val, SerializeError> {
        self.serialize_f64(f64::from(v))
    }

    fn serialize_f64(self, v: f64) -> Result<Val, SerializeError> {
        Ok(Val::Num(Num::Float(v)))
    }

    fn serialize_char(self, v: char) -> Result<Val, SerializeError> {
        let mut s = String::new();
        s.push(v);
        self.serialize_str(&s)
    }

    fn serialize_str(self, v: &str) -> Result<Val, SerializeError> {
        Ok(Val::utf8_str(Bytes::copy_from_slice(v.as_bytes())))
    }

    fn serialize_bytes(self, value: &[u8]) -> Result<Val, SerializeError> {
        let mut seq = tri!(self.serialize_seq(Some(value.len())));
        for byte in value {
            tri!(SerializeSeq::serialize_element(&mut seq, &byte));
        }
        SerializeSeq::end(seq)
    }

    fn serialize_none(self) -> Result<Val, SerializeError> {
        self.serialize_unit()
    }

    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<Val, SerializeError> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<Val, SerializeError> {
        Ok(Val::Null)
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Val, SerializeError> {
        self.serialize_unit()
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
    ) -> Result<Val, SerializeError> {
        self.serialize_str(variant)
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Val, SerializeError> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Val, SerializeError> {
        let mut map = Map::default();
        map.insert(
            self.interner.intern_key(variant),
            tri!(value.serialize(self)),
        );
        Ok(Val::obj(map))
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<Self::SerializeSeq, SerializeError> {
        Ok(VecSer {
            vec: Vec::with_capacity(len.unwrap_or(0)),
            interner: self.interner,
        })
    }

    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, SerializeError> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleStruct, SerializeError> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleVariant, SerializeError> {
        Ok(TupleVariantSer {
            name: variant,
            vec: Vec::with_capacity(len),
            interner: self.interner,
        })
    }

    fn serialize_map(self, len: Option<usize>) -> Result<Self::SerializeMap, SerializeError> {
        Ok(MapSer {
            map: Map::with_capacity_and_hasher(
                len.unwrap_or(0),
                foldhash::fast::RandomState::default(),
            ),
            next_key: None,
            interner: self.interner,
        })
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStruct, SerializeError> {
        self.serialize_map(Some(len))
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, SerializeError> {
        Ok(StructVariantSer {
            name: variant,
            map: Map::default(),
            interner: self.interner,
        })
    }
}

struct VecSer<'a> {
    vec: Vec<Val>,
    interner: &'a mut StringInterner,
}

impl SerializeSeq for VecSer<'_> {
    type Ok = Val;
    type Error = SerializeError;

    fn serialize_element<T: ?Sized + Serialize>(
        &mut self,
        value: &T,
    ) -> Result<(), SerializeError> {
        self.vec.push(tri!(value.serialize(JaqSerializer {
            interner: self.interner,
        })));
        Ok(())
    }

    fn end(self) -> Result<Val, SerializeError> {
        Ok(Val::Arr(Rc::new(self.vec)))
    }
}

impl SerializeTuple for VecSer<'_> {
    type Ok = Val;
    type Error = SerializeError;

    fn serialize_element<T: ?Sized + Serialize>(
        &mut self,
        value: &T,
    ) -> Result<(), SerializeError> {
        SerializeSeq::serialize_element(self, value)
    }

    fn end(self) -> Result<Val, SerializeError> {
        SerializeSeq::end(self)
    }
}

impl SerializeTupleStruct for VecSer<'_> {
    type Ok = Val;
    type Error = SerializeError;

    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), SerializeError> {
        SerializeSeq::serialize_element(self, value)
    }

    fn end(self) -> Result<Val, SerializeError> {
        SerializeSeq::end(self)
    }
}

struct TupleVariantSer<'a> {
    name: &'static str,
    vec: Vec<Val>,
    interner: &'a mut StringInterner,
}

impl SerTupleVariant for TupleVariantSer<'_> {
    type Ok = Val;
    type Error = SerializeError;

    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), SerializeError> {
        self.vec.push(tri!(value.serialize(JaqSerializer {
            interner: self.interner,
        })));
        Ok(())
    }

    fn end(self) -> Result<Val, SerializeError> {
        let mut map = Map::default();
        map.insert(
            self.interner.intern_key(self.name),
            Val::Arr(Rc::new(self.vec)),
        );
        Ok(Val::obj(map))
    }
}

struct MapSer<'a> {
    map: Map,
    next_key: Option<Val>,
    interner: &'a mut StringInterner,
}

impl SerMap for MapSer<'_> {
    type Ok = Val;
    type Error = SerializeError;

    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<(), SerializeError> {
        self.next_key = Some(tri!(key.serialize(MapKeySerializer {
            interner: self.interner,
        })));
        Ok(())
    }

    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), SerializeError> {
        let key = self.next_key.take().ok_or_else(|| {
            SerializeError("serialize_value called before serialize_key".to_owned())
        })?;
        self.map.insert(
            key,
            tri!(value.serialize(JaqSerializer {
                interner: self.interner,
            })),
        );
        Ok(())
    }

    fn end(self) -> Result<Val, SerializeError> {
        Ok(Val::obj(self.map))
    }
}

impl SerializeStruct for MapSer<'_> {
    type Ok = Val;
    type Error = SerializeError;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), SerializeError> {
        self.map.insert(
            self.interner.intern_key(key),
            tri!(value.serialize(JaqSerializer {
                interner: self.interner,
            })),
        );
        Ok(())
    }

    fn end(self) -> Result<Val, SerializeError> {
        SerMap::end(self)
    }
}

struct StructVariantSer<'a> {
    name: &'static str,
    map: Map,
    interner: &'a mut StringInterner,
}

impl SerStructVariant for StructVariantSer<'_> {
    type Ok = Val;
    type Error = SerializeError;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), SerializeError> {
        self.map.insert(
            self.interner.intern_key(key),
            tri!(value.serialize(JaqSerializer {
                interner: self.interner,
            })),
        );
        Ok(())
    }

    fn end(self) -> Result<Val, SerializeError> {
        let mut outer = Map::default();
        outer.insert(self.interner.intern_key(self.name), Val::obj(self.map));
        Ok(Val::obj(outer))
    }
}

struct MapKeySerializer<'a> {
    interner: &'a mut StringInterner,
}

impl Serializer for MapKeySerializer<'_> {
    type Ok = Val;
    type Error = SerializeError;

    type SerializeSeq = Impossible<Val, SerializeError>;
    type SerializeTuple = Impossible<Val, SerializeError>;
    type SerializeTupleStruct = Impossible<Val, SerializeError>;
    type SerializeTupleVariant = Impossible<Val, SerializeError>;
    type SerializeMap = Impossible<Val, SerializeError>;
    type SerializeStruct = Impossible<Val, SerializeError>;
    type SerializeStructVariant = Impossible<Val, SerializeError>;

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
    ) -> Result<Val, SerializeError> {
        Ok(self.interner.intern_key(variant))
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Val, SerializeError> {
        value.serialize(self)
    }

    fn serialize_bool(self, v: bool) -> Result<Val, SerializeError> {
        Ok(self.interner.intern_key(if v { "true" } else { "false" }))
    }

    fn serialize_i8(self, v: i8) -> Result<Val, SerializeError> {
        self.serialize_i64(i64::from(v))
    }

    fn serialize_i16(self, v: i16) -> Result<Val, SerializeError> {
        self.serialize_i64(i64::from(v))
    }

    fn serialize_i32(self, v: i32) -> Result<Val, SerializeError> {
        self.serialize_i64(i64::from(v))
    }

    fn serialize_i64(self, v: i64) -> Result<Val, SerializeError> {
        Ok(self.interner.intern_key(itoa::Buffer::new().format(v)))
    }

    fn serialize_i128(self, v: i128) -> Result<Val, SerializeError> {
        Ok(self.interner.intern_key(&v.to_string()))
    }

    fn serialize_u8(self, v: u8) -> Result<Val, SerializeError> {
        self.serialize_u64(u64::from(v))
    }

    fn serialize_u16(self, v: u16) -> Result<Val, SerializeError> {
        self.serialize_u64(u64::from(v))
    }

    fn serialize_u32(self, v: u32) -> Result<Val, SerializeError> {
        self.serialize_u64(u64::from(v))
    }

    fn serialize_u64(self, v: u64) -> Result<Val, SerializeError> {
        Ok(self.interner.intern_key(itoa::Buffer::new().format(v)))
    }

    fn serialize_u128(self, v: u128) -> Result<Val, SerializeError> {
        Ok(self.interner.intern_key(&v.to_string()))
    }

    fn serialize_f32(self, v: f32) -> Result<Val, SerializeError> {
        self.serialize_f64(f64::from(v))
    }

    fn serialize_f64(self, v: f64) -> Result<Val, SerializeError> {
        if v.is_finite() {
            Ok(self
                .interner
                .intern_key(ryu::Buffer::new().format_finite(v)))
        } else {
            Err(SerializeError(
                "JSON object keys must be finite numbers".to_owned(),
            ))
        }
    }

    fn serialize_char(self, v: char) -> Result<Val, SerializeError> {
        let mut s = String::new();
        s.push(v);
        self.serialize_str(&s)
    }

    fn serialize_str(self, v: &str) -> Result<Val, SerializeError> {
        Ok(self.interner.intern_key(v))
    }

    fn serialize_unit(self) -> Result<Val, SerializeError> {
        Err(SerializeError(
            "JSON object keys must be strings or numbers".to_owned(),
        ))
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Val, SerializeError> {
        self.serialize_unit()
    }

    fn serialize_bytes(self, _value: &[u8]) -> Result<Val, SerializeError> {
        Err(SerializeError(
            "JSON object keys must be strings or numbers".to_owned(),
        ))
    }

    fn serialize_none(self) -> Result<Val, SerializeError> {
        self.serialize_unit()
    }

    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<Val, SerializeError> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Val, SerializeError> {
        let mut map = Map::default();
        map.insert(
            self.interner.intern_key(variant),
            tri!(value.serialize(JaqSerializer {
                interner: self.interner,
            })),
        );
        Ok(Val::obj(map))
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, SerializeError> {
        Err(SerializeError(
            "JSON object keys must be strings or numbers".to_owned(),
        ))
    }

    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, SerializeError> {
        Err(SerializeError(
            "JSON object keys must be strings or numbers".to_owned(),
        ))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, SerializeError> {
        Err(SerializeError(
            "JSON object keys must be strings or numbers".to_owned(),
        ))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, SerializeError> {
        Err(SerializeError(
            "JSON object keys must be strings or numbers".to_owned(),
        ))
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, SerializeError> {
        Err(SerializeError(
            "JSON object keys must be strings or numbers".to_owned(),
        ))
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, SerializeError> {
        Err(SerializeError(
            "JSON object keys must be strings or numbers".to_owned(),
        ))
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, SerializeError> {
        Err(SerializeError(
            "JSON object keys must be strings or numbers".to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::serialize_value;
    use jaq_json::Val;
    use serde::Serialize;

    #[derive(Serialize)]
    struct Row {
        path: String,
        kind: &'static str,
    }

    #[derive(Serialize)]
    struct Doc {
        files: Vec<Row>,
    }

    #[test]
    fn interned_keys_share_allocation() {
        let doc = Doc {
            files: (0..5000)
                .map(|i| Row {
                    path: format!("src/file{i}.rs"),
                    kind: "modified",
                })
                .collect(),
        };
        let val = serialize_value(&doc).expect("serialize");
        let Val::Obj(root) = val else {
            panic!("expected object");
        };
        let files_key = doc.files.first().map(|_| ()).expect("nonempty");
        let _ = files_key;
        let files = root.get(&Val::utf8_str("files")).expect("files field");
        let Val::Arr(files) = files else {
            panic!("expected files array");
        };
        let Val::Obj(row0) = &files[0] else {
            panic!("row object");
        };
        let Val::Obj(row1) = &files[1] else {
            panic!("row object");
        };
        let (k0, _) = row0.get_index(0).expect("key");
        let (k1, _) = row1.get_index(0).expect("key");
        let Val::TStr(b0) = k0 else {
            panic!("key");
        };
        let Val::TStr(b1) = k1 else {
            panic!("key");
        };
        assert!(std::ptr::eq(b0.as_ref().as_ptr(), b1.as_ref().as_ptr()));
    }
}
