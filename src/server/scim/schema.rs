//! The SCIM schemas NetGet serves: RFC 7643 §8.7's own representations of User, Group, the
//! Enterprise User extension, ServiceProviderConfig, ResourceType and Schema, embedded verbatim
//! (`rfc7643_schemas.json`), and the attribute characteristics the query engine needs.
use serde_json::{json, Value};
use std::sync::LazyLock;

pub const USER: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
pub const GROUP: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";
pub const ENTERPRISE: &str = "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User";
pub const LIST_RESPONSE: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
pub const SEARCH_REQUEST: &str = "urn:ietf:params:scim:api:messages:2.0:SearchRequest";
pub const PATCH_OP: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";
pub const ERROR: &str = "urn:ietf:params:scim:api:messages:2.0:Error";

pub static SCHEMAS: LazyLock<Vec<Value>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("rfc7643_schemas.json")).expect("embedded RFC 7643 schemas")
});

/// A resource type NetGet serves: name, endpoint, core schema, extension schemas.
pub struct ResourceType {
    pub name: &'static str,
    pub endpoint: &'static str,
    pub schema: &'static str,
    pub extensions: &'static [&'static str],
}

pub const RESOURCE_TYPES: &[ResourceType] = &[
    ResourceType {
        name: "User",
        endpoint: "/Users",
        schema: USER,
        extensions: &[ENTERPRISE],
    },
    ResourceType {
        name: "Group",
        endpoint: "/Groups",
        schema: GROUP,
        extensions: &[],
    },
];

pub fn by_endpoint(segment: &str) -> Option<&'static ResourceType> {
    RESOURCE_TYPES.iter().find(|r| &r.endpoint[1..] == segment)
}

pub fn by_name(name: &str) -> Option<&'static ResourceType> {
    RESOURCE_TYPES
        .iter()
        .find(|r| r.name.eq_ignore_ascii_case(name))
}

pub fn schema(urn: &str) -> Option<&'static Value> {
    SCHEMAS.iter().find(|s| {
        s["id"]
            .as_str()
            .is_some_and(|id| id.eq_ignore_ascii_case(urn))
    })
}

/// Case-insensitive object member lookup (SCIM attribute names are case-insensitive).
pub fn member<'a>(obj: &'a Value, name: &str) -> Option<&'a Value> {
    obj.as_object()?
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v)
}

pub fn member_key(obj: &Value, name: &str) -> Option<String> {
    obj.as_object()?
        .keys()
        .find(|k| k.eq_ignore_ascii_case(name))
        .cloned()
}

fn find_attr<'a>(attrs: &'a Value, name: &str) -> Option<&'a Value> {
    attrs.as_array()?.iter().find(|a| {
        a["name"]
            .as_str()
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
    })
}

static COMMON: LazyLock<Value> = LazyLock::new(|| {
    json!([
        {"name": "id", "type": "string", "caseExact": true, "returned": "always", "mutability": "readOnly", "multiValued": false},
        {"name": "externalId", "type": "string", "caseExact": true, "returned": "default", "mutability": "readWrite", "multiValued": false},
        {"name": "schemas", "type": "reference", "caseExact": true, "returned": "always", "multiValued": true},
        {"name": "meta", "type": "complex", "returned": "default", "mutability": "readOnly", "multiValued": false, "subAttributes": [
            {"name": "resourceType", "type": "string", "caseExact": true},
            {"name": "created", "type": "dateTime"},
            {"name": "lastModified", "type": "dateTime"},
            {"name": "location", "type": "reference", "caseExact": true},
            {"name": "version", "type": "string", "caseExact": true}
        ]}
    ])
});

/// The definition of `attr` (and `sub`), looked up in the extension `urn` when given, else in
/// the common attributes and the core schema.
pub fn attribute(
    rt: &ResourceType,
    urn: Option<&str>,
    attr: &str,
    sub: Option<&str>,
) -> Option<&'static Value> {
    let base = match urn {
        Some(u) if !u.eq_ignore_ascii_case(rt.schema) => {
            if !rt.extensions.iter().any(|e| e.eq_ignore_ascii_case(u)) {
                return None;
            }
            find_attr(&schema(u)?["attributes"], attr)?
        }
        _ => find_attr(&COMMON, attr)
            .or_else(|| find_attr(&schema(rt.schema)?["attributes"], attr))?,
    };
    match sub {
        None => Some(base),
        Some(s) => find_attr(&base["subAttributes"], s),
    }
}

pub fn case_exact(def: Option<&Value>) -> bool {
    def.and_then(|d| d["caseExact"].as_bool()).unwrap_or(false)
}

pub fn returned(def: Option<&Value>) -> &'static str {
    match def.and_then(|d| d["returned"].as_str()) {
        Some("always") => "always",
        Some("never") => "never",
        Some("request") => "request",
        _ => "default",
    }
}

/// Every top-level attribute name with `returned` = `wanted`, core and extension.
pub fn names_returned(rt: &ResourceType, wanted: &str) -> Vec<(Option<&'static str>, String)> {
    let mut out = Vec::new();
    for a in COMMON.as_array().into_iter().flatten() {
        if a["returned"] == wanted {
            out.push((None, a["name"].as_str().unwrap_or_default().to_owned()));
        }
    }
    let mut add = |urn: Option<&'static str>, s: &str| {
        for a in schema(s)
            .and_then(|s| s["attributes"].as_array())
            .into_iter()
            .flatten()
        {
            if a["returned"] == wanted {
                out.push((urn, a["name"].as_str().unwrap_or_default().to_owned()));
            }
        }
    };
    add(None, rt.schema);
    for e in rt.extensions {
        add(Some(e), e);
    }
    out
}

/// A served schema with its `meta.location` pointing at this service.
pub fn published(s: &Value, base: &str) -> Value {
    let mut s = s.clone();
    let id = s["id"].as_str().unwrap_or_default().to_owned();
    s["schemas"] = json!(["urn:ietf:params:scim:schemas:core:2.0:Schema"]);
    s["meta"] = json!({"resourceType": "Schema", "location": format!("{base}/Schemas/{id}")});
    s
}

pub fn resource_type_resource(rt: &ResourceType, base: &str) -> Value {
    json!({
        "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ResourceType"],
        "id": rt.name,
        "name": rt.name,
        "endpoint": rt.endpoint,
        "description": format!("{} accounts", rt.name),
        "schema": rt.schema,
        "schemaExtensions": rt.extensions.iter().map(|e| json!({"schema": e, "required": false})).collect::<Vec<_>>(),
        "meta": {"resourceType": "ResourceType", "location": format!("{base}/ResourceTypes/{}", rt.name)}
    })
}
