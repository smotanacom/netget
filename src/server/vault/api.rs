//! Vault HTTP API routing and response rendering, KV version 2 only.
//!
//! Pure functions: a method and path in, a [`Route`] out; a model's structured answer in, the
//! JSON envelope the `vault` CLI's Go client decodes out. What the CLI needs that is not a
//! secret — the mount preflight that tells it the engine is KV v2, seal status, health, the
//! envelope's `request_id` / `lease_*` / `wrap_info` fields — is NetGet's. The secrets, key
//! listings and versions are the model's.

use serde_json::{json, Map, Value};

/// Largest number of keys one `send_vault_list` may carry.
pub const MAX_LIST_KEYS: usize = 10_000;

/// What a request resolves to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// `GET /v1/sys/seal-status`
    SealStatus,
    /// `GET /v1/sys/health`
    Health,
    /// `GET /v1/sys/leader`
    Leader,
    /// `GET /v1/sys/internal/ui/mounts/<path>` — the CLI's KV version preflight.
    MountPreflight(String),
    /// `GET /v1/<mount>/data/<path>`
    ReadData { mount: String, path: String },
    /// `PUT`/`POST /v1/<mount>/data/<path>`
    WriteData { mount: String, path: String },
    /// `LIST /v1/<mount>/metadata/<path>` or `GET …?list=true`
    List { mount: String, path: String },
    /// `GET /v1/<mount>/metadata/<path>`
    ReadMetadata { mount: String, path: String },
    /// A path under a KV mount with a method this server does not implement (delete, patch,
    /// undelete, destroy, config).
    Unsupported,
    /// Anything else.
    NotFound,
}

/// Split `path` (without `/v1/`) into the longest configured mount and the rest.
fn split_mount<'a>(path: &'a str, mounts: &[String]) -> Option<(String, &'a str)> {
    let path = path.trim_start_matches('/');
    mounts
        .iter()
        .filter(|m| {
            let m = m.trim_matches('/');
            path == m || path.starts_with(&format!("{m}/"))
        })
        .max_by_key(|m| m.len())
        .map(|m| {
            let m = m.trim_matches('/');
            (m.to_string(), path[m.len()..].trim_start_matches('/'))
        })
}

/// Resolve a request. `path` is the URL path; `list_query` is whether `list=true` was given.
pub fn resolve(method: &str, path: &str, list_query: bool, mounts: &[String]) -> Route {
    let Some(rest) = path.strip_prefix("/v1/") else {
        return Route::NotFound;
    };
    let is_get = method == "GET" || method == "HEAD";
    match rest.trim_end_matches('/') {
        "sys/seal-status" if is_get => return Route::SealStatus,
        "sys/health" if is_get => return Route::Health,
        "sys/leader" if is_get => return Route::Leader,
        _ => {}
    }
    if let Some(p) = rest.strip_prefix("sys/internal/ui/mounts/") {
        if is_get {
            return Route::MountPreflight(p.to_string());
        }
        return Route::NotFound;
    }
    let Some((mount, after)) = split_mount(rest, mounts) else {
        return Route::NotFound;
    };
    let (kind, secret_path) = match after.split_once('/') {
        Some((k, p)) => (k, p.trim_end_matches('/').to_string()),
        None => (after, String::new()),
    };
    match (kind, method) {
        ("data", "GET") if !secret_path.is_empty() => Route::ReadData {
            mount,
            path: secret_path,
        },
        ("data", "PUT" | "POST") if !secret_path.is_empty() => Route::WriteData {
            mount,
            path: secret_path,
        },
        ("metadata", "LIST") => Route::List {
            mount,
            path: secret_path,
        },
        ("metadata", "GET") if list_query => Route::List {
            mount,
            path: secret_path,
        },
        ("metadata", "GET") if !secret_path.is_empty() => Route::ReadMetadata {
            mount,
            path: secret_path,
        },
        _ => Route::Unsupported,
    }
}

