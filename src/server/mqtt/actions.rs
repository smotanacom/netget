//! MQTT protocol actions, events and metadata
//!
//! Every action declared here is executed by [`MqttProtocol::execute_action`] and
//! every declared field is read. Sync actions write directly to the connection they
//! were produced for, so ordering with the read loop is preserved by the connection's
//! single writer channel.

use crate::llm::actions::{
    protocol_trait::{ActionResult, Protocol, Server},
    ActionDefinition, Parameter, ParameterDefinition,
};
use crate::protocol::log_template::LogTemplate;
use crate::protocol::EventType;
use crate::server::connection::ConnectionId;
use crate::server::mqtt::{
    build_ack, build_connack, build_publish, build_suback, PKT_CONNACK, PKT_PUBACK, PKT_PUBLISH,
    PKT_PUBREC, PKT_SUBACK, PKT_UNSUBACK,
};
use crate::state::app_state::AppState;
use crate::state::ServerId;
use anyhow::{Context, Result};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{LazyLock, Mutex, RwLock};
use tokio::sync::mpsc;
use tracing::{debug, warn};

// ============================================================================
// Live connection directory
// ============================================================================

/// Senders for the currently connected MQTT clients, keyed by `(server id, client id)`.
///
/// This is a directory of *live sockets*, not storage: nothing is retained after a
/// client disconnects, and no message, subscription, topic or retained value is ever
/// held here. It exists so the model can name a recipient for `mqtt_publish`.
static MQTT_CLIENTS: LazyLock<Mutex<HashMap<(u32, String), mpsc::UnboundedSender<Vec<u8>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Record a connected client's writer so actions can address it by client id.
pub fn register_client(server_id: ServerId, client_id: &str, tx: mpsc::UnboundedSender<Vec<u8>>) {
    if let Ok(mut map) = MQTT_CLIENTS.lock() {
        map.insert((server_id.as_u32(), client_id.to_string()), tx);
    }
}

/// Drop a client from the directory when its connection ends.
pub fn unregister_client(server_id: ServerId, client_id: &str) {
    if let Ok(mut map) = MQTT_CLIENTS.lock() {
        map.remove(&(server_id.as_u32(), client_id.to_string()));
    }
}

/// Client ids currently connected to one server, sorted.
pub fn list_clients(server_id: ServerId) -> Vec<String> {
    let Ok(map) = MQTT_CLIENTS.lock() else {
        return Vec::new();
    };
    let mut ids: Vec<String> = map
        .keys()
        .filter(|(sid, _)| *sid == server_id.as_u32())
        .map(|(_, cid)| cid.clone())
        .collect();
    ids.sort();
    ids
}

/// Deliver bytes to one client id, or to every client on the server when
/// `client_id` is `"*"`. Returns the client ids actually written to.
fn deliver(server_id: u32, client_id: &str, bytes: &[u8]) -> Vec<String> {
    let Ok(map) = MQTT_CLIENTS.lock() else {
        return Vec::new();
    };
    let mut delivered = Vec::new();
    for ((sid, cid), tx) in map.iter() {
        if *sid != server_id {
            continue;
        }
        if client_id != "*" && cid != client_id {
            continue;
        }
        if tx.send(bytes.to_vec()).is_ok() {
            delivered.push(cid.clone());
        }
    }
    delivered.sort();
    delivered
}

// ============================================================================
// Protocol
// ============================================================================

/// MQTT protocol action handler.
///
/// Two shapes: the registry-wide instance built by [`MqttProtocol::new`] (used for
/// documentation, spawning and user-triggered async actions), and a per-connection
/// instance built by [`MqttProtocol::for_connection`] that owns the writer for one
/// client and therefore can execute the sync response actions.
pub struct MqttProtocol {
    server_id: Option<ServerId>,
    #[allow(dead_code)]
    connection_id: Option<ConnectionId>,
    out_tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
    status_tx: Option<mpsc::UnboundedSender<String>>,
    client_id: RwLock<Option<String>>,
    /// Bitmask of MQTT packet types written to *this* connection, used by the
    /// connection loop to tell whether the handler produced the mandatory reply.
    written_types: AtomicU32,
}

impl Default for MqttProtocol {
    fn default() -> Self {
        Self::new()
    }
}

impl MqttProtocol {
    /// Registry-wide instance: no connection, so only async actions can execute.
    pub fn new() -> Self {
        Self {
            server_id: None,
            connection_id: None,
            out_tx: None,
            status_tx: None,
            client_id: RwLock::new(None),
            written_types: AtomicU32::new(0),
        }
    }

