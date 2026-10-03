//! Selected KV v2 and userpass semantics; no protocol-specific secret store.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
pub const MAX_BODY: usize = 1024 * 1024;
pub const MAX_ITEMS: usize = 10000;
pub const MAX_TEXT: usize = 16 * 1024;
pub const MAX_TOKEN: usize = 8192;
pub const MAX_DEPTH: usize = 32;
pub const MAX_NODES: usize = 65536;
pub const MAX_RETAINED_BYTES: usize = 8 * 1024 * 1024;
pub struct Request {
    pub operation: String,
    pub method: &'static str,
    pub path: String,
    pub body: Vec<u8>,
    pub redacted: Value,
}
pub fn token(value: &str) -> Result<hyper::header::HeaderValue> {
    ensure!(
        !value.is_empty()
            && value.len() <= MAX_TOKEN
            && value.bytes().all(|c| c.is_ascii_graphic()),
        "Vault token must be bounded printable ASCII without whitespace"
    );
    let mut header: hyper::header::HeaderValue = value.parse()?;
    header.set_sensitive(true);
    Ok(header)
}
pub fn path(value: &str, empty: bool) -> Result<()> {
    ensure!(
        (empty || !value.is_empty()) && value.len() <= 4096,
        "Vault path length/empty refusal"
    );
    if value.is_empty() {
        return Ok(());
    }
    ensure!(
        value.split('/').count() <= 32
            && value.split('/').all(|s| !s.is_empty()
                && s.len() <= 256
                && s != "."
                && s != ".."
                && s.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-.".contains(&c))),
        "Vault path requires bounded ASCII name segments without dot/empty segments"
    );
    Ok(())
}
pub fn mount(value: &str) -> Result<()> {
    path(value, false)?;
    ensure!(
        value.len() <= 256 && value.split('/').next() != Some("sys"),
        "invalid Vault mount"
    );
    Ok(())
}
pub fn action_within_budget(action: &Value) -> bool {
    crate::utils::json_budget::within_budget(action, MAX_RETAINED_BYTES, MAX_NODES, MAX_DEPTH)
}
pub fn request(action: &Value, default_mount: &str, auth_mount: &str) -> Result<Request> {
    ensure!(
        action_within_budget(action),
        "Vault action depth/node/retained-content limit"
    );
    let kind = action["type"]
        .as_str()
        .context("Vault action type required")?;
    let operation = match kind {
        "vault_request" => action["operation"]
            .as_str()
            .context("Vault operation required")?,
        "vault_userpass_login" => "login",
        "vault_clear_token" => "clear_token",
        _ => anyhow::bail!("unknown Vault action"),
    };
    let fields: Vec<&str> = match operation {
        "seal_status" | "health" | "leader" => vec!["type", "operation"],
        "read" => vec!["type", "operation", "mount", "path", "version"],
        "write" => vec!["type", "operation", "mount", "path", "data", "cas"],
        "list" | "metadata" => vec!["type", "operation", "mount", "path"],
        "login" => vec!["type", "username", "password", "auth_mount"],
        "clear_token" => vec!["type"],
        _ => anyhow::bail!("unsupported selected Vault operation"),
    };
    ensure!(
        action
            .as_object()
            .context("Vault action must be object")?
            .keys()
            .all(|k| fields.contains(&k.as_str())),
        "unsupported field for selected Vault operation"
    );
    let mut redacted = action.clone();
    let mut body = Vec::new();
    let (method, route) = match operation {
        "seal_status" => ("GET", "/v1/sys/seal-status".into()),
        "health" => ("GET", "/v1/sys/health".into()),
        "leader" => ("GET", "/v1/sys/leader".into()),
        "clear_token" => ("", "".into()),
        "login" => {
            let auth_mount = match action.get("auth_mount") {
                None => auth_mount,
                Some(v) => v.as_str().context("auth_mount must be string")?,
            };
            mount(auth_mount)?;
            let username = action["username"].as_str().context("username required")?;
            ensure!(
                !username.is_empty()
                    && username.len() <= 256
                    && !username.starts_with(['-', '.'])
                    && !username.ends_with('.')
                    && username
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"_-.".contains(&c)),
                "invalid userpass username"
            );
            let password = action["password"].as_str().context("password required")?;
            ensure!(
                !password.is_empty() && password.len() <= MAX_TEXT,
                "password length refusal"
            );
            redacted.as_object_mut().unwrap().remove("password");
            redacted["password_present"] = json!(true);
            body = serde_json::to_vec(&json!({"password":password}))?;
            ("POST", format!("/v1/auth/{auth_mount}/login/{username}"))
        }
        _ => {
            let mount_name = match action.get("mount") {
                None => default_mount,
                Some(v) => v.as_str().context("mount must be string")?,
            };
            mount(mount_name)?;
            let secret_path = action["path"]
                .as_str()
                .context("path required (empty allowed for list)")?;
            let validation_path = if operation == "list" {
                ensure!(secret_path != "/", "mount-root list requires empty path");
                secret_path.strip_suffix('/').unwrap_or(secret_path)
            } else {
                secret_path
            };
            path(validation_path, operation == "list")?;
            let (method, kind) = if operation == "write" {
                ("PUT", "data")
            } else if ["list", "metadata"].contains(&operation) {
                ("GET", "metadata")
            } else {
                ("GET", "data")
            };
            let mut route = format!("/v1/{mount_name}/{kind}/{secret_path}");
            if operation == "read" {
                if let Some(version) = action.get("version") {
                    let version = version
                        .as_u64()
                        .context("version must be nonnegative integer;0 means latest")?;
                    route.push_str(&format!("?version={version}"));
                }
            }
            if operation == "list" {
                route.push_str("?list=true");
            }
            if operation == "write" {
                let data = action["data"]
                    .as_object()
                    .context("KV write data must be object")?;
                let mut envelope = json!({"data":data});
                if let Some(cas) = action.get("cas") {
                    let cas = cas
                        .as_u64()
                        .context("cas must be nonnegative integer;0 means create-only")?;
                    envelope["options"] = json!({"cas":cas});
                }
                body = serde_json::to_vec(&envelope)?;
                ensure!(body.len() <= MAX_BODY, "Vault write body limit");
                json(&body)?;
            }
            (method, route)
        }
    };
    Ok(Request {
        operation: operation.into(),
        method,
        path: route,
        body,
        redacted,
    })
}
#[derive(Deserialize, Serialize)]
pub struct SealStatus {
    #[serde(rename(deserialize = "type"))]
    pub seal_type: String,
    pub initialized: bool,
    pub sealed: bool,
    pub t: u64,
    pub n: u64,
    pub progress: u64,
    pub nonce: String,
    pub version: String,
    pub cluster_name: Option<String>,
    pub cluster_id: Option<String>,
    pub migration: Option<bool>,
    pub recovery_seal: Option<bool>,
    pub storage_type: Option<String>,
}
#[derive(Deserialize, Serialize)]
pub struct Health {
    pub initialized: bool,
    pub sealed: bool,
    pub standby: bool,
    pub performance_standby: Option<bool>,
    pub replication_performance_mode: Option<String>,
    pub replication_dr_mode: Option<String>,
    pub server_time_utc: i64,
    pub version: String,
    pub enterprise: Option<bool>,
    pub cluster_name: Option<String>,
    pub cluster_id: Option<String>,
}
#[derive(Deserialize, Serialize)]
pub struct Leader {
    pub ha_enabled: bool,
    pub is_self: bool,
    pub leader_address: Option<String>,
    pub leader_cluster_address: Option<String>,
    pub performance_standby: Option<bool>,
    pub active_time: Option<String>,
}
#[derive(Deserialize)]
struct Envelope {
    request_id: String,
    lease_id: String,
    lease_duration: u64,
    renewable: bool,
    data: Value,
    auth: Option<Value>,
    wrap_info: Option<Value>,
    warnings: Option<Vec<String>>,
}
#[derive(Deserialize, Serialize)]
struct VersionMetadata {
    created_time: String,
    deletion_time: String,
    destroyed: bool,
    version: u64,
    custom_metadata: Option<BTreeMap<String, String>>,
}
#[derive(Deserialize, Serialize)]
struct Read {
    data: BTreeMap<String, Value>,
    metadata: VersionMetadata,
}
#[derive(Deserialize, Serialize)]
struct CreatedBy {
    actor: Option<String>,
    operation: Option<String>,
    entity_id: Option<String>,
    client_id: Option<String>,
}
#[derive(Deserialize, Serialize)]
struct VersionEntry {
    created_time: String,
    deletion_time: String,
    destroyed: bool,
    created_by: Option<CreatedBy>,
}
#[derive(Deserialize, Serialize)]
struct Metadata {
    created_time: String,
    updated_time: String,
    current_version: u64,
    oldest_version: u64,
    max_versions: u64,
    cas_required: bool,
    delete_version_after: String,
    custom_metadata: Option<BTreeMap<String, String>>,
    versions: BTreeMap<String, VersionEntry>,
}
#[derive(Deserialize, Serialize)]
struct Keys {
    keys: Vec<String>,
}
#[derive(Deserialize, Serialize)]
struct Authentication {
    #[serde(skip_serializing)]
    client_token: String,
    accessor: Option<String>,
    policies: Vec<String>,
    token_policies: Option<Vec<String>>,
    metadata: Option<BTreeMap<String, String>>,
    lease_duration: u64,
    renewable: bool,
    entity_id: Option<String>,
    token_type: String,
    orphan: Option<bool>,
    num_uses: Option<u64>,
    #[serde(skip_serializing)]
    mfa_requirement: Option<Value>,
}
fn typed<T: serde::de::DeserializeOwned + Serialize>(v: Value) -> Result<Value> {
    Ok(serde_json::to_value(
        serde_json::from_value::<T>(v).context("invalid Vault response schema")?,
    )?)
}
fn timestamp(value: &str, empty: bool) -> Result<()> {
    if empty && value.is_empty() {
        return Ok(());
    }
    chrono::DateTime::parse_from_rfc3339(value).context("invalid Vault RFC3339 timestamp")?;
    Ok(())
}
fn version(v: &Value) -> Result<()> {
    ensure!(
        v["version"].as_u64().context("version missing")? > 0,
        "secret version must be positive"
    );
    timestamp(v["created_time"].as_str().unwrap(), false)?;
    timestamp(v["deletion_time"].as_str().unwrap(), true)?;
    Ok(())
}
pub fn parse(operation: &str, value: Value) -> Result<(Value, Option<hyper::header::HeaderValue>)> {
    match operation {
        "seal_status" => {
            let v = typed::<SealStatus>(value)?;
            ensure!(
                v["t"].as_u64().unwrap() <= v["n"].as_u64().unwrap()
                    && v["progress"].as_u64().unwrap() <= v["n"].as_u64().unwrap(),
                "invalid seal threshold/progress"
            );
            Ok((v, None))
        }
        "health" => Ok((typed::<Health>(value)?, None)),
        "leader" => Ok((typed::<Leader>(value)?, None)),
        _ => {
            let e: Envelope = serde_json::from_value(value).context("invalid Vault envelope")?;
            ensure!(
                e.wrap_info.is_none(),
                "response wrapping is not implemented"
            );
            ensure!(
                e.lease_id.is_empty() && e.lease_duration == 0 && !e.renewable,
                "selected Vault envelope must not contain a dynamic lease"
            );
            let (data, credential) = if operation == "login" {
                ensure!(
                    e.data.is_null(),
                    "userpass login unexpectedly returned secret data"
                );
                let auth: Authentication =
                    serde_json::from_value(e.auth.context("missing login auth")?)
                        .context("invalid userpass auth response")?;
                ensure!(
                    auth.mfa_requirement.is_none(),
                    "userpass MFA continuation is not implemented"
                );
                ensure!(
                    ["service", "batch"].contains(&auth.token_type.as_str()),
                    "unsupported Vault token type"
                );
                let credential = token(&auth.client_token)?;
                (serde_json::to_value(auth)?, Some(credential))
            } else {
                ensure!(
                    e.auth.is_none(),
                    "KV v2 response must not contain authentication"
                );
                let data = match operation {
                    "read" => {
                        let v = typed::<Read>(e.data)?;
                        version(&v["metadata"])?;
                        ensure!(
                            v["metadata"]["destroyed"] == false
                                && v["metadata"]["deletion_time"] == "",
                            "unavailable secret version returned HTTP success"
                        );
                        v
                    }
                    "write" => {
                        let v = typed::<VersionMetadata>(e.data)?;
                        version(&v)?;
                        ensure!(
                            v["destroyed"] == false && v["deletion_time"] == "",
                            "write acknowledgement names unavailable version"
                        );
                        v
                    }
                    "list" => {
                        let v = typed::<Keys>(e.data)?;
                        for key in v["keys"].as_array().unwrap() {
                            let key = key.as_str().unwrap();
                            let name = key.strip_suffix('/').unwrap_or(key);
                            ensure!(
                                !name.is_empty()
                                    && name.len() <= 256
                                    && !name.contains('/')
                                    && !name.chars().any(char::is_control),
                                "invalid Vault list key"
                            );
                        }
                        v
                    }
                    "metadata" => {
                        let v = typed::<Metadata>(e.data)?;
                        timestamp(v["created_time"].as_str().unwrap(), false)?;
                        timestamp(v["updated_time"].as_str().unwrap(), false)?;
                        let current = v["current_version"].as_u64().unwrap();
                        ensure!(
                            v["oldest_version"].as_u64().unwrap() <= current,
                            "invalid oldest/current version"
                        );
                        for (key, entry) in v["versions"].as_object().unwrap() {
                            let n = key.parse::<u64>().context("invalid metadata version key")?;
                            ensure!(
                                n > 0 && n <= current && n.to_string() == *key,
                                "invalid metadata version range/key"
                            );
                            timestamp(entry["created_time"].as_str().unwrap(), false)?;
                            timestamp(entry["deletion_time"].as_str().unwrap(), true)?;
                        }
                        v
                    }
                    _ => anyhow::bail!("unsupported Vault response operation"),
                };
                (data, None)
            };
            Ok((
                json!({"request_id":e.request_id,"lease_id":e.lease_id,"lease_duration":e.lease_duration,"renewable":e.renewable,"warnings":e.warnings,"data":data}),
                credential,
            ))
        }
    }
}
pub fn errors(v: Value) -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct Errors {
        errors: Vec<String>,
    }
    Ok(serde_json::from_value::<Errors>(v)
        .context("invalid Vault error schema")?
        .errors)
}
struct SecretRedactor {
    forms: Vec<String>,
}
impl SecretRedactor {
    fn new(secrets: &[String]) -> Self {
        let mut forms = Vec::new();
        for secret in secrets.iter().filter(|s| !s.is_empty()) {
            // Schema errors may render values with JSON/Rust escapes. Compute
            // these bounded literal forms once for the entire display copy.
            let json = serde_json::to_string(secret).unwrap();
            let debug = format!("{secret:?}");
            forms.extend([
                secret.clone(),
                json[1..json.len() - 1].to_owned(),
                debug[1..debug.len() - 1].to_owned(),
            ]);
        }
        forms.sort_unstable();
        forms.dedup();
        Self { forms }
    }
    fn text(&self, text: &str) -> String {
        // Omit the whole reflected string: replacing a short password first
        // could expose an overlapping token's suffix, or rescan the marker.
        // Typed status/category fields remain available separately.
        if self.forms.iter().any(|form| text.contains(form)) {
            crate::utils::redact::REDACTED.to_owned()
        } else {
            text.to_owned()
        }
    }
    fn value(&self, value: &mut Value, redact_keys: bool, depth: usize) {
        if depth > MAX_DEPTH {
            // Programmatically constructed handler values need the same bound
            // as wire JSON. Drop omitted subtrees iteratively as well.
            let omitted = std::mem::replace(
                value,
                Value::String(crate::utils::redact::REDACTED.to_owned()),
            );
            crate::utils::json_budget::drop_iteratively(omitted);
            return;
        }
        match value {
            Value::String(s) => *s = self.text(s),
            Value::Array(a) => {
                for v in a {
                    self.value(v, redact_keys, depth + 1)
                }
            }
            Value::Object(o) => {
                // This is a display copy; the request keeps its real keys.
                let original = std::mem::take(o);
                for (key, mut value) in original {
                    self.value(&mut value, redact_keys, depth + 1);
                    o.insert(if redact_keys { self.text(&key) } else { key }, value);
                }
            }
            _ => {}
        }
    }
}
pub fn redact_text(text: &str, secrets: &[String]) -> String {
    SecretRedactor::new(secrets).text(text)
}
pub fn redact_errors(errors: &[String], secrets: &[String]) -> Vec<String> {
    let redactor = SecretRedactor::new(secrets);
    errors.iter().map(|error| redactor.text(error)).collect()
}
fn redact_schema(value: &mut Value, secrets: &[String], free_form_paths: &[&str]) {
    let redactor = SecretRedactor::new(secrets);
    // Fixed schema names are not credential reflections. Preserve their shape
    // even when a legitimate one-character password also occurs in a name.
    redactor.value(value, false, 0);
    for path in free_form_paths {
        if let Some(value) = value.pointer_mut(path) {
            redactor.value(value, true, path.matches('/').count());
        }
    }
}
pub fn redact_request(value: &mut Value, secrets: &[String]) {
    redact_schema(value, secrets, &["/data"])
}
pub fn redact_response(value: &mut Value, operation: &str, secrets: &[String]) {
    let paths: &[&str] = match operation {
        "login" => &["/data/metadata"],
        "read" => &["/data/data", "/data/metadata/custom_metadata"],
        "metadata" => &["/data/custom_metadata"],
        _ => &[],
    };
    redact_schema(value, secrets, paths)
}

