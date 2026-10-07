pub mod actions;
use crate::server::opcua::codec;
use crate::{
    protocol::{ConnectContext, Event},
    state::{AccessLogOwner, ClientStatus},
};
use ::opcua::{
    client::{ClientBuilder, DataChangeCallback, IdentityToken, MonitoredItem, Session},
    types::*,
};
use anyhow::{ensure, Result};
use serde_json::Value;
use std::{net::SocketAddr, sync::Arc, time::Duration};
struct Bridge {
    updates: tokio::sync::mpsc::Receiver<serde_json::Value>,
    notify: tokio::sync::mpsc::Sender<serde_json::Value>,
    subscriptions: usize,
}
impl Bridge {
    async fn idle(&mut self, s: &mut Arc<Session>) -> Result<Option<serde_json::Value>> {
        let _ = s;
        Ok(self.updates.recv().await)
    }
    async fn exchange(
        &mut self,
        s: &mut Arc<Session>,
        v: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        use serde_json::json;
        codec::validate(v)?;
        match v["type"].as_str() {
            Some("opcua_read") => {
                let nodes = [ReadValueId {
                    node_id: codec::node(v, "node_id")?,
                    attribute_id: AttributeId::Value as u32,
                    ..Default::default()
                }];
                let values = s.read(&nodes, TimestampsToReturn::Both, 0.0).await?;
                let value = values
                    .first()
                    .ok_or_else(|| anyhow::anyhow!("missing read result"))?;
                let mut out =
                    json!({"success":value.status().is_good(),"status":value.status().bits()});
                if let Some(val) = value.value.as_ref().filter(|_| value.status().is_good()) {
                    let decoded = codec::structured(val)?;
                    out["value_type"] = decoded["value_type"].clone();
                    out["value"] = decoded["value"].clone();
                }
                Ok(out)
            }
            Some("opcua_write") => {
                let values = s
                    .write(&[WriteValue {
                        node_id: codec::node(v, "node_id")?,
                        attribute_id: AttributeId::Value as u32,
                        value: DataValue::new_now(codec::value(v)?),
                        ..Default::default()
                    }])
                    .await?;
                let status = *values
                    .first()
                    .ok_or_else(|| anyhow::anyhow!("missing write result"))?;
                Ok(json!({"success":status.is_good(),"status":status.bits()}))
            }
            Some("opcua_browse") => {
                let results = s
                    .browse(
                        &[BrowseDescription {
                            node_id: codec::node(v, "node_id")?,
                            browse_direction: BrowseDirection::Forward,
                            include_subtypes: true,
                            result_mask: 63,
                            ..Default::default()
                        }],
                        100,
                        None,
                    )
                    .await?;
                let r = results
                    .first()
                    .ok_or_else(|| anyhow::anyhow!("missing browse result"))?;
                let refs=r.references.as_deref().unwrap_or_default().iter().map(|r|json!({"node_id":r.node_id.node_id.to_string(),"browse_name":r.browse_name.name.as_ref(),"node_class":r.node_class as u32})).collect::<Vec<_>>();
                if !r.continuation_point.is_null() {
                    s.browse_next(true, std::slice::from_ref(&r.continuation_point))
                        .await?;
                }
                Ok(
                    json!({"success":r.status_code.is_good(),"status":r.status_code.bits(),"references":refs,"truncated":!r.continuation_point.is_null()}),
                )
            }
            Some("opcua_call") => {
                let args = v["arguments"]
                    .as_array()
                    .expect("validated")
                    .iter()
                    .map(codec::value)
                    .collect::<Result<Vec<_>>>()?;
                let r = s
                    .call_one(CallMethodRequest {
                        object_id: codec::node(v, "object_id")?,
                        method_id: codec::node(v, "method_id")?,
                        input_arguments: Some(args),
                    })
                    .await?;
                let outputs = r
                    .output_arguments
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .map(codec::structured)
                    .collect::<Result<Vec<_>>>()?;
                Ok(
                    json!({"success":r.status_code.is_good(),"status":r.status_code.bits(),"outputs":outputs}),
                )
            }
            Some("opcua_subscribe") => {
                ensure!(self.subscriptions < 10, "subscription limit 10");
                let tx = self.notify.clone();
                let subscription = s
                    .create_subscription(
                        Duration::from_millis(100),
                        30,
                        10,
                        100,
                        0,
                        true,
                        DataChangeCallback::new(move |value: DataValue, item: &MonitoredItem| {
                            if let Some(v) = value.value.as_ref() {
                                if let Ok(mut out) = codec::structured(v) {
                                    out["node_id"] =
                                        json!(item.item_to_monitor().node_id.to_string());
                                    out["status"] = json!(value.status().bits());
                                    out["success"] = json!(value.status().is_good());
                                    let _ = tx.try_send(out);
                                }
                            }
                        }),
                    )
                    .await?;
                let results = s
                    .create_monitored_items(
                        subscription,
                        TimestampsToReturn::Both,
                        vec![codec::node(v, "node_id")?.into()],
                    )
                    .await?;
                let r = results
                    .first()
                    .ok_or_else(|| anyhow::anyhow!("missing monitored item result"))?;
                self.subscriptions += 1;
                Ok(
                    json!({"success":r.result.status_code.is_good(),"status":r.result.status_code.bits(),"subscription_id":subscription}),
                )
            }
            _ => unreachable!(),
        }
    }
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let protocol: Arc<dyn crate::llm::actions::client_trait::Client> =
        Arc::new(actions::OpcuaClientProtocol);
    let kinds = &*actions::EVENTS;
    let endpoint_url = if ctx.remote_addr.starts_with("opc.tcp://") {
        ctx.remote_addr.clone()
    } else {
        format!("opc.tcp://{}/", ctx.remote_addr)
    };
    let mut client = ClientBuilder::new()
        .application_name("NetGet scanner")
        .application_uri("urn:netget:scanner")
        .max_message_size(codec::MAX_FRAME)
        .max_chunk_count(16)
        .max_array_length(1024)
        .max_string_length(4096)
        .max_byte_string_length(4096)
        .session_retry_limit(0)
        .client()
        .map_err(|e| anyhow::anyhow!("{}", e.join("; ")))?;
    let endpoint: EndpointDescription = (
        endpoint_url.as_str(),
        "None",
        MessageSecurityMode::None,
        UserTokenPolicy::anonymous(),
    )
        .into();
    let (mut stream, event_loop) = tokio::time::timeout(
        Duration::from_secs(10),
        client.connect_to_matching_endpoint(endpoint, IdentityToken::Anonymous),
    )
    .await??;
    ctx.state
        .spawn_client_task(ctx.client_id, async move {
            let _ = event_loop.run().await;
        })
        .await;
    ensure!(
        tokio::time::timeout(Duration::from_secs(10), stream.wait_for_connection()).await?,
        "UA session failed"
    );
    let (notify, updates) = tokio::sync::mpsc::channel(32);
    let mut session = Bridge {
        updates,
        notify,
        subscriptions: 0,
    };
    let local = "0.0.0.0:0".parse()?;
    let mut commands =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let state = ctx.state.clone();
    let cid = ctx.client_id;
    state
        .spawn_client_task(cid, async move {
            // Handler work has its own task so an unanswered manual turn does not block injected commands.
            let (events_tx, mut events_rx) = tokio::sync::mpsc::channel::<Event>(32);
            let (actions_tx, mut actions_rx) = tokio::sync::mpsc::channel::<Value>(32);
            let event_ctx = ctx.clone();
            let p = protocol.clone();
            let turns = ctx
                .state
                .spawn_client_task(cid, async move {
                    while let Some(event) = events_rx.recv().await {
                        let instruction = event_ctx
                            .state
                            .get_instruction_for_client(cid)
                            .await
                            .unwrap_or_default();
                        let memory = event_ctx
                            .state
                            .get_memory_for_client(cid)
                            .await
                            .unwrap_or_default();
                        match crate::client::llm_budget::call_llm_for_client(
                            &event_ctx.llm_client,
                            &event_ctx.state,
                            cid.to_string(),
                            &instruction,
                            &memory,
                            Some(&event),
                            p.as_ref(),
                            &event_ctx.status_tx,
                        )
                        .await
                        {
                            Ok(result) => {
                                if let Some(memory) = result.memory_updates {
                                    event_ctx.state.set_memory_for_client(cid, memory).await;
                                }
                                for a in result.actions {
                                    if actions_tx.send(a).await.is_err() {
                                        return;
                                    }
                                }
                            }
                            Err(e) => crate::logging::emit::Log::new(Some(&event_ctx.status_tx))
                                .warn(format!("{} client handler: {e}", p.protocol_name())),
                        }
                    }
                })
                .await;
            let _ = events_tx.try_send(Event::new(
                &kinds[0],
                serde_json::json!({"remote_addr":ctx.remote_addr}),
            ));
            loop {
                let (action, command) = tokio::select! {
                    Some(c) = commands.recv() => (c.action.clone(),Some(c)),
                    Some(a) = actions_rx.recv() => (a,None),
                    incoming = session.idle(&mut stream) => {match incoming {Ok(Some(response))=>{let _=events_tx.try_send(Event::new(&kinds[1],response));},Ok(None)=>{},Err(_)=>break};continue;},
                    else => break,
                };
                if action["type"] == "disconnect" {
                    if let Some(c) = command {
                        crate::client::command_support::reply(
                            c,
                            Ok(crate::state::client_handles::ClientSendOutcome::Disconnected),
                        );
                    }
                    break;
                }
                if let Err(e) = protocol.execute_action(action.clone()) {
                    if let Some(c) = command {
                        crate::client::command_support::reply(
                            c,
                            Ok(crate::state::client_handles::ClientSendOutcome::Rejected {
                                error: e.to_string(),
                            }),
                        );
                    }
                    continue;
                }
                let result = match tokio::time::timeout(
                    Duration::from_secs(10),
                    session.exchange(&mut stream, &action),
                )
                .await
                {
                    Ok(r) => r,
                    Err(_) => Err(anyhow::anyhow!("device response deadline exceeded")),
                };
                let terminal = result.is_err();
                match result {
                    Ok(response) => {
                        ctx.state
                            .record_access_log(
                                AccessLogOwner::Client(cid.as_u32()),
                                protocol.protocol_name(),
                                None,
                                &kinds[1].id,
                                response.clone(),
                                vec![],
                            )
                            .await;
                        let _ = events_tx.try_send(Event::new(&kinds[1], response.clone()));
                        if let Some(c) = command {
                            crate::client::command_support::reply(
                                c,
                                Ok(crate::state::client_handles::ClientSendOutcome::Executed {
                                    detail: response.to_string(),
                                }),
                            );
                        }
                    }
                    Err(e) => {
                        if let Some(c) = command {
                            crate::client::command_support::reply(c, Err(e));
                        }
                    }
                }
                if terminal {
                    break;
                }
            }
            turns.abort();
            let _=tokio::time::timeout(Duration::from_secs(5),stream.disconnect()).await;

            ctx.state.remove_client_handle(cid).await;
            ctx.state
                .update_client_status(cid, ClientStatus::Disconnected)
                .await;
        })
        .await;
    Ok(local)
}