    /// Instance bound to one client connection.
    pub fn for_connection(
        server_id: ServerId,
        connection_id: ConnectionId,
        out_tx: mpsc::UnboundedSender<Vec<u8>>,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Self {
        Self {
            server_id: Some(server_id),
            connection_id: Some(connection_id),
            out_tx: Some(out_tx),
            status_tx: Some(status_tx),
            client_id: RwLock::new(None),
            written_types: AtomicU32::new(0),
        }
    }

    pub fn set_client_id(&self, client_id: &str) {
        if let Ok(mut guard) = self.client_id.write() {
            *guard = Some(client_id.to_string());
        }
    }

    fn current_client_id(&self) -> Option<String> {
        self.client_id.read().ok().and_then(|g| g.clone())
    }

    /// True if a packet of `packet_type` has been written to this connection since
    /// the last [`MqttProtocol::clear_written_types`].
    pub fn wrote_packet_type(&self, packet_type: u8) -> bool {
        self.written_types.load(Ordering::SeqCst) & (1u32 << (packet_type & 0x1F)) != 0
    }

    pub fn clear_written_types(&self) {
        self.written_types.store(0, Ordering::SeqCst);
    }

    /// Write a packet to this connection and record its type.
    fn send_to_connection(&self, packet_type: u8, bytes: Vec<u8>) -> Result<()> {
        let tx = self.out_tx.as_ref().context(
            "This MQTT action can only run in response to a client packet (no connection bound)",
        )?;
        tx.send(bytes)
            .map_err(|_| anyhow::anyhow!("MQTT connection is already closed"))?;
        self.written_types
            .fetch_or(1u32 << (packet_type & 0x1F), Ordering::SeqCst);
        Ok(())
    }

    fn log(&self, message: String) {
        debug!("{}", message);
        if let Some(tx) = &self.status_tx {
            let _ = tx.send(format!("[DEBUG] {}", message));
        }
    }

    fn require_packet_id(action: &serde_json::Value) -> Result<u16> {
        let id = action
            .get("packet_id")
            .and_then(|v| v.as_u64())
            .context("Missing 'packet_id'. Echo the packet_id from the triggering event so the client can match the acknowledgement to its request")?;
        if id > u16::MAX as u64 {
            return Err(anyhow::anyhow!(
                "packet_id {} is out of range (0-65535)",
                id
            ));
        }
        Ok(id as u16)
    }

    /// The broker this action addresses: our own id when the executor is bound to one,
    /// otherwise the model's `server_id`.
    ///
    /// The `None` arm used to end in `as u32`, which wraps in silence — `4294967297` becomes
    /// `1`, so an action naming a broker that does not exist is delivered to whichever broker
    /// happens to be number 1 rather than failing.
    fn require_server_id(&self, action: &serde_json::Value) -> Result<u32> {
        if let Some(id) = self.server_id {
            return Ok(id.as_u32());
        }
        let raw = action.get("server_id").and_then(|v| v.as_u64()).context(
            "Missing 'server_id' (the id of the running MQTT server, from the server list)",
        )?;
        u32::try_from(raw).map_err(|_| {
            anyhow::anyhow!(
                "server_id {raw} is not a NetGet server id (0-4294967295); take it from the \
                 server list rather than inventing one"
            )
        })
    }

    // ---- executors -------------------------------------------------------

    fn execute_connack(&self, action: serde_json::Value) -> Result<ActionResult> {
        let return_code = action
            .get("return_code")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if return_code > 5 {
            return Err(anyhow::anyhow!(
                "return_code {} is not an MQTT 3.1.1 CONNACK code (0=accepted, 1=bad protocol version, 2=client id rejected, 3=server unavailable, 4=bad username/password, 5=not authorized)",
                return_code
            ));
        }
        let session_present = action
            .get("session_present")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        self.log(format!(
            "MQTT -> CONNACK rc={} session_present={} to '{}'",
            return_code,
            session_present,
            self.current_client_id().unwrap_or_else(|| "?".into())
        ));

        self.send_to_connection(
            PKT_CONNACK,
            build_connack(return_code as u8, session_present),
        )?;

        Ok(ActionResult::Custom {
            name: "mqtt_connack".to_string(),
            data: json!({ "return_code": return_code, "session_present": session_present }),
        })
    }

    fn execute_puback(&self, action: serde_json::Value) -> Result<ActionResult> {
        let packet_id = Self::require_packet_id(&action)?;
        self.log(format!("MQTT -> PUBACK id={}", packet_id));
        self.send_to_connection(PKT_PUBACK, build_ack(PKT_PUBACK, packet_id))?;
        Ok(ActionResult::Custom {
            name: "mqtt_puback".to_string(),
            data: json!({ "packet_id": packet_id }),
        })
    }

    fn execute_pubrec(&self, action: serde_json::Value) -> Result<ActionResult> {
        let packet_id = Self::require_packet_id(&action)?;
        self.log(format!("MQTT -> PUBREC id={}", packet_id));
        self.send_to_connection(PKT_PUBREC, build_ack(PKT_PUBREC, packet_id))?;
        Ok(ActionResult::Custom {
            name: "mqtt_pubrec".to_string(),
            data: json!({ "packet_id": packet_id }),
        })
    }

    fn execute_suback(&self, action: serde_json::Value) -> Result<ActionResult> {
        let packet_id = Self::require_packet_id(&action)?;
        let granted = action
            .get("granted_qos")
            .and_then(|v| v.as_array())
            .context("Missing 'granted_qos'. Provide one entry per topic filter in the subscribe event: 0, 1 or 2 to grant that QoS, or 128 to refuse the subscription")?;

        let mut codes = Vec::with_capacity(granted.len());
        for value in granted {
            let code = value
                .as_u64()
                .context("granted_qos entries must be integers (0, 1, 2 or 128)")?;
            if !matches!(code, 0 | 1 | 2 | 128) {
                return Err(anyhow::anyhow!(
                    "granted_qos entry {} is invalid: use 0, 1 or 2 to grant, 128 to refuse",
                    code
                ));
            }
            codes.push(code as u8);
        }
        if codes.is_empty() {
            return Err(anyhow::anyhow!(
                "granted_qos must contain one entry per requested topic filter"
            ));
        }

        self.log(format!(
            "MQTT -> SUBACK id={} granted={:?}",
            packet_id, codes
        ));
        self.send_to_connection(PKT_SUBACK, build_suback(packet_id, &codes))?;

        Ok(ActionResult::Custom {
            name: "mqtt_suback".to_string(),
            data: json!({ "packet_id": packet_id, "granted_qos": codes }),
        })
    }

    fn execute_unsuback(&self, action: serde_json::Value) -> Result<ActionResult> {
        let packet_id = Self::require_packet_id(&action)?;
        self.log(format!("MQTT -> UNSUBACK id={}", packet_id));
        self.send_to_connection(PKT_UNSUBACK, build_ack(PKT_UNSUBACK, packet_id))?;
        Ok(ActionResult::Custom {
            name: "mqtt_unsuback".to_string(),
            data: json!({ "packet_id": packet_id }),
        })
    }

    /// `mqtt_publish` (sync) and `mqtt_publish_to_client` (async) share this body.
    /// `server_id` is taken from the bound connection for the sync form and from the
    /// action for the async form.
    fn execute_publish(
        &self,
        action: serde_json::Value,
        target_key: &str,
        require_target: bool,
    ) -> Result<ActionResult> {
        let topic = action
            .get("topic")
            .and_then(|v| v.as_str())
            .context("Missing 'topic'")?;
        if topic.is_empty() {
            return Err(anyhow::anyhow!("'topic' must not be empty"));
        }
        let payload = action
            .get("payload")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let qos = action.get("qos").and_then(|v| v.as_u64()).unwrap_or(0);
        if qos > 2 {
            return Err(anyhow::anyhow!("qos must be 0, 1 or 2 (got {})", qos));
        }
        let retain = action
            .get("retain")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let packet_id = if qos > 0 {
            Self::require_packet_id(&action)?
        } else {
            action
                .get("packet_id")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                .min(u16::MAX as u64) as u16
        };

        let bytes = build_publish(topic, &payload, qos as u8, retain, packet_id);

        let target = action.get(target_key).and_then(|v| v.as_str());
        match target {
            Some(client) => {
                let server_id = self.require_server_id(&action)?;
                let delivered = deliver(server_id, client, &bytes);
                if delivered.is_empty() {
                    warn!(
                        "MQTT publish to '{}' on server {} matched no connected client",
                        client, server_id
                    );
                    return Err(anyhow::anyhow!(
                        "No connected MQTT client matches '{}' on server {}. Connected clients: {:?}",
                        client,
                        server_id,
                        list_clients(ServerId::new(server_id))
                    ));
                }
                self.log(format!(
                    "MQTT -> PUBLISH topic='{}' qos={} to {:?}",
                    topic, qos, delivered
                ));
                Ok(ActionResult::Custom {
                    name: "mqtt_publish".to_string(),
                    data: json!({
                        "topic": topic,
                        "qos": qos,
                        "retain": retain,
                        "packet_id": packet_id,
                        "delivered_to": delivered,
                    }),
                })
            }
            None => {
                if require_target {
                    return Err(anyhow::anyhow!(
                        "Missing '{}': name the connected client that should receive this message, or \"*\" for all of them",
                        target_key
                    ));
                }
                self.log(format!(
                    "MQTT -> PUBLISH topic='{}' qos={} to '{}'",
                    topic,
                    qos,
                    self.current_client_id().unwrap_or_else(|| "?".into())
                ));
                self.send_to_connection(PKT_PUBLISH, bytes)?;
                Ok(ActionResult::Custom {
                    name: "mqtt_publish".to_string(),
                    data: json!({
                        "topic": topic,
                        "qos": qos,
                        "retain": retain,
                        "packet_id": packet_id,
                        "delivered_to": [self.current_client_id()],
                    }),
                })
            }
        }
    }

    fn execute_list_clients(&self, action: serde_json::Value) -> Result<ActionResult> {
        let server_id = self.require_server_id(&action)?;
        let clients = list_clients(ServerId::new(server_id));
        Ok(ActionResult::Custom {
            name: "list_mqtt_clients".to_string(),
            data: json!({ "server_id": server_id, "clients": clients }),
        })
    }
}

impl Protocol for MqttProtocol {
    fn get_startup_parameters(&self) -> Vec<ParameterDefinition> {
        vec![ParameterDefinition {
            name: "max_packet_size".to_string(),
            type_hint: "integer".to_string(),
            description:
                "Largest MQTT control packet accepted from a client, in bytes (default 262144, \
                 maximum 16777216). Connections sending a larger packet are closed."
                    .to_string(),
            required: false,
            example: json!(262144),
        }]
    }

    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        vec![mqtt_publish_to_client_action(), list_mqtt_clients_action()]
    }

    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![
            mqtt_connack_action(),
            mqtt_suback_action(),
            mqtt_puback_action(),
            mqtt_pubrec_action(),
            mqtt_unsuback_action(),
            mqtt_publish_action(),
            close_this_connection_action(),
        ]
    }

    fn protocol_name(&self) -> &'static str {
        "MQTT"
    }

    fn get_event_types(&self) -> Vec<EventType> {
        get_mqtt_event_types()
    }

    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>MQTT"
    }

    fn keywords(&self) -> Vec<&'static str> {
        vec![
            "mqtt",
            "mosquitto",
            "iot messaging",
            "message queue telemetry",
        ]
    }

    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{DevelopmentState, ProtocolMetadataV2};

        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Beta)
            .implementation("Hand-written MQTT 3.1.1 control-packet codec (no broker crate)")
            .llm_control(
                "CONNACK return code, SUBACK granted QoS, PUBACK/PUBREC for QoS>0 publishes, \
                 and broker-originated PUBLISH to any named connected client",
            )
            .e2e_testing(
                "rumqttc, in two tests that are not #[ignore]d: test_mqtt_basic_connect \
                 (CONNECT/CONNACK) and test_mqtt_subscribe_and_receive_a_published_message, \
                 where a real rumqttc client completes CONNECT -> SUBSCRIBE -> SUBACK -> \
                 PUBLISH and then receives the broker's own PUBLISH back on the same \
                 connection, with topic and payload asserted.",
            )
            .notes(
                "MQTT 3.1.1 only (no v5, no TLS, no WebSocket). PINGREQ/PINGRESP and \
                 PUBREL/PUBCOMP are answered by the broker without a model call. There is no \
                 subscription table or retained-message store: the model tracks subscriptions \
                 in its memory and names recipients explicitly in mqtt_publish, so nothing is \
                 delivered automatically on publish. Non-UTF-8 payloads reach the model as a \
                 lossy string with payload_is_text=false.",
            )
            .build()
    }

    fn description(&self) -> &'static str {
        "MQTT 3.1.1 broker for IoT messaging"
    }

    fn example_prompt(&self) -> &'static str {
        "Start an MQTT broker on port 1883 that accepts every client and grants QoS 0 subscriptions"
    }

    fn group_name(&self) -> &'static str {
        "Application"
    }

    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;

        StartupExamples::new(
            // LLM mode
            json!({
                "type": "open_server",
                "port": 1883,
                "base_stack": "mqtt",
                "instruction": "MQTT broker for IoT sensors. Accept every CONNECT (mqtt_connack return_code 0). \
                                Grant every subscription at the QoS the client asked for. Acknowledge QoS 1 \
                                publishes with mqtt_puback echoing packet_id. Remember which client subscribed \
                                to which topic filter, and when a sensor publishes, forward it with mqtt_publish \
                                to each subscriber whose filter matches."
            }),
            // Script mode: deterministic, no model call per packet
            json!({
                "type": "open_server",
                "port": 1883,
                "base_stack": "mqtt",
                "event_handlers": [
                    {
                        "event_pattern": "mqtt_connect",
                        "handler": {
                            "type": "script",
                            "language": "python",
                            "code": "respond([{'type': 'mqtt_connack', 'return_code': 0 if event['username'] == 'sensor' else 5}])"
                        }
                    },
                    {
                        "event_pattern": "mqtt_subscribe",
                        "handler": {
                            "type": "script",
                            "language": "python",
                            "code": "respond([{'type': 'mqtt_suback', 'packet_id': event['packet_id'], 'granted_qos': [t['qos'] for t in event['topics']]}])"
                        }
                    }
                ]
            }),
            // Static mode: {{event.packet_id}} echoes the correlation id
            json!({
                "type": "open_server",
                "port": 1883,
                "base_stack": "mqtt",
                "event_handlers": [
                    {
                        "event_pattern": "mqtt_connect",
                        "handler": {
                            "type": "static",
                            "actions": [{"type": "mqtt_connack", "return_code": 0}]
                        }
                    },
                    {
                        "event_pattern": "mqtt_publish",
                        "handler": {
                            "type": "static",
                            "actions": [{"type": "mqtt_puback", "packet_id": "{{event.packet_id}}"}]
                        }
                    }
                ]
            }),
        )
    }
}

