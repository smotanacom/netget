//! OCPP-J RPC frames (CALL, CALLRESULT, CALLERROR) for OCPP 1.6 and 2.0.1, shared by both
//! roles, with the required-field checks for the core charging workflows.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};

pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
pub const MAX_ID_CHARS: usize = 36;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    V16,
    V201,
}

impl Version {
    pub fn subprotocol(self) -> &'static str {
        match self {
            Version::V16 => "ocpp1.6",
            Version::V201 => "ocpp2.0.1",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Version::V16 => "1.6",
            Version::V201 => "2.0.1",
        }
    }
    pub fn from_subprotocol(s: &str) -> Option<Self> {
        match s.trim() {
            "ocpp1.6" => Some(Version::V16),
            "ocpp2.0.1" => Some(Version::V201),
            _ => None,
        }
    }
    pub fn from_label(s: &str) -> Option<Self> {
        match s {
            "1.6" => Some(Version::V16),
            "2.0.1" => Some(Version::V201),
            _ => None,
        }
    }
    /// The error code for a missing required field, spelled as each version spells it.
    pub fn occurrence_code(self) -> &'static str {
        match self {
            Version::V16 => "OccurenceConstraintViolation",
            Version::V201 => "OccurrenceConstraintViolation",
        }
    }
    pub fn error_codes(self) -> &'static [&'static str] {
        match self {
            Version::V16 => &[
                "NotImplemented",
                "NotSupported",
                "InternalError",
                "ProtocolError",
                "SecurityError",
                "FormationViolation",
                "PropertyConstraintViolation",
                "OccurenceConstraintViolation",
                "TypeConstraintViolation",
                "GenericError",
            ],
            Version::V201 => &[
                "FormatViolation",
                "GenericError",
                "InternalError",
                "MessageTypeNotSupported",
                "NotImplemented",
                "NotSupported",
                "OccurrenceConstraintViolation",
                "PropertyConstraintViolation",
                "ProtocolError",
                "RpcFrameworkError",
                "SecurityError",
                "TypeConstraintViolation",
            ],
        }
    }
    /// Malformed frame / not a JSON RPC array.
    pub fn formation_code(self) -> &'static str {
        match self {
            Version::V16 => "FormationViolation",
            Version::V201 => "FormatViolation",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    Call {
        id: String,
        action: String,
        payload: Value,
    },
    Result {
        id: String,
        payload: Value,
    },
    Error {
        id: String,
        code: String,
        description: String,
        details: Value,
    },
}

fn id_ok(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty() && id.chars().count() <= MAX_ID_CHARS,
        "message id must be 1..=36 characters"
    );
    ensure!(
        !id.chars().any(|c| c.is_control()),
        "message id contains a control character"
    );
    Ok(())
}

fn action_ok(action: &str) -> Result<()> {
    ensure!(
        !action.is_empty()
            && action.len() <= 64
            && action.bytes().all(|b| b.is_ascii_alphanumeric()),
        "action must be 1..=64 ASCII letters or digits"
    );
    Ok(())
}

pub fn budget_ok(v: &Value) -> bool {
    crate::utils::json_budget::within_budget(v, MAX_MESSAGE_BYTES * 2, 8192, 32)
}

/// Parse one WebSocket text message. `Err` carries (message id if known, reason).
pub fn parse(text: &str) -> std::result::Result<Frame, (Option<String>, String)> {
    let fail = |id: Option<String>, why: &str| Err((id, why.to_owned()));
    if text.len() > MAX_MESSAGE_BYTES {
        return fail(None, "message exceeds 64 KiB");
    }
    let value: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return fail(None, "not JSON"),
    };
    if !budget_ok(&value) {
        return fail(None, "message nesting or size exceeds the bound");
    }
    let Some(items) = value.as_array() else {
        return fail(None, "an OCPP-J message is a JSON array");
    };
    let id = items.get(1).and_then(Value::as_str).map(str::to_owned);
    let Some(id_text) = id.clone() else {
        return fail(None, "message id must be a string");
    };
    if let Err(e) = id_ok(&id_text) {
        return fail(None, &e.to_string());
    }
    match items.first().and_then(Value::as_u64) {
        Some(2) => {
            if items.len() != 4 {
                return fail(id, "CALL has four elements");
            }
            let Some(action) = items[2].as_str() else {
                return fail(id, "CALL action must be a string");
            };
            if action_ok(action).is_err() {
                return fail(id, "invalid CALL action name");
            }
            if !items[3].is_object() {
                return fail(id, "CALL payload must be an object");
            }
            Ok(Frame::Call {
                id: id_text,
                action: action.to_owned(),
                payload: items[3].clone(),
            })
        }
        Some(3) => {
            if items.len() != 3 || !items[2].is_object() {
                return fail(id, "CALLRESULT is [3, id, {payload}]");
            }
            Ok(Frame::Result {
                id: id_text,
                payload: items[2].clone(),
            })
        }
        Some(4) => {
            if items.len() != 5 || !items[4].is_object() {
                return fail(id, "CALLERROR is [4, id, code, description, {details}]");
            }
            let (Some(code), Some(description)) = (items[2].as_str(), items[3].as_str()) else {
                return fail(id, "CALLERROR code and description must be strings");
            };
            Ok(Frame::Error {
                id: id_text,
                code: code.to_owned(),
                description: description.to_owned(),
                details: items[4].clone(),
            })
        }
        _ => fail(id, "message type must be 2, 3 or 4"),
    }
}

