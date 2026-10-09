//! Validate the selected gNMI RPC semantics, independently of protobuf framing.
use super::{
    proto::gnmi as pb,
    value::{self, Notification, Path, Update},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tonic::Status;
pub fn check_model(action: &Value) -> Result<(), Status> {
    fn walk(v: &Value, depth: usize, nodes: &mut usize, bytes: &mut usize) -> Result<(), Status> {
        *nodes += 1;
        *bytes += 64;
        if depth > 32 || *nodes > 10_000 || *bytes > 1024 * 1024 {
            return Err(Status::resource_exhausted(
                "gNMI model depth/node/retained bound",
            ));
        }
        match v {
            Value::String(s) => {
                *bytes += s.len();
                if s.len() > 65536 || *bytes > 1024 * 1024 {
                    return Err(Status::resource_exhausted("gNMI model text/retained bound"));
                }
            }
            Value::Array(v) => {
                for value in v {
                    walk(value, depth + 1, nodes, bytes)?;
                }
            }
            Value::Object(v) => {
                for (key, value) in v {
                    if key.len() > 256 {
                        return Err(Status::resource_exhausted("gNMI model key bound"));
                    }
                    *bytes += key.len();
                    walk(value, depth + 1, nodes, bytes)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    walk(action, 0, &mut 0, &mut 0)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    pub name: String,
    #[serde(default)]
    pub organization: String,
    #[serde(default)]
    pub version: String,
}
pub fn models(values: Vec<pb::ModelData>) -> Result<Vec<Model>, Status> {
    let models: Vec<_> = values
        .into_iter()
        .map(|v| Model {
            name: v.name,
            organization: v.organization,
            version: v.version,
        })
        .collect();
    model_proto(models.clone())?;
    Ok(models)
}
pub fn model_proto(values: Vec<Model>) -> Result<Vec<pb::ModelData>, Status> {
    if values.len() > 128 {
        return Err(Status::resource_exhausted("model count exceeds 128"));
    }
    values
        .into_iter()
        .map(|v| {
            if v.name.is_empty()
                || [&v.name, &v.organization, &v.version]
                    .iter()
                    .any(|s| s.len() > 256 || s.contains('\0'))
            {
                return Err(Status::invalid_argument("invalid model name/text"));
            }
            Ok(pb::ModelData {
                name: v.name,
                organization: v.organization,
                version: v.version,
            })
        })
        .collect()
}
pub fn get_request(request: &pb::GetRequest) -> Result<Value, Status> {
    value::encoding_name(request.encoding)?;
    let kind = match request.r#type {
        0 => "ALL",
        1 => "CONFIG",
        2 => "STATE",
        3 => "OPERATIONAL",
        _ => return Err(Status::invalid_argument("unknown Get data type")),
    };
    if request.path.len() > 128 || !request.extension.is_empty() {
        return Err(Status::unimplemented(
            "Get paths/extensions outside selected scope",
        ));
    }
    Ok(
        json!({"prefix":Path::from_proto(request.prefix.clone())?,"path":request.path.iter().cloned().map(|p|Path::from_proto(Some(p))).collect::<Result<Vec<_>,_>>()?,"data_type":kind,"encoding":value::encoding_name(request.encoding)?,"use_models":models(request.use_models.clone())?}),
    )
}
pub fn set_request(request: &pb::SetRequest) -> Result<Value, Status> {
    if !request.union_replace.is_empty() || !request.extension.is_empty() {
        return Err(Status::unimplemented(
            "union_replace and extensions are excluded",
        ));
    }
    if request.delete.len() + request.replace.len() + request.update.len() > 256 {
        return Err(Status::resource_exhausted("Set operations exceed 256"));
    }
    let convert = |values: &[pb::Update]| {
        values
            .iter()
            .cloned()
            .map(Update::from_proto)
            .collect::<Result<Vec<_>, _>>()
    };
    Ok(
        json!({"prefix":Path::from_proto(request.prefix.clone())?,"delete":request.delete.iter().cloned().map(|p|Path::from_proto(Some(p))).collect::<Result<Vec<_>,_>>()?,"replace":convert(&request.replace)?,"update":convert(&request.update)?}),
    )
}
pub fn set_response(request: &pb::SetRequest, timestamp: &str) -> Result<pb::SetResponse, Status> {
    set_request(request)?;
    let mut results = Vec::new();
    for (paths, operation) in [
        (request.delete.clone(), 1),
        (
            request
                .replace
                .iter()
                .map(|v| v.path.clone().unwrap_or_default())
                .collect(),
            2,
        ),
        (
            request
                .update
                .iter()
                .map(|v| v.path.clone().unwrap_or_default())
                .collect(),
            3,
        ),
    ] {
        for path in paths {
            results.push(pb::UpdateResult {
                timestamp: 0,
                path: Some(path),
                message: None,
                op: operation,
            });
        }
    }
    value::checked(pb::SetResponse {
        prefix: request.prefix.clone(),
        response: results,
        message: None,
        timestamp: timestamp.parse().map_err(|_| {
            Status::invalid_argument("timestamp must be i64 nanosecond decimal string")
        })?,
        extension: vec![],
    })
}
pub fn set_result(response: pb::SetResponse) -> Result<Value, Status> {
    if response.message.is_some() || !response.extension.is_empty() {
        return Err(Status::unimplemented(
            "legacy errors/extensions are excluded",
        ));
    }
    let result = response
        .response
        .into_iter()
        .map(|r| {
            if r.message.is_some() {
                return Err(Status::unimplemented("legacy update errors are excluded"));
            }
            let operation = match r.op {
                1 => "DELETE",
                2 => "REPLACE",
                3 => "UPDATE",
                _ => {
                    return Err(Status::invalid_argument(
                        "unsupported Set acknowledgement operation",
                    ))
                }
            };
            Ok(json!({"path":Path::from_proto(r.path)?,"operation":operation}))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(
        json!({"prefix":Path::from_proto(response.prefix)?,"timestamp":response.timestamp.to_string(),"response":result}),
    )
}
pub fn check_set_ack(request: &pb::SetRequest, response: &pb::SetResponse) -> Result<(), Status> {
    let expected = set_response(request, "0")?;
    if Path::from_proto(response.prefix.clone())? != Path::from_proto(expected.prefix)?
        || response.response.len() != expected.response.len()
    {
        return Err(Status::invalid_argument(
            "Set response does not acknowledge the requested transaction",
        ));
    }
    for (actual, expected) in response.response.iter().zip(expected.response) {
        if actual.op != expected.op
            || Path::from_proto(actual.path.clone())? != Path::from_proto(expected.path)?
        {
            return Err(Status::invalid_argument(
                "Set response does not acknowledge the requested transaction",
            ));
        }
    }
    Ok(())
}
pub fn get_result(response: pb::GetResponse, encoding: i32) -> Result<Value, Status> {
    if response.error.is_some() || !response.extension.is_empty() {
        return Err(Status::unimplemented(
            "legacy errors/extensions are excluded",
        ));
    }
    for n in &response.notification {
        value::notification_encoding(n, encoding)?;
    }
    Ok(
        json!({"notification":response.notification.into_iter().map(Notification::from_proto).collect::<Result<Vec<_>,_>>()?}),
    )
}
pub fn capability_result(response: pb::CapabilityResponse) -> Result<Value, Status> {
    if !response.extension.is_empty() {
        return Err(Status::unimplemented("capability extensions are excluded"));
    }
    if response.g_nmi_version.len() > 256 {
        return Err(Status::resource_exhausted("gNMI version bound"));
    }
    let encodings = response
        .supported_encodings
        .into_iter()
        .map(value::encoding_name)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(
        json!({"supported_models":models(response.supported_models)?,"supported_encodings":encodings,"gnmi_version":response.g_nmi_version}),
    )
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Once,
    Poll,
    Stream,
}
#[derive(Clone)]
pub struct Subscription {
    pub mode: Mode,
    pub encoding: i32,
    pub updates_only: bool,
    pub event: Value,
}
pub fn subscription(list: &pb::SubscriptionList) -> Result<Subscription, Status> {
    value::encoding_name(list.encoding)?;
    // gNMIc explicitly encodes marking0 when --qos0 disables marking. It is
    // the ordinary default DSCP behavior; nonzero marking is outside this subset.
    if list.qos.as_ref().is_some_and(|q| q.marking != 0) || list.allow_aggregation {
        return Err(Status::unimplemented(
            "nonzero QoS and aggregation are excluded",
        ));
    }
    if list.subscription.is_empty() || list.subscription.len() > 128 {
        return Err(Status::invalid_argument(
            "subscription requires 1..128 paths",
        ));
    }
    let mode = match list.mode {
        0 => Mode::Stream,
        1 => Mode::Once,
        2 => Mode::Poll,
        _ => return Err(Status::invalid_argument("unknown subscription list mode")),
    };
    let paths=list.subscription.iter().map(|s|{
        if !(0..=1).contains(&s.mode)||s.sample_interval!=0||s.suppress_redundant||s.heartbeat_interval!=0{return Err(Status::unimplemented("SAMPLE, suppression and heartbeat are excluded"));}
        Ok(json!({"path":Path::from_proto(s.path.clone())?,"mode":if s.mode==0{"TARGET_DEFINED"}else{"ON_CHANGE"}}))
    }).collect::<Result<Vec<_>,_>>()?;
    let event = json!({"prefix":Path::from_proto(list.prefix.clone())?,"subscription":paths,"mode":match mode{Mode::Once=>"ONCE",Mode::Poll=>"POLL",Mode::Stream=>"STREAM"},"encoding":value::encoding_name(list.encoding)?,"updates_only":list.updates_only,"use_models":models(list.use_models.clone())?});
    Ok(Subscription {
        mode,
        encoding: list.encoding,
        updates_only: list.updates_only,
        event,
    })
}
pub enum Answer {
    Capabilities(pb::CapabilityResponse),
    Get(Vec<pb::Notification>),
    Set(String),
    Update(Vec<pb::Notification>),
    Sync,
    Wait(u64),
    Finish,
    Error(Status),
}
fn decode<T: serde::de::DeserializeOwned>(v: &Value) -> Result<T, Status> {
    serde_json::from_value(v.clone())
        .map_err(|_| Status::invalid_argument("invalid typed gNMI action"))
}
fn notifications(v: &Value) -> Result<Vec<pb::Notification>, Status> {
    let values = v
        .as_array()
        .ok_or_else(|| Status::invalid_argument("notification must be an array"))?;
    if values.len() > 128 {
        return Err(Status::resource_exhausted("notification count exceeds 128"));
    }
    values
        .iter()
        .map(|v| decode::<Notification>(v)?.into_proto())
        .collect()
}
pub fn answer(action: &Value) -> Result<Answer, Status> {
    Ok(match action["type"].as_str().unwrap_or("") {
        "gnmi_capabilities" => {
            let model_values = action["supported_models"]
                .as_array()
                .ok_or_else(|| Status::invalid_argument("supported_models must be an array"))?;
            if model_values.len() > 128 {
                return Err(Status::resource_exhausted("model count exceeds 128"));
            }
            let model = model_proto(decode(&action["supported_models"])?)?;
            let encodings = action["supported_encodings"]
                .as_array()
                .ok_or_else(|| Status::invalid_argument("supported_encodings must be an array"))?;
            if encodings.is_empty() || encodings.len() > 4 {
                return Err(Status::invalid_argument(
                    "advertise 1..4 supported encodings",
                ));
            }
            let encodings = encodings
                .iter()
                .map(|v| value::encoding(v.as_str().unwrap_or("")))
                .collect::<Result<Vec<_>, _>>()?;
            let response = pb::CapabilityResponse {
                supported_models: model,
                supported_encodings: encodings,
                g_nmi_version: "0.10.0".into(),
                extension: vec![],
            };
            Answer::Capabilities(value::checked(response)?)
        }
        "gnmi_get_response" => {
            let n = notifications(&action["notification"])?;
            value::checked(pb::GetResponse {
                notification: n.clone(),
                error: None,
                extension: vec![],
            })?;
            Answer::Get(n)
        }
        "gnmi_set_accepted" => {
            let t = action["timestamp"]
                .as_str()
                .ok_or_else(|| Status::invalid_argument("timestamp required"))?;
            t.parse::<i64>()
                .map_err(|_| Status::invalid_argument("invalid timestamp"))?;
            Answer::Set(t.into())
        }
        "gnmi_update" => {
            let n = notifications(&action["notification"])?;
            if n.len() > 16 {
                return Err(Status::resource_exhausted(
                    "at most 16 queued notifications",
                ));
            }
            let mut bytes = 0usize;
            for v in &n {
                let response = value::checked(pb::SubscribeResponse {
                    response: Some(pb::subscribe_response::Response::Update(v.clone())),
                    extension: vec![],
                })?;
                bytes += prost::Message::encoded_len(&response);
                if bytes > super::codec::MAX_MESSAGE_BYTES {
                    return Err(Status::resource_exhausted(
                        "pending notifications exceed 1 MiB",
                    ));
                }
            }
            Answer::Update(n)
        }
        "gnmi_sync" => Answer::Sync,
        "gnmi_wait" => {
            let milliseconds = action["milliseconds"]
                .as_u64()
                .filter(|v| (1..=1000).contains(v))
                .ok_or_else(|| Status::invalid_argument("wait milliseconds must be 1..1000"))?;
            Answer::Wait(milliseconds)
        }
        "gnmi_finish" => Answer::Finish,
        "gnmi_error" => {
            let code = action["code"]
                .as_i64()
                .filter(|v| (1..=16).contains(v))
                .ok_or_else(|| Status::invalid_argument("error code must be 1..16"))?;
            let message = action["message"]
                .as_str()
                .ok_or_else(|| Status::invalid_argument("error message required"))?;
            if message.len() > 512 {
                return Err(Status::resource_exhausted(
                    "error diagnostic exceeds 512 bytes",
                ));
            }
            Answer::Error(Status::new(tonic::Code::from_i32(code as i32), message))
        }
        _ => return Err(Status::invalid_argument("unknown gNMI server action")),
    })
}