impl Server for MqttProtocol {
    fn spawn(
        &self,
        ctx: crate::protocol::SpawnContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::server::mqtt::MqttServer;
            MqttServer::spawn_with_llm_actions(
                ctx.legacy_listen_addr(),
                ctx.llm_client,
                ctx.state,
                ctx.status_tx,
                ctx.server_id,
                ctx.startup_params,
            )
            .await
        })
    }

    fn execute_action(&self, action: serde_json::Value) -> Result<ActionResult> {
        let action_type = action
            .get("type")
            .and_then(|v| v.as_str())
            .context("Missing 'type' field in action")?;

        match action_type {
            "mqtt_connack" => self.execute_connack(action),
            "mqtt_puback" => self.execute_puback(action),
            "mqtt_pubrec" => self.execute_pubrec(action),
            "mqtt_suback" => self.execute_suback(action),
            "mqtt_unsuback" => self.execute_unsuback(action),
            "mqtt_publish" => self.execute_publish(action, "to_client_id", false),
            "mqtt_publish_to_client" => self.execute_publish(action, "client_id", true),
            "list_mqtt_clients" => self.execute_list_clients(action),
            "close_this_connection" => Ok(ActionResult::CloseConnection),
            // The dashboard's "disconnect this peer" injects {"type":"close_connection"}.
            // Not offered to the model (close_this_connection is its verb); this arm exists
            // so the generic peer command task half-closes the shared write half on it.
            "close_connection" => Ok(ActionResult::CloseConnection),
            _ => Err(anyhow::anyhow!("Unknown MQTT action: {}", action_type)),
        }
    }
}

