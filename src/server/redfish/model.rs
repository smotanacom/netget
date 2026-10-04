//! Redfish envelope rules shared by the service and the client: paths, `@odata.id` and
//! `@odata.type`, collections, Base-registry errors and Task resources.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};

pub const ROOT: &str = "/redfish/v1/";
pub const REDFISH_VERSION: &str = "1.20.0";
pub const BASE_REGISTRY: &str = "Base.1.19.0";
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_PATH: usize = 512;

pub fn budget_ok(v: &Value) -> bool {
    crate::utils::json_budget::within_budget(v, MAX_BODY_BYTES, 50_000, 32)
}

/// A request path under `/redfish/v1`, without query and without a trailing slash (except the
/// service root, which is always `/redfish/v1/`). `None` for anything else.
pub fn normalize(path: &str) -> Option<String> {
    let path = path.split(['?', '#']).next().unwrap_or("");
    if path.len() > MAX_PATH
        || !path.starts_with("/redfish/v1")
        || path.chars().any(|c| c.is_control() || c == '\\')
        || path.split('/').any(|seg| seg == ".." || seg == ".")
        || path.contains("//")
    {
        return None;
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed == "/redfish/v1" {
        return Some(ROOT.to_owned());
    }
    trimmed
        .strip_prefix("/redfish/v1/")
        .map(|_| trimmed.to_owned())
}

fn same_path(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// `#Namespace.vX_Y_Z.Type`, or `#XCollection.XCollection` for an unversioned collection.
pub fn valid_type(t: &str) -> bool {
    let Some(rest) = t.strip_prefix('#') else {
        return false;
    };
    let parts: Vec<&str> = rest.split('.').collect();
    let ident =
        |s: &str| !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric());
    match parts.as_slice() {
        [ns, ty] => ident(ns) && ident(ty),
        [ns, ver, ty] => {
            ident(ns)
                && ident(ty)
                && ver.strip_prefix('v').is_some_and(|v| {
                    let n: Vec<&str> = v.split('_').collect();
                    n.len() == 3
                        && n.iter()
                            .all(|x| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit()))
                })
        }
        _ => false,
    }
}

pub fn is_collection(resource: &Value) -> bool {
    resource["@odata.type"]
        .as_str()
        .is_some_and(|t| t.ends_with("Collection"))
}

/// Check a resource the handler supplied for `path`: an object whose `@odata.id` is the path
/// (filled in when absent), a well-formed `@odata.type`, `Id` and `Name` on a resource, and
/// `Members` of links on a collection, whose count Rust writes.
pub fn check_resource(path: &str, resource: &mut Value) -> Result<()> {
    ensure!(budget_ok(resource), "resource exceeds the Redfish bounds");
    let obj = resource
        .as_object_mut()
        .context("a Redfish resource is a JSON object")?;
    match obj.get("@odata.id") {
        None => {
            obj.insert("@odata.id".into(), json!(path));
        }
        Some(Value::String(id)) => {
            ensure!(
                same_path(id, path),
                "@odata.id {id} is not the requested {path}"
            )
        }
        Some(_) => bail!("@odata.id must be a string"),
    }
    let ty = obj
        .get("@odata.type")
        .and_then(Value::as_str)
        .context("@odata.type is required")?
        .to_owned();
    ensure!(
        valid_type(&ty),
        "@odata.type {ty} is not #Namespace.vX_Y_Z.Type"
    );
    ensure!(
        obj.get("Name").is_some_and(Value::is_string),
        "Name is required"
    );
    if ty.ends_with("Collection") {
        let members = obj
            .get("Members")
            .and_then(Value::as_array)
            .context("a collection needs Members")?;
        for m in members {
            ensure!(
                m["@odata.id"]
                    .as_str()
                    .is_some_and(|id| normalize(id).is_some()),
                "each member is {{\"@odata.id\": \"/redfish/v1/...\"}}"
            );
        }
        let n = members.len();
        obj.insert("Members@odata.count".into(), json!(n));
    } else {
        ensure!(
            obj.get("Id").is_some_and(Value::is_string),
            "Id is required"
        );
    }
    Ok(())
}