struct Seed<'a> {
    depth: usize,
    nodes: &'a mut usize,
}
impl<'de> serde::de::DeserializeSeed<'de> for Seed<'_> {
    type Value = Value;
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        d: D,
    ) -> std::result::Result<Value, D::Error> {
        use serde::de::Error;
        *self.nodes += 1;
        if self.depth > MAX_DEPTH || *self.nodes > MAX_NODES {
            return Err(D::Error::custom("Vault JSON nesting/node limit"));
        }
        d.deserialize_any(self)
    }
}
impl<'de> serde::de::Visitor<'de> for Seed<'_> {
    type Value = Value;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("bounded Vault JSON")
    }
    fn visit_bool<E: serde::de::Error>(self, v: bool) -> std::result::Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<Value, E> {
        serde_json::Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("invalid JSON number"))
    }
    fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<Value, E> {
        if v.len() > MAX_TEXT {
            return Err(E::custom("Vault text limit"));
        }
        Ok(Value::String(v.into()))
    }
    fn visit_string<E: serde::de::Error>(self, v: String) -> std::result::Result<Value, E> {
        if v.len() > MAX_TEXT {
            return Err(E::custom("Vault text limit"));
        }
        Ok(Value::String(v))
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(
        self,
        mut a: A,
    ) -> std::result::Result<Value, A::Error> {
        use serde::de::Error;
        let mut values = Vec::new();
        while let Some(v) = a.next_element_seed(Seed {
            depth: self.depth + 1,
            nodes: self.nodes,
        })? {
            if values.len() >= MAX_ITEMS {
                return Err(A::Error::custom("Vault array limit"));
            }
            values.push(v);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(
        self,
        mut a: A,
    ) -> std::result::Result<Value, A::Error> {
        use serde::de::Error;
        let mut values = serde_json::Map::new();
        while let Some(key) = a.next_key::<String>()? {
            if key.len() > 256 || values.len() >= 256 {
                return Err(A::Error::custom("Vault object field/name limit"));
            }
            if values.contains_key(&key) {
                return Err(A::Error::custom("duplicate Vault JSON field"));
            }
            let value = a.next_value_seed(Seed {
                depth: self.depth + 1,
                nodes: self.nodes,
            })?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}
pub fn json(body: &[u8]) -> Result<Value> {
    ensure!(body.len() <= MAX_BODY, "Vault body limit");
    use serde::de::DeserializeSeed;
    let mut decoder = serde_json::Deserializer::from_slice(body);
    let value = Seed {
        depth: 0,
        nodes: &mut 0,
    }
    .deserialize(&mut decoder)
    .context("invalid Vault JSON")?;
    decoder.end().context("trailing Vault JSON content")?;
    Ok(value)
}
