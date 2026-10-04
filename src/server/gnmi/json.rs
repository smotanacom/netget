//! Bounded JSON values, decoded before any gNMI JSON bytes reach a model event.
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};
use tonic::Status;
pub fn parse(bytes: &[u8]) -> Result<Value, Status> {
    parse_with_budget(bytes, &mut 0)
}
pub(super) fn parse_with_budget(bytes: &[u8], nodes: &mut usize) -> Result<Value, Status> {
    if bytes.len() > super::codec::MAX_VALUE_BYTES {
        return Err(Status::resource_exhausted("gNMI JSON value exceeds 64 KiB"));
    }
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let mut deepest = 0usize;
    let value = Seed {
        nodes,
        depth: 0,
        deepest: &mut deepest,
    }
    .deserialize(&mut decoder)
    .map_err(|_| {
        if *nodes > super::codec::MAX_NODES || deepest > super::codec::MAX_DEPTH {
            Status::resource_exhausted("gNMI JSON node/depth bound")
        } else {
            Status::invalid_argument("invalid gNMI JSON or text/key bound")
        }
    })?;
    decoder
        .end()
        .map_err(|_| Status::invalid_argument("trailing JSON value"))?;
    Ok(value)
}
struct Seed<'a> {
    nodes: &'a mut usize,
    depth: usize,
    deepest: &'a mut usize,
}
impl<'de> DeserializeSeed<'de> for Seed<'_> {
    type Value = Value;
    fn deserialize<D: serde::Deserializer<'de>>(self, decoder: D) -> Result<Value, D::Error> {
        *self.nodes += 1;
        *self.deepest = (*self.deepest).max(self.depth);
        if *self.nodes > super::codec::MAX_NODES || self.depth > super::codec::MAX_DEPTH {
            return Err(serde::de::Error::custom("JSON node/depth bound"));
        }
        decoder.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Seed<'_> {
    type Value = Value;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("bounded JSON value")
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Value, E> {
        Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("nonfinite JSON number"))
    }
    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Value, E> {
        if v.len() > super::codec::MAX_VALUE_BYTES {
            return Err(E::custom("JSON text bound"));
        }
        Ok(Value::String(v.into()))
    }
    fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Value, E> {
        self.visit_str(&v)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut result = Vec::new();
        while let Some(v) = seq.next_element_seed(Seed {
            nodes: self.nodes,
            depth: self.depth + 1,
            deepest: self.deepest,
        })? {
            result.push(v);
        }
        Ok(Value::Array(result))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut result = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if key.len() > 256 || result.contains_key(&key) {
                return Err(serde::de::Error::custom("JSON key bound or duplicate"));
            }
            *self.nodes += 1;
            if *self.nodes > super::codec::MAX_NODES {
                return Err(serde::de::Error::custom("JSON node bound"));
            }
            let value = map.next_value_seed(Seed {
                nodes: self.nodes,
                depth: self.depth + 1,
                deepest: self.deepest,
            })?;
            result.insert(key, value);
        }
        Ok(Value::Object(result))
    }
}
