//! Action and event definitions CalDAV and CardDAV share, named per protocol (`caldav_*`,
//! `carddav_*`).
use super::server::Kind;
use crate::llm::actions::{ActionDefinition, Parameter, ParameterDefinition};
use crate::protocol::log_template::LogTemplate;
use crate::protocol::EventType;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};

pub fn parameter(name: &str, kind: &str, description: &str, required: bool) -> Parameter {
    Parameter {
        name: name.into(),
        type_hint: kind.into(),
        description: description.into(),
        required,
    }
}

pub fn action(
    name: &str,
    description: &str,
    parameters: Vec<Parameter>,
    example: Value,
) -> ActionDefinition {
    ActionDefinition {
        name: name.into(),
        description: description.into(),
        parameters,
        example,
        log_template: Some(LogTemplate::new().with_info(format!("-> {name}"))),
    }
}

fn sample(kind: Kind) -> (&'static str, &'static str, &'static str) {
    match kind {
        Kind::Calendar => ("event-1.ics", "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//NetGet//EN\r\nBEGIN:VEVENT\r\nUID:event-1\r\nDTSTAMP:20261005T090000Z\r\nDTSTART:20261005T100000Z\r\nDTEND:20261005T110000Z\r\nSUMMARY:Standup\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n", "calendar"),
        Kind::AddressBook => ("ada.vcf", "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:ada\r\nFN:Ada Lovelace\r\nEMAIL:ada@example.com\r\nEND:VCARD\r\n", "address book"),
    }
}

pub fn sync_actions(p: &str, kind: Kind) -> Vec<ActionDefinition> {
    let (name, data, noun) = sample(kind);
    let format = if kind == Kind::Calendar {
        "iCalendar (RFC 5545)"
    } else {
        "vCard (RFC 6350)"
    };
    vec![
        action(&format!("{p}_login_accept"), "Accept the Basic-auth credentials (remembered for 30 minutes)", vec![], json!({"type": format!("{p}_login_accept")})),
        action(&format!("{p}_login_reject"), "Refuse the credentials: the client gets 401", vec![parameter("reason", "string", "Why, for the log only", false)], json!({"type": format!("{p}_login_reject"), "reason": "unknown user"})),
        action(
            &format!("{p}_collections"),
            &format!("Answer list_collections with the user's {noun}s"),
            vec![parameter("collections", "array", &format!("[{{name, displayname, description, color, components}}]; name is the URL segment, components (calendars only) like [\"VEVENT\", \"VTODO\"]"), true)],
            json!({"type": format!("{p}_collections"), "collections": [{"name": "work", "displayname": "Work"}]}),
        ),
        action(
            &format!("{p}_objects"),
            &format!("Answer list_objects with the {noun}'s objects; give data whenever with_data is true (Rust filters REPORT queries over it)"),
            vec![parameter("objects", "array", &format!("[{{name, etag, data}}]: name is the file name ({name}), data the {format} text, etag optional (Rust hashes the data)"), true)],
            json!({"type": format!("{p}_objects"), "objects": [{"name": name, "data": data}]}),
        ),
        action(
            &format!("{p}_object"),
            "Answer get with the object",
            vec![parameter("data", "string", &format!("The {format} text"), true), parameter("etag", "string", "Its entity tag; omitted, Rust hashes the data", false)],
            json!({"type": format!("{p}_object"), "data": data}),
        ),
        action(
            &format!("{p}_stored"),
            "Accept a put: the object is stored as sent. Check if_match / if_none_match against what you hold first and answer the error with status 412 when they fail.",
            vec![parameter("etag", "string", "The new entity tag; omitted, Rust hashes the data", false), parameter("created", "boolean", "True when the name was new (201), false for an update (204)", false)],
            json!({"type": format!("{p}_stored"), "created": true}),
        ),
        action(&format!("{p}_done"), "Succeed: a delete, make_collection or proppatch was carried out", vec![], json!({"type": format!("{p}_done")})),
        action(
            &format!("{p}_error"),
            "Refuse the request",
            vec![
                parameter("status", "number", "403, 404 (no such object or collection), 409, 412 (precondition failed), 415 or 507", true),
                parameter("precondition", "string", "Optional DAV precondition element, e.g. no-uid-conflict", false),
                parameter("message", "string", "Plain-text explanation", false),
            ],
            json!({"type": format!("{p}_error"), "status": 404, "message": "no such object"}),
        ),
    ]
}

fn find(all: &[ActionDefinition], name: &str) -> ActionDefinition {
    all.iter()
        .find(|a| a.name == name)
        .cloned()
        .expect("defined above")
}

