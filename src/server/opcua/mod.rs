pub mod actions;
pub mod codec;
use crate::protocol::{Event, SpawnContext};
use ::opcua::{
    core::sync::RwLock,
    server::{
        address_space::{AddressSpace, MethodBuilder, VariableBuilder},
        diagnostics::NamespaceMetadata,
        node_manager::{
            memory::{InMemoryNodeManagerBuilder, InMemoryNodeManagerImpl},
            MethodCall, ParsedReadValueId, RequestContext, ServerContext, WriteNode,
        },
        ServerBuilder,
    },
    types::*,
};
use anyhow::Result;
use std::{net::SocketAddr, time::Duration};
struct Manager {
    ctx: SpawnContext,
    ns: u16,
}
impl Manager {
    async fn decision(&self, v: serde_json::Value) -> Option<serde_json::Value> {
        let event = Event::new(&actions::EVENTS[0], v);
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            crate::llm::action_helper::call_llm(
                &self.ctx.llm_client,
                &self.ctx.state,
                self.ctx.server_id,
                None,
                &event,
                &actions::OpcuaProtocol,
            ),
        )
        .await;
        let tag = match &result {
            Err(_) | Ok(Err(_)) => "decision=fail_closed_llm_error",
            Ok(Ok(r)) if r.protocol_results.is_empty() => "decision=model_silent",
            _ => "decision=model_answer",
        };
        crate::logging::emit::Log::new(Some(&self.ctx.status_tx)).debug(format!("OPC UA {tag}"));
        let r = result.ok()?.ok()?;
        r.protocol_results.into_iter().find_map(|a| {
            if let crate::llm::actions::protocol_trait::ActionResult::Custom { data, .. } = a {
                Some(data)
            } else {
                None
            }
        })
    }
}
fn status(a: &serde_json::Value) -> StatusCode {
    match a["status"].as_str() {
        Some("type_mismatch") => StatusCode::BadTypeMismatch,
        Some("unknown") => StatusCode::BadNodeIdUnknown,
        Some("unsupported") => StatusCode::BadNotSupported,
        _ => StatusCode::BadUserAccessDenied,
    }
}
#[async_trait::async_trait]
impl InMemoryNodeManagerImpl for Manager {
    fn name(&self) -> &str {
        "NetGet device"
    }
    fn namespaces(&self) -> Vec<NamespaceMetadata> {
        vec![NamespaceMetadata {
            namespace_uri: "urn:netget:device".into(),
            namespace_index: self.ns,
            ..Default::default()
        }]
    }
    async fn init(&self, a: &mut AddressSpace, _: ServerContext) {
        let ns = self.ns;
        let folder = NodeId::new(ns, "Device");
        a.add_folder(
            &folder,
            QualifiedName::new(ns, "Device"),
            "Device",
            &NodeId::objects_folder_id(),
        );
        VariableBuilder::new(
            &NodeId::new(ns, "Value"),
            QualifiedName::new(ns, "Value"),
            "Value",
        )
        .value(0f64)
        .data_type(DataTypeId::Double)
        .writable()
        .organized_by(folder.clone())
        .insert(a);
        MethodBuilder::new(
            &NodeId::new(ns, "Method"),
            QualifiedName::new(ns, "Method"),
            "Method",
        )
        .component_of(folder)
        .input_args(
            a,
            &NodeId::new(ns, "InputArguments"),
            &[("Input", DataTypeId::Double).into()],
        )
        .output_args(
            a,
            &NodeId::new(ns, "OutputArguments"),
            &[("Output", DataTypeId::Double).into()],
        )
        .insert(a);
    }
    async fn read_values(
        &self,
        c: &RequestContext,
        a: &RwLock<AddressSpace>,
        nodes: &[&ParsedReadValueId],
        age: f64,
        ts: TimestampsToReturn,
    ) -> Vec<DataValue> {
        let mut out = vec![];
        for n in nodes {
            if n.node_id != NodeId::new(self.ns, "Value") {
                out.push(a.read().read(c, n, age, ts));
                continue;
            }
            let r = self
                .decision(serde_json::json!({"operation":"read","node_id":n.node_id.to_string()}))
                .await;
            let v = r
                .as_ref()
                .filter(|a| a["type"] == "opcua_reply" && a.get("status").is_none())
                .and_then(|a| codec::value(a).ok())
                .filter(|v| matches!(v, Variant::Double(_)));
            out.push(v.map(DataValue::new_now).unwrap_or_else(|| {
                DataValue {
                    status: Some(
                        r.as_ref()
                            .map(status)
                            .unwrap_or(StatusCode::BadUserAccessDenied),
                    ),
                    ..Default::default()
                }
            }));
        }
        out
    }
    async fn write(
        &self,
        c: &RequestContext,
        _: &RwLock<AddressSpace>,
        nodes: &mut [&mut WriteNode],
    ) -> std::result::Result<(), StatusCode> {
        for n in nodes {
            let v = n.value();
            if v.node_id != NodeId::new(self.ns, "Value") || v.attribute_id != AttributeId::Value {
                n.set_status(StatusCode::BadNotWritable);
                continue;
            }
            let Some(Variant::Double(value)) = v.value.value.as_ref() else {
                n.set_status(StatusCode::BadTypeMismatch);
                continue;
            };
            if !value.is_finite() {
                n.set_status(StatusCode::BadTypeMismatch);
                continue;
            }
            let r=self.decision(serde_json::json!({"operation":"write","node_id":v.node_id.to_string(),"value_type":"double","value":value})).await;
            let approved = r
                .as_ref()
                .is_some_and(|a| a["type"] == "opcua_reply" && a["accepted"] == true);
            if approved {
                c.subscriptions.notify_data_change(std::iter::once((
                    v.value.clone(),
                    &v.node_id,
                    AttributeId::Value,
                )));
                n.set_status(StatusCode::Good);
            } else {
                n.set_status(
                    r.as_ref()
                        .map(status)
                        .unwrap_or(StatusCode::BadUserAccessDenied),
                );
            }
        }
        Ok(())
    }
    async fn call(
        &self,
        _: &RequestContext,
        _: &RwLock<AddressSpace>,
        methods: &mut [&mut &mut MethodCall],
    ) -> std::result::Result<(), StatusCode> {
        for m in methods {
            if m.method_id() != &NodeId::new(self.ns, "Method")
                || m.object_id() != &NodeId::new(self.ns, "Device")
            {
                m.set_status(StatusCode::BadMethodInvalid);
                continue;
            }
            let args = m
                .arguments()
                .iter()
                .map(codec::structured)
                .collect::<Result<Vec<_>>>();
            let Ok(args) = args else {
                m.set_status(StatusCode::BadTypeMismatch);
                continue;
            };
            let r=self.decision(serde_json::json!({"operation":"call","node_id":m.method_id().to_string(),"arguments":args})).await;
            let outputs = r
                .as_ref()
                .filter(|a| a["type"] == "opcua_reply" && a.get("status").is_none())
                .and_then(|a| a["outputs"].as_array())
                .and_then(|a| {
                    if a.len() != 1 {
                        return None;
                    }
                    a.iter().map(codec::value).collect::<Result<Vec<_>>>().ok()
                })
                .filter(|a| matches!(a[0], Variant::Double(_)));
            if let Some(outputs) = outputs {
                m.set_outputs(outputs);
                m.set_status(StatusCode::Good);
            } else {
                m.set_status(
                    r.as_ref()
                        .map(status)
                        .unwrap_or(StatusCode::BadUserAccessDenied),
                );
            }
        }
        Ok(())
    }
}
pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let listener =
        crate::server::socket_helpers::create_reusable_tcp_listener(ctx.legacy_listen_addr())
            .await?;
    let addr = listener.local_addr()?;
    let state = ctx.state.clone();
    let sid = ctx.server_id;
    let manager = InMemoryNodeManagerBuilder::new(move |c: ServerContext, a: &mut AddressSpace| {
        let ns = c
            .type_tree
            .write()
            .namespaces_mut()
            .add_namespace("urn:netget:device");
        a.add_namespace("urn:netget:device", ns);
        Manager { ctx, ns }
    });
    let (mut server, _handle) = ServerBuilder::new_anonymous("NetGet device")
        .host(addr.ip().to_string())
        .port(addr.port())
        .application_uri("urn:netget:device-server")
        .max_message_size(codec::MAX_FRAME)
        .max_chunk_count(16)
        .max_array_length(1024)
        .max_string_length(4096)
        .max_byte_string_length(4096)
        .hello_timeout(30)
        .max_sessions(20)
        .max_session_timeout_ms(60000)
        .with_node_manager(manager)
        .build()
        .map_err(anyhow::Error::msg)?;
    let observed = state.clone();
    server.set_connection_hook(move |peer,local,control| {let state=observed.clone();Box::pin(async move {
        use crate::server::connection::ConnectionId;
        let id=ConnectionId::new(state.get_next_unified_id().await);let now=crate::utils::clock::Instant::now();
        state.add_connection_to_server(sid,crate::state::server::ConnectionState{id,remote_addr:peer,local_addr:local,bytes_sent:0,bytes_received:0,packets_sent:0,packets_received:0,last_activity:now,status:crate::state::server::ConnectionStatus::Active,status_changed_at:now,protocol_info:crate::state::server::ProtocolConnectionInfo::empty()}).await;
        let mut commands=crate::server::peer_support::register_peer_channel(&state,sid,id.as_u32()).await;
        let owner=state.clone();state.spawn_server_task(sid,async move {loop {tokio::select! {
            _=control.closed()=>break,
            Some(command)=commands.recv()=>{if command.action["type"]=="disconnect"{control.close().await;crate::client::command_support::reply(command,Ok(crate::state::client_handles::ClientSendOutcome::Disconnected));}else{crate::client::command_support::reply(command,Ok(crate::state::client_handles::ClientSendOutcome::Rejected{error:"UA replies require a pending handler request".into()}));}}
        }}owner.remove_peer_handle(sid,id.as_u32()).await;owner.update_connection_status(sid,id,crate::state::server::ConnectionStatus::Closed).await;}).await;
    })});
    state
        .spawn_server_task(sid, async move {
            let _ = server.run_with(listener).await;
        })
        .await;
    Ok(addr)
}