/// Handler-facing names of the errors a handler may answer with: (name, status, MessageId,
/// message).
pub const ERRORS: &[(&str, u16, &str, &str)] = &[
    ("resource_not_found", 404, "ResourceMissingAtURI", "The resource at the URI was not found."),
    ("property_not_writable", 400, "PropertyNotWritable", "A property in the request cannot be written."),
    ("property_unknown", 400, "PropertyUnknown", "A property in the request is not known to this resource."),
    ("property_value_not_in_list", 400, "PropertyValueNotInList", "A property value is not one of the allowed values."),
    ("action_not_supported", 400, "ActionNotSupported", "The action is not supported by this resource."),
    ("action_parameter_missing", 400, "ActionParameterMissing", "The action requires a parameter that was not supplied."),
    ("action_parameter_value_not_in_list", 400, "ActionParameterValueNotInList", "An action parameter value is not one of the allowed values."),
    ("insufficient_privilege", 403, "InsufficientPrivilege", "There are insufficient privileges for the account or credentials associated with the current session to perform the requested operation."),
    ("operation_not_allowed", 405, "OperationNotAllowed", "The HTTP method is not allowed on this resource."),
    ("resource_in_use", 409, "ResourceInUse", "The change could not be made because the resource is in use or in transition."),
    ("resource_already_exists", 409, "ResourceAlreadyExists", "The resource could not be created because it already exists."),
    ("service_temporarily_unavailable", 503, "ServiceTemporarilyUnavailable", "The service is temporarily unavailable."),
    ("general_error", 500, "GeneralError", "A general error has occurred."),
];

pub fn error_named(name: &str) -> Option<(u16, &'static str, &'static str)> {
    ERRORS
        .iter()
        .find(|(n, ..)| *n == name)
        .map(|(_, s, id, m)| (*s, *id, *m))
}

/// A Redfish error body (DSP0266 §9.6) carrying one Base-registry message.
pub fn error_body(message_id: &str, message: &str) -> Value {
    let id = format!("{BASE_REGISTRY}.{message_id}");
    json!({"error": {
        "code": id,
        "message": message,
        "@Message.ExtendedInfo": [{
            "@odata.type": "#Message.v1_2_1.Message",
            "MessageId": id,
            "Message": message,
            "Severity": "Critical",
            "MessageSeverity": "Critical",
            "Resolution": "None."
        }]
    }})
}

pub const TASK_STATES: &[&str] = &[
    "New",
    "Starting",
    "Running",
    "Suspended",
    "Interrupted",
    "Pending",
    "Stopping",
    "Completed",
    "Killed",
    "Exception",
    "Service",
    "Cancelling",
    "Cancelled",
];

pub fn task_finished(state: &str) -> bool {
    matches!(state, "Completed" | "Killed" | "Exception" | "Cancelled")
}

pub fn task_resource(
    id: &str,
    state: &str,
    percent: u64,
    messages: &[Value],
    start: &str,
) -> Value {
    json!({
        "@odata.id": format!("/redfish/v1/TaskService/Tasks/{id}"),
        "@odata.type": "#Task.v1_7_3.Task",
        "Id": id,
        "Name": format!("Task {id}"),
        "TaskState": state,
        "TaskStatus": if matches!(state, "Exception" | "Killed") { "Critical" } else { "OK" },
        "PercentComplete": percent,
        "StartTime": start,
        "Messages": messages,
        "TaskMonitor": format!("/redfish/v1/TaskService/TaskMonitors/{id}"),
    })
}

pub fn link(path: &str) -> Value {
    json!({"@odata.id": path})
}

pub fn collection(path: &str, ty: &str, name: &str, members: Vec<Value>) -> Value {
    let n = members.len();
    json!({"@odata.id": path, "@odata.type": format!("#{ty}.{ty}"), "Name": name, "Members": members, "Members@odata.count": n})
}

/// The `Actions` target a client should POST to for `action` (e.g. `ComputerSystem.Reset`) on
/// a resource, if the resource advertises it.
pub fn action_target(resource: &Value, action: &str) -> Option<String> {
    resource["Actions"][format!("#{action}")]["target"]
        .as_str()
        .and_then(normalize)
}

/// Parse an action POST path: (`resource path`, `Namespace.Action`).
pub fn split_action(path: &str) -> Option<(String, String)> {
    let (resource, action) = path.rsplit_once("/Actions/")?;
    let ok = action.split('.').count() == 2
        && action
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.');
    ok.then(|| (resource.to_owned(), action.to_owned()))
}

pub fn messages_from(v: Option<&Value>) -> Result<Vec<Value>> {
    let Some(list) = v.filter(|v| !v.is_null()) else {
        return Ok(vec![]);
    };
    let list = list.as_array().context("messages must be an array")?;
    ensure!(list.len() <= 16, "at most 16 messages");
    list.iter()
        .map(|m| {
            let text = m
                .as_str()
                .or_else(|| m["Message"].as_str())
                .filter(|t| !t.is_empty() && t.len() <= 1024)
                .context("each message is a string of 1..1024 bytes")?;
            Ok(json!({"@odata.type": "#Message.v1_2_1.Message", "Message": text, "MessageId": format!("{BASE_REGISTRY}.Success"), "Severity": "OK", "MessageSeverity": "OK"}))
        })
        .collect()
}

pub fn object(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}
