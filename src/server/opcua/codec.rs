//! Structured OPC UA values exposed to handlers; deliberately excludes opaque payloads.
use ::opcua::types::{NodeId, Variant};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
pub const MAX_FRAME: usize = 262144;
pub fn node(v: &Value, key: &str) -> Result<NodeId> {
    let s = v[key].as_str().context("NodeId string required")?;
    ensure!(
        s.len() <= 256 && !s.contains(";b=") && !s.starts_with("b="),
        "only numeric/string/GUID NodeIds"
    );
    s.parse().map_err(|_| anyhow::anyhow!("invalid NodeId"))
}
pub fn value(v: &Value) -> Result<Variant> {
    match v["value_type"].as_str() {
        Some("double") => {
            let n = v["value"].as_f64().context("double")?;
            ensure!(n.is_finite(), "finite value required");
            Ok(Variant::Double(n))
        }
        Some("boolean") => Ok(Variant::Boolean(v["value"].as_bool().context("boolean")?)),
        Some("int32") => Ok(Variant::Int32(i32::try_from(
            v["value"].as_i64().context("integer")?,
        )?)),
        Some("uint32") => Ok(Variant::UInt32(u32::try_from(
            v["value"].as_u64().context("unsigned")?,
        )?)),
        Some("string") => {
            let s = v["value"].as_str().context("string")?;
            ensure!(s.len() <= 1024, "string limit");
            Ok(s.into())
        }
        _ => bail!("unsupported scalar type"),
    }
}
pub fn structured(v: &Variant) -> Result<Value> {
    let (t, n) = match v {
        Variant::Double(n) if n.is_finite() => ("double", json!(n)),
        Variant::Float(n) if n.is_finite() => ("double", json!(n)),
        Variant::Boolean(n) => ("boolean", json!(n)),
        Variant::Int32(n) => ("int32", json!(n)),
        Variant::UInt32(n) => ("uint32", json!(n)),
        Variant::String(n) => ("string", json!(n.as_ref())),
        _ => bail!("unsupported scalar response"),
    };
    Ok(json!({"value_type":t,"value":n}))
}
pub fn validate(v: &Value) -> Result<()> {
    match v["type"].as_str() {
        Some("opcua_browse") | Some("opcua_read") | Some("opcua_subscribe") => {
            node(v, "node_id")?;
        }
        Some("opcua_write") => {
            node(v, "node_id")?;
            value(v)?;
        }
        Some("opcua_call") => {
            node(v, "object_id")?;
            node(v, "method_id")?;
            let a = v["arguments"].as_array().context("arguments")?;
            ensure!(a.len() <= 16, "argument limit");
            for a in a {
                value(a)?;
            }
        }
        _ => bail!("unknown OPC UA action"),
    };
    Ok(())
}