// ============================================================================
// Action definitions
// ============================================================================

pub fn mqtt_connack_action() -> ActionDefinition {
    ActionDefinition {
        name: "mqtt_connack".to_string(),
        description: "Answer a client's CONNECT. Every MQTT connection needs exactly one CONNACK \
                      before the client will send anything else."
            .to_string(),
        parameters: vec![
            Parameter {
                name: "return_code".to_string(),
                type_hint: "integer".to_string(),
                description: "0 = accept the client. Refusals: 1 unacceptable protocol version, \
                              2 client id rejected, 3 server unavailable, 4 bad username or \
                              password, 5 not authorized. Any refusal closes the connection."
                    .to_string(),
                required: true,
            },
            Parameter {
                name: "session_present".to_string(),
                type_hint: "boolean".to_string(),
                description: "True only when resuming a stored session for a client that \
                              connected with clean_session=false. Use false unless you are \
                              deliberately simulating a resumed session."
                    .to_string(),
                required: false,
            },
        ],
        example: json!({"type": "mqtt_connack", "return_code": 0}),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> MQTT CONNACK rc={return_code}")
                .with_debug(
                    "MQTT mqtt_connack: rc={return_code} session_present={session_present}",
                ),
        ),
    }
}

pub fn mqtt_suback_action() -> ActionDefinition {
    ActionDefinition {
        name: "mqtt_suback".to_string(),
        description: "Answer a client's SUBSCRIBE. Required: the client blocks until it arrives."
            .to_string(),
        parameters: vec![
            Parameter {
                name: "packet_id".to_string(),
                type_hint: "integer".to_string(),
                description: "Copy packet_id from the mqtt_subscribe event verbatim. The client \
                              matches this reply to its request by that number."
                    .to_string(),
                required: true,
            },
            Parameter {
                name: "granted_qos".to_string(),
                type_hint: "array".to_string(),
                description: "One integer per topic filter in the event's topics list, in the \
                              same order: 0, 1 or 2 to grant that maximum QoS, or 128 to refuse \
                              that single subscription."
                    .to_string(),
                required: true,
            },
        ],
        example: json!({"type": "mqtt_suback", "packet_id": 10, "granted_qos": [0]}),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> MQTT SUBACK id={packet_id}")
                .with_debug("MQTT mqtt_suback: id={packet_id} granted={granted_qos}"),
        ),
    }
}