pub fn login_event(p: &str, kind: Kind) -> EventType {
    let all = sync_actions(p, kind);
    EventType::new(
        &format!("{p}_login"),
        "A client presents HTTP Basic credentials. Accept or reject.",
        json!({"type": format!("{p}_login_accept")}),
    )
    .with_parameters(vec![
        parameter(
            "user_name",
            "string",
            "The user name sent; it is also the URL segment of the user's principal and home",
            true,
        ),
        parameter(
            "password",
            "string",
            "The password sent, in the clear",
            true,
        ),
    ])
    .with_actions(vec![
        find(&all, &format!("{p}_login_accept")),
        find(&all, &format!("{p}_login_reject")),
    ])
}

pub fn request_event(p: &str, kind: Kind) -> EventType {
    let all = sync_actions(p, kind);
    let noun = if kind == Kind::Calendar {
        "calendar"
    } else {
        "address book"
    };
    EventType::new(&format!("{p}_request"), &format!("A request for the data you own. Discovery, PROPFIND, REPORT filtering and validation are Rust's; you answer per operation: list_collections → {p}_collections, list_objects → {p}_objects, get → {p}_object, put → {p}_stored, delete/make_collection/proppatch → {p}_done, or {p}_error."), json!({"type": format!("{p}_collections"), "collections": [{"name": "work"}]}))
        .with_parameters(vec![
            parameter("operation", "string", "list_collections, list_objects, get, put, delete, make_collection or proppatch", true),
            parameter("user", "string", "The authenticated user whose home is addressed", false),
            parameter("collection", "string", &format!("The {noun}'s URL segment"), false),
            parameter("name", "string", "The object's file name (absent when a delete targets the whole collection)", false),
            parameter("data", "string", "For put: the validated object text", false),
            parameter("uid", "string", "For put: the object's UID", false),
            parameter("component", "string", "For put: VEVENT, VTODO, VJOURNAL or VCARD", false),
            parameter("if_match", "string", "If-Match header the client sent (an ETag or *)", false),
            parameter("if_none_match", "string", "If-None-Match header the client sent (* for create-only)", false),
            parameter("with_data", "boolean", "For list_objects: whether object data is needed", false),
            parameter("displayname", "string", "For make_collection: the requested display name", false),
        ])
        .with_actions(all.into_iter().filter(|a| !a.name.contains("login")).collect())
}

pub fn startup_parameters(default_user: &str, default_auth: &str) -> Vec<ParameterDefinition> {
    vec![
        ParameterDefinition {
            name: "auth".into(),
            type_hint: "string".into(),
            description: "required (HTTP Basic, decided by the login event) or none".into(),
            required: false,
            example: json!("none"),
            default: Some(json!(default_auth)),
        },
        ParameterDefinition {
            name: "default_user".into(),
            type_hint: "string".into(),
            description: "User whose principal and home are served when auth is none".into(),
            required: false,
            example: json!("alice"),
            default: Some(json!(default_user)),
        },
    ]
}

pub const PRECONDITIONS: &[&str] = &[
    "no-uid-conflict",
    "valid-calendar-data",
    "valid-address-data",
    "supported-calendar-component",
    "lock-token-submitted",
    "need-privileges",
    "resource-must-be-null",
];

pub fn check_answer(p: &str, v: &Value) -> Result<()> {
    ensure!(
        crate::utils::json_budget::within_budget(v, 8 * 1024 * 1024, 200_000, 16),
        "answer exceeds the DAV bounds"
    );
    let ty = v["type"].as_str().unwrap_or_default();
    let suffix = ty
        .strip_prefix(&format!("{p}_"))
        .context("action belongs to another protocol")?;
    match suffix {
        "login_accept" | "login_reject" | "done" => {}
        "collections" => ensure!(
            v["collections"].as_array().is_some_and(|l| l
                .iter()
                .all(|c| c["name"].as_str().is_some_and(super::server::segment_ok))),
            "collections need names that are URL segments"
        ),
        "objects" => ensure!(
            v["objects"].as_array().is_some_and(|l| l
                .iter()
                .all(|o| o["name"].as_str().is_some_and(super::server::segment_ok))),
            "objects need names that are URL segments"
        ),
        "object" => ensure!(v["data"].is_string(), "data is required"),
        "stored" => {}
        "error" => {
            ensure!(
                v["status"]
                    .as_u64()
                    .is_some_and(|s| matches!(s, 403 | 404 | 409 | 412 | 415 | 507)),
                "status must be 403, 404, 409, 412, 415 or 507"
            );
            if let Some(c) = v.get("precondition").filter(|c| !c.is_null()) {
                ensure!(
                    c.as_str().is_some_and(|c| PRECONDITIONS.contains(&c)),
                    "unknown precondition"
                );
            }
        }
        _ => bail!("unknown action {ty}"),
    }
    Ok(())
}