/// 64 hex characters derived from `seed`, for identifiers (accessor, cluster id) that must be
/// stable for one server and mean nothing.
fn hex_id(seed: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut out = String::with_capacity(64);
    for round in 0u8..4 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (round, seed).hash(&mut h);
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

/// Vault's error body: `{"errors": [...]}`.
pub fn errors_body(errors: &[String]) -> Value {
    json!({ "errors": errors })
}

/// A request id in UUID form. Unique per response; it identifies nothing.
pub fn request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The response envelope every secret-engine answer is wrapped in.
pub fn envelope(data: Value, mount_type: &str) -> Value {
    json!({
        "request_id": request_id(),
        "lease_id": "",
        "renewable": false,
        "lease_duration": 0,
        "data": data,
        "wrap_info": null,
        "warnings": null,
        "auth": null,
        "mount_type": mount_type,
    })
}

/// `/v1/sys/internal/ui/mounts/<path>` for a path under a configured mount: what makes the CLI
/// switch to KV v2 paths (`<mount>/data/…`, `<mount>/metadata/…`).
pub fn mount_preflight(mount: &str) -> Value {
    envelope(
        json!({
            "accessor": format!("kv_{}", &hex_id(mount)[..8]),
            "config": {"default_lease_ttl": 0, "force_no_cache": false, "max_lease_ttl": 0},
            "description": "key/value secret storage",
            "external_entropy_access": false,
            "local": false,
            "options": {"version": "2"},
            "path": format!("{mount}/"),
            "plugin_version": "",
            "running_plugin_version": "v0.20.0+builtin",
            "running_sha256": "",
            "seal_wrap": false,
            "type": "kv",
            "uuid": request_id(),
        }),
        "",
    )
}

/// Which mount (if any) a preflight path falls under.
pub fn preflight_mount(path: &str, mounts: &[String]) -> Option<String> {
    split_mount(path, mounts).map(|(m, _)| m)
}

/// Identity for the static `sys/` endpoints.
#[derive(Clone, Debug)]
pub struct VaultIdentity {
    pub version: String,
    pub cluster_name: String,
}

pub fn seal_status(id: &VaultIdentity) -> Value {
    json!({
        "type": "shamir",
        "initialized": true,
        "sealed": false,
        "t": 1,
        "n": 1,
        "progress": 0,
        "nonce": "",
        "version": id.version,
        "build_date": "2025-01-01T00:00:00Z",
        "migration": false,
        "cluster_name": id.cluster_name,
        "cluster_id": hex_id(&id.cluster_name)[..36].to_string(),
        "recovery_seal": false,
        "storage_type": "inmem",
    })
}

pub fn health(id: &VaultIdentity) -> Value {
    json!({
        "initialized": true,
        "sealed": false,
        "standby": false,
        "performance_standby": false,
        "replication_performance_mode": "disabled",
        "replication_dr_mode": "disabled",
        "server_time_utc": now_unix(),
        "version": id.version,
        "enterprise": false,
        "cluster_name": id.cluster_name,
        "cluster_id": hex_id(&id.cluster_name)[..36].to_string(),
        "echo_duration_ms": 0,
        "clock_skew_ms": 0,
    })
}

pub fn leader() -> Value {
    json!({
        "ha_enabled": false,
        "is_self": false,
        "active_time": "0001-01-01T00:00:00Z",
        "leader_address": "",
        "leader_cluster_address": "",
        "performance_standby": false,
        "performance_standby_last_remote_wal": 0,
    })
}

fn now_unix() -> i64 {
    crate::utils::clock::SystemTime::now()
        .duration_since(crate::utils::clock::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_rfc3339() -> String {
    chrono::DateTime::from_timestamp(now_unix(), 0)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
        .unwrap_or_default()
}

/// A timestamp the model gave, or now. Refuses one that is not RFC 3339.
fn created_time(v: Option<&Value>) -> Result<String, String> {
    match v {
        None | Some(Value::Null) => Ok(now_rfc3339()),
        Some(Value::String(s)) => chrono::DateTime::parse_from_rfc3339(s.trim())
            .map(|t| {
                t.with_timezone(&chrono::Utc)
                    .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
            })
            .map_err(|_| format!("created_time {s:?} is not RFC 3339")),
        Some(_) => Err("created_time must be an RFC 3339 string".to_string()),
    }
}

fn version_of(v: Option<&Value>) -> Result<u64, String> {
    match v {
        None | Some(Value::Null) => Ok(1),
        Some(Value::Number(n)) => match n.as_u64() {
            Some(0) | None => Err("version must be a positive integer".to_string()),
            Some(n) => Ok(n),
        },
        Some(Value::String(s)) => s
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| format!("version {s:?} must be a positive integer")),
        Some(_) => Err("version must be a positive integer".to_string()),
    }
}

fn custom_metadata(v: Option<&Value>) -> Result<Value, String> {
    match v {
        None | Some(Value::Null) => Ok(Value::Null),
        Some(Value::Object(m)) => {
            let mut out = Map::new();
            for (k, v) in m {
                let text = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                out.insert(k.clone(), Value::String(text));
            }
            Ok(Value::Object(out))
        }
        Some(_) => Err("custom_metadata must be an object of strings".to_string()),
    }
}

/// The version metadata block shared by reads and writes.
fn version_metadata(answer: &Value) -> Result<Value, String> {
    Ok(json!({
        "created_time": created_time(answer.get("created_time"))?,
        "custom_metadata": custom_metadata(answer.get("custom_metadata"))?,
        "deletion_time": "",
        "destroyed": false,
        "version": version_of(answer.get("version"))?,
    }))
}

/// The secret's key/value map. Any JSON object is a valid secret — KV stores arbitrary JSON —
/// but it must be an object.
fn secret_data(answer: &Value) -> Result<Value, String> {
    match answer.get("data") {
        None | Some(Value::Null) => Ok(json!({})),
        Some(Value::Object(m)) => Ok(Value::Object(m.clone())),
        Some(_) => Err("data must be an object of key/value pairs".to_string()),
    }
}

/// `GET <mount>/data/<path>` body.
pub fn render_secret(answer: &Value) -> Result<Value, String> {
    Ok(envelope(
        json!({"data": secret_data(answer)?, "metadata": version_metadata(answer)?}),
        "kv",
    ))
}

/// `PUT <mount>/data/<path>` body.
pub fn render_write_ok(answer: &Value) -> Result<Value, String> {
    Ok(envelope(version_metadata(answer)?, "kv"))
}

/// `GET <mount>/metadata/<path>` body — built from the same answer as a read.
pub fn render_metadata(answer: &Value) -> Result<Value, String> {
    let meta = version_metadata(answer)?;
    let current = meta["version"].as_u64().unwrap_or(1);
    let created = meta["created_time"].clone();
    let mut versions = Map::new();
    versions.insert(
        current.to_string(),
        json!({"created_time": created, "deletion_time": "", "destroyed": false}),
    );
    Ok(envelope(
        json!({
            "cas_required": false,
            "created_time": created,
            "current_version": current,
            "custom_metadata": meta["custom_metadata"],
            "delete_version_after": "0s",
            "max_versions": 0,
            "oldest_version": 0,
            "updated_time": created,
            "versions": versions,
        }),
        "kv",
    ))
}

/// `LIST <mount>/metadata/<path>` body. Keys are names, not paths: a folder ends in `/`, and a
/// key containing a `/` anywhere else would claim a nesting the listing does not have.
pub fn render_list(answer: &Value) -> Result<Value, String> {
    let keys = answer
        .get("keys")
        .and_then(Value::as_array)
        .ok_or("keys must be an array of strings")?;
    if keys.len() > MAX_LIST_KEYS {
        return Err(format!("more than {MAX_LIST_KEYS} keys in one listing"));
    }
    let mut out = Vec::with_capacity(keys.len());
    for k in keys {
        let k = k.as_str().ok_or("keys must be strings")?;
        let name = k.strip_suffix('/').unwrap_or(k);
        if name.is_empty() || name.contains('/') || name.len() > 512 {
            return Err(format!(
                "key {k:?} must be one path segment, with a trailing '/' for a folder"
            ));
        }
        out.push(Value::String(k.to_string()));
    }
    Ok(envelope(json!({"keys": out}), "kv"))
}

/// The status and errors of a `send_vault_error`.
pub fn render_error(answer: &Value) -> Result<(u16, Vec<String>), String> {
    let status = answer.get("status").and_then(Value::as_u64).unwrap_or(400);
    if !(400..600).contains(&status) {
        return Err(format!(
            "status must be a 4xx or 5xx HTTP status, got {status}"
        ));
    }
    let errors = match answer.get("errors") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|e| match e {
                Value::String(s) => crate::utils::sanitize::line_field(s),
                other => other.to_string(),
            })
            .collect(),
        Some(Value::String(s)) => vec![crate::utils::sanitize::line_field(s)],
        Some(_) => return Err("errors must be an array of strings".to_string()),
    };
    Ok((status as u16, errors))
}