pub fn mqtt_puback_action() -> ActionDefinition {
    ActionDefinition {
        name: "mqtt_puback".to_string(),
        description: "Acknowledge a QoS 1 PUBLISH. Without it the client republishes the message \
                      forever. Do not send it for a QoS 0 publish."
            .to_string(),
        parameters: vec![Parameter {
            name: "packet_id".to_string(),
            type_hint: "integer".to_string(),
            description: "Copy packet_id from the mqtt_publish event verbatim.".to_string(),
            required: true,
        }],
        example: json!({"type": "mqtt_puback", "packet_id": 42}),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> MQTT PUBACK id={packet_id}")
                .with_debug("MQTT mqtt_puback: id={packet_id}"),
        ),
    }
}

pub fn mqtt_pubrec_action() -> ActionDefinition {
    ActionDefinition {
        name: "mqtt_pubrec".to_string(),
        description: "Acknowledge a QoS 2 PUBLISH (first half of the handshake). The broker \
                      answers the client's following PUBREL with PUBCOMP on its own."
            .to_string(),
        parameters: vec![Parameter {
            name: "packet_id".to_string(),
            type_hint: "integer".to_string(),
            description: "Copy packet_id from the mqtt_publish event verbatim.".to_string(),
            required: true,
        }],
        example: json!({"type": "mqtt_pubrec", "packet_id": 42}),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> MQTT PUBREC id={packet_id}")
                .with_debug("MQTT mqtt_pubrec: id={packet_id}"),
        ),
    }
}

pub fn mqtt_unsuback_action() -> ActionDefinition {
    ActionDefinition {
        name: "mqtt_unsuback".to_string(),
        description: "Answer a client's UNSUBSCRIBE. Required: the client blocks until it arrives."
            .to_string(),
        parameters: vec![Parameter {
            name: "packet_id".to_string(),
            type_hint: "integer".to_string(),
            description: "Copy packet_id from the mqtt_unsubscribe event verbatim.".to_string(),
            required: true,
        }],
        example: json!({"type": "mqtt_unsuback", "packet_id": 11}),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> MQTT UNSUBACK id={packet_id}")
                .with_debug("MQTT mqtt_unsuback: id={packet_id}"),
        ),
    }
}