pub fn encode(frame: &Frame) -> Result<String> {
    let v = match frame {
        Frame::Call {
            id,
            action,
            payload,
        } => {
            id_ok(id)?;
            action_ok(action)?;
            ensure!(payload.is_object(), "payload must be an object");
            json!([2, id, action, payload])
        }
        Frame::Result { id, payload } => {
            id_ok(id)?;
            ensure!(payload.is_object(), "payload must be an object");
            json!([3, id, payload])
        }
        Frame::Error {
            id,
            code,
            description,
            details,
        } => {
            id_ok(id)?;
            ensure!(details.is_object(), "details must be an object");
            ensure!(
                description.len() <= 255,
                "error description exceeds 255 characters"
            );
            json!([4, id, code, description, details])
        }
    };
    let text = serde_json::to_string(&v)?;
    ensure!(
        text.len() <= MAX_MESSAGE_BYTES && budget_ok(&v),
        "message exceeds the OCPP-J bound"
    );
    Ok(text)
}

/// Required payload members of the selected core actions, (request, response).
fn required(
    version: Version,
    action: &str,
) -> Option<(&'static [&'static str], &'static [&'static str])> {
    Some(match (version, action) {
        (Version::V16, "BootNotification") => (
            &["chargePointVendor", "chargePointModel"],
            &["status", "currentTime", "interval"],
        ),
        (Version::V16, "Heartbeat") => (&[], &["currentTime"]),
        (Version::V16, "StatusNotification") => (&["connectorId", "errorCode", "status"], &[]),
        (Version::V16, "Authorize") => (&["idTag"], &["idTagInfo"]),
        (Version::V16, "StartTransaction") => (
            &["connectorId", "idTag", "meterStart", "timestamp"],
            &["transactionId", "idTagInfo"],
        ),
        (Version::V16, "StopTransaction") => (&["transactionId", "meterStop", "timestamp"], &[]),
        (Version::V16, "MeterValues") => (&["connectorId", "meterValue"], &[]),
        (Version::V16, "RemoteStartTransaction") => (&["idTag"], &["status"]),
        (Version::V16, "RemoteStopTransaction") => (&["transactionId"], &["status"]),
        (Version::V16, "Reset") => (&["type"], &["status"]),
        (Version::V16, "ChangeConfiguration") => (&["key", "value"], &["status"]),
        (Version::V16, "GetConfiguration") => (&[], &[]),
        (Version::V201, "BootNotification") => (
            &["chargingStation", "reason"],
            &["currentTime", "interval", "status"],
        ),
        (Version::V201, "Heartbeat") => (&[], &["currentTime"]),
        (Version::V201, "StatusNotification") => (
            &["timestamp", "connectorStatus", "evseId", "connectorId"],
            &[],
        ),
        (Version::V201, "Authorize") => (&["idToken"], &["idTokenInfo"]),
        (Version::V201, "TransactionEvent") => (
            &[
                "eventType",
                "timestamp",
                "triggerReason",
                "seqNo",
                "transactionInfo",
            ],
            &[],
        ),
        (Version::V201, "MeterValues") => (&["evseId", "meterValue"], &[]),
        (Version::V201, "RequestStartTransaction") => (&["idToken", "remoteStartId"], &["status"]),
        (Version::V201, "RequestStopTransaction") => (&["transactionId"], &["status"]),
        (Version::V201, "Reset") => (&["type"], &["status"]),
        (Version::V201, "SetVariables") => (&["setVariableData"], &["setVariableResult"]),
        (Version::V201, "GetVariables") => (&["getVariableData"], &["getVariableResult"]),
        _ => return None,
    })
}

/// Check a request payload; `Err` names the first missing member.
pub fn check_request(version: Version, action: &str, payload: &Value) -> Result<()> {
    check(version, action, payload, true)
}
/// Check a response payload to `action`.
pub fn check_response(version: Version, action: &str, payload: &Value) -> Result<()> {
    check(version, action, payload, false)
}
fn check(version: Version, action: &str, payload: &Value, request: bool) -> Result<()> {
    let obj: &Map<String, Value> = payload.as_object().context("payload must be an object")?;
    if let Some((req, res)) = required(version, action) {
        for key in if request { req } else { res } {
            ensure!(
                obj.contains_key(*key),
                "{action} {} requires '{key}'",
                if request { "request" } else { "response" }
            );
        }
    }
    Ok(())
}

pub fn is_core(version: Version, action: &str) -> bool {
    required(version, action).is_some()
}

/// A charge point identity from the URL path's last segment.
pub fn charge_point_id(path: &str) -> Result<String> {
    let last = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let id = percent_decode(last)?;
    ensure!(
        !id.is_empty() && id.len() <= 48,
        "charge point id must be 1..=48 characters"
    );
    ensure!(
        id.bytes().all(|b| b.is_ascii_graphic() && b != b'/'),
        "charge point id must be printable ASCII without '/'"
    );
    Ok(id)
}

fn percent_decode(text: &str) -> Result<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text
                .get(i + 1..i + 3)
                .context("truncated percent-encoding")?;
            out.push(u8::from_str_radix(hex, 16)?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).context("charge point id is not UTF-8")
}

pub fn error_frame(id: &str, code: &str, description: &str) -> String {
    encode(&Frame::Error {
        id: id.into(),
        code: code.into(),
        description: description.into(),
        details: json!({}),
    })
    .unwrap_or_else(|_| {
        // An id that cannot be echoed is answered with a fixed placeholder id.
        format!("[4,\"-1\",\"{code}\",\"invalid message\",{{}}]")
    })
}

pub fn validate_error_code(version: Version, code: &str) -> Result<()> {
    if !version.error_codes().contains(&code) {
        bail!("'{code}' is not an OCPP {} error code", version.label());
    }
    Ok(())
}
