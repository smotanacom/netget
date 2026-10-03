use super::codec::{DEFAULT_LLM_FALLBACK, MAX_BODY_BYTES, MAX_POINTS};
use crate::llm::actions::{
    protocol_trait::{ActionResult, Protocol, Server},
    ActionDefinition, Parameter, ParameterDefinition, StartupExamples,
};
use crate::protocol::log_template::LogTemplate;
use crate::protocol::{
    metadata::{DevelopmentState, ProtocolMetadataV2},
    EventType, SpawnContext,
};
use crate::state::app_state::AppState;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::LazyLock;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Decision {
    Accept,
    Reject {
        status: u16,
        message: String,
        retry_after_seconds: Option<u16>,
    },
    Partial {
        accepted_lines: Vec<usize>,
        message: String,
    },
}
pub fn parameter(name: &str, type_hint: &str, description: &str, required: bool) -> Parameter {
    Parameter {
        name: name.into(),
        type_hint: type_hint.into(),
        description: description.into(),
        required,
    }
}
fn accept_action() -> ActionDefinition {
    ActionDefinition { name:"accept_influx_points".into(),description:"Accept every validated point. Returns 204 when all lines are valid, or a 400 partial-write response for syntax errors. Acceptance is observation/handling, not persistent storage.".into(),parameters:vec![],example:json!({"type":"accept_influx_points"}),log_template: Some(LogTemplate::new().with_info("Accept validated InfluxDB write points")) }
}
fn reject_action() -> ActionDefinition {
    ActionDefinition { name:"reject_influx_points".into(),description:"Reject all points with an InfluxDB JSON error; no automatic retry or private schema storage.".into(),parameters:vec![parameter("status","number","400,401,403,404,413,422,429,500 or 503",true),parameter("message","string","Error explanation, <=1024 bytes, no controls",true),parameter("retry_after_seconds","number","Optional retry advice 1..3600, only 429/503",false)],example:json!({"type":"reject_influx_points","status":422,"message":"Field type conflict"}),log_template: Some(LogTemplate::new().with_info("Reject InfluxDB write with HTTP {status}: {message}")) }
}
fn partial_action() -> ActionDefinition {
    ActionDefinition { name:"accept_influx_subset".into(),description:"Accept the specified valid source line numbers, rejecting every other line with an explicit 400 partial-write JSON error. Cannot accept a syntax-invalid line.".into(),parameters:vec![parameter("accepted_lines","array","Unique valid source line numbers, <=256",true),parameter("message","string","Partial-write explanation, <=1024 bytes, no controls",true)],example:json!({"type":"accept_influx_subset","accepted_lines":[1],"message":"Second point rejected"}),log_template: Some(LogTemplate::new().with_info("Accept InfluxDB write subset: source lines {accepted_lines}")) }
}
pub static INFLUX_WRITE_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new("influx_write","A v2 write with org, bucket, precision, typed valid points/source line numbers/timestamp_ns, syntax errors and authentication facts. Tokens and raw line protocol never appear in event data.",accept_action().example).with_parameters(vec![parameter("org","string","Organization name/ID",true),parameter("bucket","string","Target bucket name or ID for the submitted points",true),parameter("precision","string","Timestamp precision: ns, us, ms or s",true),parameter("points","array","Valid typed points and source line numbers",true),parameter("errors","array","Syntax error line numbers and explanations",true),parameter("authenticated","boolean","Configured token check succeeded (or authentication not configured)",true),parameter("auth_required","boolean","A token is configured",true),parameter("source_addr","string","TCP socket address of the HTTP write sender",true)]).with_actions(vec![accept_action(),reject_action(),partial_action()])
});
#[derive(Default)]
pub struct InfluxDbProtocol;
impl InfluxDbProtocol {
    pub fn new() -> Self {
        Self
    }
}
impl Protocol for InfluxDbProtocol {
    fn protocol_name(&self) -> &'static str {
        "InfluxDB"
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>HTTP>InfluxDB"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["influxdb", "influxdb2", "influx-write"]
    }
    fn group_name(&self) -> &'static str {
        "Network Services"
    }
    fn description(&self) -> &'static str {
        "Bounded typed InfluxDB v2 write collector; no model calls for unmatched batches by default"
    }
    fn example_prompt(&self) -> &'static str {
        "Collect InfluxDB v2 writes on localhost port 8086"
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![accept_action(), reject_action(), partial_action()]
    }
    fn get_async_actions(&self, _: &AppState) -> Vec<ActionDefinition> {
        vec![]
    }
    fn get_event_types(&self) -> Vec<EventType> {
        vec![INFLUX_WRITE_EVENT.clone()]
    }
    fn metadata(&self) -> ProtocolMetadataV2 {
        ProtocolMetadataV2::builder().answers_on_failure().state(DevelopmentState::Experimental).well_known_port(8086).max_inbound_bytes(MAX_BODY_BYTES).request_only("InfluxDB writes are HTTP request/response; no unprompted peer messages")
        .implementation("Native bounded typed line protocol, HTTP/1.1 v2 write endpoint and gzip via existing flate2")
        .llm_control("Explicit handlers accept/reject/partially accept typed points; shared memory; unmatched writes collect without model calls unless opted in")
        .e2e_testing("Official Python1.50.0 emitter; official InfluxDB2.9.1 daemon with Python readback; MIT line-protocol2.2.1 decoder-backed HTTP receiver; native pair/negative/lifecycle bounds")
        .notes("POST /api/v2/write: org, bucket, precision ns/us/ms/s; identity/gzip; optional Token/Bearer authentication (anonymous if absent). Bounds: 256 KiB compressed/decompressed body, 16 KiB line, 256 points, 1024 lines, 64 tags/fields, 1024-byte names/token, 256 connections, 32 KiB/64 headers, 30s header/body and 10s response-write deadlines. One request per TCP connection. Strict single separators; no CR/LF/NUL strings or trailing-backslash names. Empty successful handler answer accepts valid lines; failed actions return503. No orgID query, role/org ACLs, TLS, query/schema/durable store, retries, fuzz or pcap.").build()
    }
    fn get_startup_parameters(&self) -> Vec<ParameterDefinition> {
        vec![
        ParameterDefinition {name:"auth_token".into(),type_hint:"string".into(),description:"Expected Token/Bearer credential, <=1024 printable ASCII bytes; absent means anonymous collector. Never forwarded to event handlers/access logs.".into(),required:false,example:json!("collector-secret"),default:None},
        ParameterDefinition {name:"llm_fallback".into(),type_hint:"boolean".into(),description:"Opt unmatched writes into model calls; configured handlers always run".into(),required:false,example:json!(true),default:Some(json!(DEFAULT_LLM_FALLBACK))},
    ]
    }
    fn get_startup_examples(&self) -> StartupExamples {
        StartupExamples::new(
            json!({"type":"open_server","base_stack":"influxdb","port":8086,"startup_params":{"auth_token":"collector-secret","llm_fallback":true},"instruction":"Decide which metric points to accept"}),
            json!({"type":"open_server","base_stack":"influxdb","port":8086,"event_handlers":[{"event_pattern":"influx_write","handler":{"type":"script","language":"python","code":"import json,sys\nx=json.load(sys.stdin)\nprint(json.dumps({'actions':[{'type':'accept_influx_points'}]}))"}}]}),
            json!({"type":"open_server","base_stack":"influxdb","port":8086,"event_handlers":[{"event_pattern":"influx_write","handler":{"type":"static","actions":[{"type":"accept_influx_points"}]}}]}),
        )
    }
}
fn message(action: &Value) -> Result<String> {
    let message = action["message"].as_str().context("message required")?;
    ensure!(
        !message.is_empty() && message.len() <= 1024 && !message.chars().any(char::is_control),
        "message byte/control limit"
    );
    Ok(message.into())
}
impl Server for InfluxDbProtocol {
    fn spawn(
        &self,
        ctx: SpawnContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<std::net::SocketAddr>> + Send>>
    {
        Box::pin(super::InfluxDbServer::spawn(ctx))
    }
    fn execute_action(&self, action: Value) -> Result<ActionResult> {
        let decision = match action["type"].as_str() {
            Some("accept_influx_points") => Decision::Accept,
            Some("reject_influx_points") => {
                let status = u16::try_from(action["status"].as_u64().context("status required")?)?;
                ensure!(
                    [400, 401, 403, 404, 413, 422, 429, 500, 503].contains(&status),
                    "unsupported rejection status"
                );
                let retry = action
                    .get("retry_after_seconds")
                    .map(|v| {
                        u16::try_from(v.as_u64().context("retry_after_seconds must be integer")?)
                            .map_err(anyhow::Error::from)
                    })
                    .transpose()?;
                if let Some(n) = retry {
                    ensure!(
                        (1..=3600).contains(&n) && [429, 503].contains(&status),
                        "Retry-After only 1..3600 on 429/503"
                    );
                }
                Decision::Reject {
                    status,
                    message: message(&action)?,
                    retry_after_seconds: retry,
                }
            }
            Some("accept_influx_subset") => {
                let lines: Vec<usize> = serde_json::from_value(
                    action
                        .get("accepted_lines")
                        .context("accepted_lines required")?
                        .clone(),
                )?;
                ensure!(
                    lines.len() <= MAX_POINTS && lines.iter().all(|n| *n > 0),
                    "accepted line count/range"
                );
                ensure!(
                    lines
                        .iter()
                        .collect::<std::collections::BTreeSet<_>>()
                        .len()
                        == lines.len(),
                    "duplicate accepted line"
                );
                Decision::Partial {
                    accepted_lines: lines,
                    message: message(&action)?,
                }
            }
            _ => bail!("unknown InfluxDB collector action"),
        };
        Ok(ActionResult::Custom {
            name: "influx_write_decision".into(),
            data: serde_json::to_value(decision)?,
        })
    }
}