pub fn mqtt_publish_action() -> ActionDefinition {
    ActionDefinition {
        name: "mqtt_publish".to_string(),
        description: "Send a message from the broker to a client. This is the only way a \
                      subscriber ever receives anything: the broker does no routing of its own, \
                      so after a client publishes, deliver it yourself to each subscriber you \
                      remember. Without to_client_id it goes to the client whose packet you are \
                      answering."
            .to_string(),
        parameters: vec![
            Parameter {
                name: "topic".to_string(),
                type_hint: "string".to_string(),
                description: "Exact topic name to publish on, e.g. 'home/kitchen/temp'. Never a \
                              wildcard: '+' and '#' are only valid in subscriptions."
                    .to_string(),
                required: true,
            },
            Parameter {
                name: "payload".to_string(),
                type_hint: "string".to_string(),
                description: "Message body as text (JSON, a number, anything). Sent as UTF-8."
                    .to_string(),
                required: false,
            },
            Parameter {
                name: "qos".to_string(),
                type_hint: "integer".to_string(),
                description: "0 (default, fire and forget), 1 or 2. For 1 and 2 you must also \
                              supply packet_id, and the client will acknowledge it."
                    .to_string(),
                required: false,
            },
            Parameter {
                name: "retain".to_string(),
                type_hint: "boolean".to_string(),
                description: "Set the RETAIN flag on the delivered message. The broker stores \
                              nothing, so this only marks the packet for the receiving client."
                    .to_string(),
                required: false,
            },
            Parameter {
                name: "packet_id".to_string(),
                type_hint: "integer".to_string(),
                description: "Identifier 1-65535 for this message, required when qos is 1 or 2. \
                              Pick an unused number; the client echoes it back."
                    .to_string(),
                required: false,
            },
            Parameter {
                name: "to_client_id".to_string(),
                type_hint: "string".to_string(),
                description: "Client id of the recipient, taken from connected_clients in the \
                              event, or \"*\" for every client connected to this server. Omit to \
                              reply on the current connection."
                    .to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "mqtt_publish",
            "to_client_id": "dashboard",
            "topic": "home/kitchen/temp",
            "payload": "21.5",
            "qos": 0
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> MQTT PUBLISH {topic}")
                .with_debug("MQTT mqtt_publish: topic={topic} qos={qos} to={to_client_id}"),
        ),
    }
}

pub fn close_this_connection_action() -> ActionDefinition {
    ActionDefinition {
        name: "close_this_connection".to_string(),
        description: "Drop the current MQTT connection without a further packet. MQTT has no \
                      server-side disconnect message in 3.1.1, so this is how a client is ejected."
            .to_string(),
        parameters: vec![],
        example: json!({"type": "close_this_connection"}),
        log_template: Some(
            LogTemplate::new()
                .with_info("MQTT connection closed by handler")
                .with_debug("MQTT close_this_connection"),
        ),
    }
}

pub fn mqtt_publish_to_client_action() -> ActionDefinition {
    ActionDefinition {
        name: "mqtt_publish_to_client".to_string(),
        description: "Push a message to a connected MQTT client outside of any request, e.g. \
                      when the user asks to send a reading to a subscriber."
            .to_string(),
        parameters: vec![
            Parameter {
                name: "server_id".to_string(),
                type_hint: "integer".to_string(),
                description: "Id of the running MQTT server, as shown in the server list."
                    .to_string(),
                required: true,
            },
            Parameter {
                name: "client_id".to_string(),
                type_hint: "string".to_string(),
                description: "Client id of the recipient, or \"*\" for every client connected to \
                              that server. Use list_mqtt_clients to see the current ids."
                    .to_string(),
                required: true,
            },
            Parameter {
                name: "topic".to_string(),
                type_hint: "string".to_string(),
                description: "Exact topic name (no wildcards).".to_string(),
                required: true,
            },
            Parameter {
                name: "payload".to_string(),
                type_hint: "string".to_string(),
                description: "Message body as text, sent as UTF-8.".to_string(),
                required: false,
            },
            Parameter {
                name: "qos".to_string(),
                type_hint: "integer".to_string(),
                description: "0 (default), 1 or 2. packet_id is required for 1 and 2.".to_string(),
                required: false,
            },
            Parameter {
                name: "retain".to_string(),
                type_hint: "boolean".to_string(),
                description: "Set the RETAIN flag on the delivered message.".to_string(),
                required: false,
            },
            Parameter {
                name: "packet_id".to_string(),
                type_hint: "integer".to_string(),
                description: "Identifier 1-65535, required when qos is 1 or 2.".to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "mqtt_publish_to_client",
            "server_id": 1,
            "client_id": "dashboard",
            "topic": "alerts/high_temp",
            "payload": "31.2"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> MQTT PUBLISH {topic} to {client_id}")
                .with_debug("MQTT mqtt_publish_to_client: {client_id} topic={topic}"),
        ),
    }
}

pub fn list_mqtt_clients_action() -> ActionDefinition {
    ActionDefinition {
        name: "list_mqtt_clients".to_string(),
        description: "List the client ids currently connected to an MQTT server.".to_string(),
        parameters: vec![Parameter {
            name: "server_id".to_string(),
            type_hint: "integer".to_string(),
            description: "Id of the running MQTT server, as shown in the server list.".to_string(),
            required: true,
        }],
        example: json!({"type": "list_mqtt_clients", "server_id": 1}),
        log_template: Some(
            LogTemplate::new()
                .with_info("MQTT list clients")
                .with_debug("MQTT list_mqtt_clients: server={server_id}"),
        ),
    }
}

// ============================================================================
// Action constants
// ============================================================================

pub static MQTT_CONNACK_ACTION: LazyLock<ActionDefinition> = LazyLock::new(mqtt_connack_action);
pub static MQTT_SUBACK_ACTION: LazyLock<ActionDefinition> = LazyLock::new(mqtt_suback_action);
pub static MQTT_PUBACK_ACTION: LazyLock<ActionDefinition> = LazyLock::new(mqtt_puback_action);
pub static MQTT_PUBREC_ACTION: LazyLock<ActionDefinition> = LazyLock::new(mqtt_pubrec_action);
pub static MQTT_UNSUBACK_ACTION: LazyLock<ActionDefinition> = LazyLock::new(mqtt_unsuback_action);
pub static MQTT_PUBLISH_ACTION: LazyLock<ActionDefinition> = LazyLock::new(mqtt_publish_action);
pub static MQTT_CLOSE_CONNECTION_ACTION: LazyLock<ActionDefinition> =
    LazyLock::new(close_this_connection_action);

// ============================================================================
// Event types
// ============================================================================

/// A client sent CONNECT. The connection stays silent until an `mqtt_connack` is
/// produced (the broker sends an accepting CONNACK itself if the handler produces none).
pub static MQTT_CONNECT_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "mqtt_connect",
        "MQTT client sent CONNECT and is waiting for CONNACK",
        json!({"type": "placeholder", "event_id": "mqtt_connect"}),
    )
    .with_parameters(vec![
        Parameter {
            name: "client_id".to_string(),
            type_hint: "string".to_string(),
            description: "Client identifier from the CONNECT packet. Empty client ids are \
                          replaced with 'anon-<connection id>'."
                .to_string(),
            required: true,
        },
        Parameter {
            name: "username".to_string(),
            type_hint: "string".to_string(),
            description: "Username supplied by the client, or null if none.".to_string(),
            required: false,
        },
        Parameter {
            name: "has_password".to_string(),
            type_hint: "boolean".to_string(),
            description: "Whether a password field was present. The password itself is not \
                          surfaced."
                .to_string(),
            required: true,
        },
        Parameter {
            name: "clean_session".to_string(),
            type_hint: "boolean".to_string(),
            description: "Client asked to start a fresh session.".to_string(),
            required: true,
        },
        Parameter {
            name: "keep_alive".to_string(),
            type_hint: "integer".to_string(),
            description: "Keep-alive interval in seconds requested by the client.".to_string(),
            required: true,
        },
        Parameter {
            name: "protocol_name".to_string(),
            type_hint: "string".to_string(),
            description: "Protocol name from the packet, normally 'MQTT'.".to_string(),
            required: true,
        },
        Parameter {
            name: "protocol_level".to_string(),
            type_hint: "integer".to_string(),
            description: "4 for MQTT 3.1.1, 5 for MQTT 5.0 (only 3.1.1 is implemented)."
                .to_string(),
            required: true,
        },
        Parameter {
            name: "will_topic".to_string(),
            type_hint: "string".to_string(),
            description: "Last-will topic, or null when the client set no will.".to_string(),
            required: false,
        },
        Parameter {
            name: "will_message".to_string(),
            type_hint: "string".to_string(),
            description: "Last-will payload as text, or null. The broker never publishes it."
                .to_string(),
            required: false,
        },
    ])
    .with_actions(vec![
        MQTT_CONNACK_ACTION.clone(),
        MQTT_CLOSE_CONNECTION_ACTION.clone(),
    ])
    .with_log_template(
        LogTemplate::new()
            .with_info("MQTT CONNECT: {client_id}")
            .with_debug("MQTT CONNECT: client_id={client_id} user={username} clean={clean_session}")
            .with_trace("MQTT CONNECT: {json_pretty(.)}"),
    )
});

