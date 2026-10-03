use crate::server::gnmi::{
    proto::gnmi as pb,
    semantic::{self, Model},
    value::{self, Path, Update},
};
use serde::Deserialize;
use serde_json::Value;
use tonic::Status;
fn proto() -> String {
    "PROTO".into()
}
fn all() -> String {
    "ALL".into()
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Get {
    #[serde(default)]
    prefix: Path,
    #[serde(default)]
    path: Vec<Path>,
    #[serde(default = "all")]
    data_type: String,
    #[serde(default = "proto")]
    encoding: String,
    #[serde(default)]
    use_models: Vec<Model>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Set {
    #[serde(default)]
    prefix: Path,
    #[serde(default)]
    delete: Vec<Path>,
    #[serde(default)]
    replace: Vec<Update>,
    #[serde(default)]
    update: Vec<Update>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Subscription {
    path: Path,
    #[serde(default)]
    mode: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Subscribe {
    #[serde(default)]
    prefix: Path,
    mode: String,
    subscription: Vec<Subscription>,
    #[serde(default = "proto")]
    encoding: String,
    #[serde(default)]
    updates_only: bool,
    #[serde(default)]
    use_models: Vec<Model>,
}
pub enum Request {
    Capabilities(pb::CapabilityRequest),
    Get(pb::GetRequest),
    Set(pb::SetRequest),
    Subscribe(pb::SubscriptionList),
    Poll,
    Cancel,
    Wait,
    Disconnect,
}
fn decode<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T, Status> {
    serde_json::from_value(value.clone())
        .map_err(|_| Status::invalid_argument("invalid typed gNMI request"))
}
pub fn parse(action: &Value) -> Result<Request, Status> {
    super::check_action(action)?;
    Ok(match action["type"].as_str().unwrap_or("") {
        "disconnect" => Request::Disconnect,
        "wait_for_more" => Request::Wait,
        "gnmi_poll" => Request::Poll,
        "gnmi_cancel" => Request::Cancel,
        "gnmi_capabilities" => Request::Capabilities(pb::CapabilityRequest { extension: vec![] }),
        "gnmi_get" => {
            let request: Get = decode(&action["request"])?;
            let kind = match request.data_type.as_str() {
                "ALL" => 0,
                "CONFIG" => 1,
                "STATE" => 2,
                "OPERATIONAL" => 3,
                _ => return Err(Status::invalid_argument("unknown data_type")),
            };
            let request = value::checked(pb::GetRequest {
                prefix: Some(request.prefix.into_proto()?),
                path: request
                    .path
                    .into_iter()
                    .map(Path::into_proto)
                    .collect::<Result<_, _>>()?,
                r#type: kind,
                encoding: value::encoding(&request.encoding)?,
                use_models: semantic::model_proto(request.use_models)?,
                extension: vec![],
            })?;
            semantic::get_request(&request)?;
            Request::Get(request)
        }
        "gnmi_set" => {
            let request: Set = decode(&action["request"])?;
            let request = value::checked(pb::SetRequest {
                prefix: Some(request.prefix.into_proto()?),
                delete: request
                    .delete
                    .into_iter()
                    .map(Path::into_proto)
                    .collect::<Result<_, _>>()?,
                replace: request
                    .replace
                    .into_iter()
                    .map(Update::into_proto)
                    .collect::<Result<_, _>>()?,
                update: request
                    .update
                    .into_iter()
                    .map(Update::into_proto)
                    .collect::<Result<_, _>>()?,
                union_replace: vec![],
                extension: vec![],
            })?;
            semantic::set_request(&request)?;
            Request::Set(request)
        }
        "gnmi_subscribe" => {
            let request: Subscribe = decode(&action["request"])?;
            let mode = match request.mode.as_str() {
                "STREAM" => 0,
                "ONCE" => 1,
                "POLL" => 2,
                _ => return Err(Status::invalid_argument("unknown subscribe mode")),
            };
            let list = pb::SubscriptionList {
                prefix: Some(request.prefix.into_proto()?),
                subscription: request
                    .subscription
                    .into_iter()
                    .map(|s| {
                        let mode = match s.mode.as_deref().unwrap_or("TARGET_DEFINED") {
                            "TARGET_DEFINED" => 0,
                            "ON_CHANGE" => 1,
                            _ => return Err(Status::unimplemented("SAMPLE is excluded")),
                        };
                        Ok(pb::Subscription {
                            path: Some(s.path.into_proto()?),
                            mode,
                            sample_interval: 0,
                            suppress_redundant: false,
                            heartbeat_interval: 0,
                        })
                    })
                    .collect::<Result<_, Status>>()?,
                qos: None,
                mode,
                allow_aggregation: false,
                use_models: semantic::model_proto(request.use_models)?,
                encoding: value::encoding(&request.encoding)?,
                updates_only: request.updates_only,
            };
            semantic::subscription(&list)?;
            value::checked(pb::SubscribeRequest {
                request: Some(pb::subscribe_request::Request::Subscribe(list.clone())),
                extension: vec![],
            })?;
            Request::Subscribe(list)
        }
        _ => return Err(Status::invalid_argument("unknown gNMI client action")),
    })
}