/// A client published a message.
pub static MQTT_PUBLISH_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "mqtt_publish",
        "MQTT client published a message to a topic",
        json!({"type": "placeholder", "event_id": "mqtt_publish"}),
    )
    .with_parameters(vec![
        Parameter {
            name: "client_id".to_string(),
            type_hint: "string".to_string(),
            description: "Client that sent the message.".to_string(),
            required: true,
        },
        Parameter {
            name: "topic".to_string(),
            type_hint: "string".to_string(),
            description: "Exact topic the message was published on.".to_string(),
            required: true,
        },
        Parameter {
            name: "payload".to_string(),
            type_hint: "string".to_string(),
            description: "Message body decoded as UTF-8 text.".to_string(),
            required: true,
        },
        Parameter {
            name: "payload_is_text".to_string(),
            type_hint: "boolean".to_string(),
            description: "False when the payload was not valid UTF-8; the payload field is then \
                          a lossy rendering and payload_size gives the true byte count."
                .to_string(),
            required: true,
        },
        Parameter {
            name: "payload_size".to_string(),
            type_hint: "integer".to_string(),
            description: "Payload length in bytes as received.".to_string(),
            required: true,
        },
        Parameter {
            name: "qos".to_string(),
            type_hint: "integer".to_string(),
            description: "0, 1 or 2. QoS 1 needs mqtt_puback, QoS 2 needs mqtt_pubrec, QoS 0 \
                          needs no reply."
                .to_string(),
            required: true,
        },
        Parameter {
            name: "retain".to_string(),
            type_hint: "boolean".to_string(),
            description: "Client asked the broker to retain this message for future subscribers."
                .to_string(),
            required: true,
        },
        Parameter {
            name: "duplicate".to_string(),
            type_hint: "boolean".to_string(),
            description: "DUP flag: the client is redelivering a message it thinks was lost."
                .to_string(),
            required: true,
        },
        Parameter {
            name: "packet_id".to_string(),
            type_hint: "integer".to_string(),
            description: "Packet identifier to echo in mqtt_puback or mqtt_pubrec. 0 for QoS 0, \
                          where no acknowledgement exists."
                .to_string(),
            required: true,
        },
        Parameter {
            name: "connected_clients".to_string(),
            type_hint: "array".to_string(),
            description: "Client ids currently connected to this server. Use these with \
                          mqtt_publish's to_client_id to forward the message to subscribers."
                .to_string(),
            required: true,
        },
    ])
    .with_actions(vec![
        MQTT_PUBACK_ACTION.clone(),
        MQTT_PUBREC_ACTION.clone(),
        MQTT_PUBLISH_ACTION.clone(),
        MQTT_CLOSE_CONNECTION_ACTION.clone(),
    ])
    .with_log_template(
        LogTemplate::new()
            .with_info("MQTT PUBLISH {topic} from {client_id}")
            .with_debug("MQTT PUBLISH: topic={topic} qos={qos} {payload_size} bytes")
            .with_trace("MQTT PUBLISH: {json_pretty(.)}"),
    )
});

/// A client subscribed to one or more topic filters.
pub static MQTT_SUBSCRIBE_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "mqtt_subscribe",
        "MQTT client subscribed to topic filters and is waiting for SUBACK",
        json!({"type": "placeholder", "event_id": "mqtt_subscribe"}),
    )
    .with_parameters(vec![
        Parameter {
            name: "client_id".to_string(),
            type_hint: "string".to_string(),
            description: "Client that subscribed.".to_string(),
            required: true,
        },
        Parameter {
            name: "packet_id".to_string(),
            type_hint: "integer".to_string(),
            description: "Packet identifier that mqtt_suback must echo.".to_string(),
            required: true,
        },
        Parameter {
            name: "topics".to_string(),
            type_hint: "array".to_string(),
            description: "Requested subscriptions, in order: [{\"filter\": \"home/#\", \
                          \"qos\": 1}]. granted_qos must have one entry per element."
                .to_string(),
            required: true,
        },
    ])
    .with_actions(vec![
        MQTT_SUBACK_ACTION.clone(),
        MQTT_PUBLISH_ACTION.clone(),
        MQTT_CLOSE_CONNECTION_ACTION.clone(),
    ])
    .with_log_template(
        LogTemplate::new()
            .with_info("MQTT SUBSCRIBE from {client_id}")
            .with_debug("MQTT SUBSCRIBE: client={client_id} id={packet_id} topics={topics}")
            .with_trace("MQTT SUBSCRIBE: {json_pretty(.)}"),
    )
});

/// A client removed one or more subscriptions.
pub static MQTT_UNSUBSCRIBE_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "mqtt_unsubscribe",
        "MQTT client unsubscribed from topic filters and is waiting for UNSUBACK",
        json!({"type": "placeholder", "event_id": "mqtt_unsubscribe"}),
    )
    .with_parameters(vec![
        Parameter {
            name: "client_id".to_string(),
            type_hint: "string".to_string(),
            description: "Client that unsubscribed.".to_string(),
            required: true,
        },
        Parameter {
            name: "packet_id".to_string(),
            type_hint: "integer".to_string(),
            description: "Packet identifier that mqtt_unsuback must echo.".to_string(),
            required: true,
        },
        Parameter {
            name: "topics".to_string(),
            type_hint: "array".to_string(),
            description: "Topic filters being removed, as strings.".to_string(),
            required: true,
        },
    ])
    .with_actions(vec![
        MQTT_UNSUBACK_ACTION.clone(),
        MQTT_CLOSE_CONNECTION_ACTION.clone(),
    ])
    .with_log_template(
        LogTemplate::new()
            .with_info("MQTT UNSUBSCRIBE from {client_id}")
            .with_debug("MQTT UNSUBSCRIBE: client={client_id} id={packet_id}")
            .with_trace("MQTT UNSUBSCRIBE: {json_pretty(.)}"),
    )
});

pub fn get_mqtt_event_types() -> Vec<EventType> {
    vec![
        MQTT_CONNECT_EVENT.clone(),
        MQTT_PUBLISH_EVENT.clone(),
        MQTT_SUBSCRIBE_EVENT.clone(),
        MQTT_UNSUBSCRIBE_EVENT.clone(),
    ]
}
