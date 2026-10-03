//! Receive/parse ownership stays independent of parked common handlers.
pub mod actions;
pub mod codec;
use crate::protocol::{Event, SpawnContext};
use crate::server::connection::ConnectionId;
use crate::state::{
    server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo},
    AccessLogOwner,
};
use crate::{console_error, console_info};
use anyhow::{ensure, Result};
use serde_json::json;
use std::{net::SocketAddr, time::Duration};
pub const MAX_QUEUED_EVENTS: usize = 32;
pub struct IpfixServer;
pub fn duration(value: u64) -> Result<Duration> {
    ensure!(
        (1..=86400).contains(&value),
        "IPFIX duration must be1..86400seconds"
    );
    Ok(Duration::from_secs(value))
}
impl IpfixServer {
    pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
        let params = ctx.startup_params.as_ref();
        let ttl = duration(
            params
                .map(|p| p.get_optional_u64("template_ttl_seconds"))
                .transpose()?
                .flatten()
                .unwrap_or(codec::DEFAULT_TEMPLATE_TTL),
        )?;
        let idle = duration(
            params
                .map(|p| p.get_optional_u64("session_idle_seconds"))
                .transpose()?
                .flatten()
                .unwrap_or(codec::DEFAULT_SESSION_IDLE),
        )?;
        let llm = params
            .map(|p| p.get_optional_bool("llm_fallback"))
            .transpose()?
            .flatten()
            .unwrap_or(codec::DEFAULT_LLM_FALLBACK);
        let socket = tokio::net::UdpSocket::bind(ctx.legacy_listen_addr()).await?;
        let local = socket.local_addr()?;
        console_info!(
            ctx.status_tx,
            "IPFIX UDP listening on {} (llm_fallback={})",
            local,
            llm
        );
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<(SocketAddr, usize, codec::Message)>(MAX_QUEUED_EVENTS);
        let dispatcher = ctx.clone();
        let owner = ctx.state.clone();
        let id = ctx.server_id;
        owner.spawn_server_task(id,async move{while let Some((peer,n,message))=rx.recv().await{
   let cid=ConnectionId::new(dispatcher.state.get_next_unified_id().await);let now=crate::utils::clock::Instant::now();dispatcher.state.add_connection_to_server(id,ConnectionState{id:cid,remote_addr:peer,local_addr:local,bytes_sent:0,bytes_received:n as u64,packets_sent:0,packets_received:1,last_activity:now,status:ConnectionStatus::Active,status_changed_at:now,protocol_info:ProtocolConnectionInfo::empty()}).await;
   let event=Event::new(&actions::IPFIX_MESSAGE_EVENT,json!({"message":message,"source_addr":peer.to_string()}));let configured=dispatcher.state.get_event_handler_config(id).await.is_some_and(|c|c.find_handler("ipfix_message").is_some());
   if llm||configured{match crate::llm::action_helper::call_llm(&dispatcher.llm_client,&dispatcher.state,id,Some(cid),&event,&actions::IpfixProtocol::new()).await{
    Ok(result)if result.failures.is_empty()=>{for m in result.messages{console_info!(dispatcher.status_tx,"{}",m);}},
    result=>{let reason=match result{Ok(r)=>format!("{} failed actions",r.failures.len()),Err(e)=>e.to_string()};dispatcher.state.record_access_log(AccessLogOwner::Server(id.as_u32()),"IPFIX",Some(cid.as_u32()),"ipfix_handler_failed",event.data,vec![json!({"decision":"fail_closed_handler_error","error":reason})]).await;}
   }}else{dispatcher.state.record_access_log(AccessLogOwner::Server(id.as_u32()),"IPFIX",Some(cid.as_u32()),"ipfix_message",event.data,vec![json!({"type":"collect_ipfix_records"})]).await;}
   dispatcher.state.remove_connection_from_server(id,cid).await;let _=dispatcher.status_tx.send("__UPDATE_UI__".into());
  }}).await;
        owner.spawn_server_task(id,async move{
   let mut cache=codec::TemplateCache::new(ttl,idle);let mut buffer=vec![0;codec::MAX_MESSAGE_BYTES+1];let mut expiry=tokio::time::interval(Duration::from_secs(1));expiry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
   loop{let(n,peer)=tokio::select!{_=expiry.tick()=>{cache.expire(tokio::time::Instant::now());continue;},r=socket.recv_from(&mut buffer)=>match r{Ok(v)=>v,Err(e)=>{console_error!(ctx.status_tx,"IPFIX receive failed: {}",e);break;}}};
    match cache.ingest(peer,&buffer[..n],tokio::time::Instant::now()){
     Ok(message)=>if let Err(error)=tx.try_send((peer,n,message)){let closed=matches!(error,tokio::sync::mpsc::error::TrySendError::Closed(_));ctx.state.record_access_log(AccessLogOwner::Server(id.as_u32()),"IPFIX",None,"ipfix_event_capacity",json!({"source_addr":peer.to_string()}),vec![json!({"decision":"fail_closed_event_capacity","event_cap":MAX_QUEUED_EVENTS})]).await;if closed{break;}},
     Err(error)=>{ctx.state.record_access_log(AccessLogOwner::Server(id.as_u32()),"IPFIX",None,"ipfix_invalid_datagram",json!({"source_addr":peer.to_string(),"received_bytes":n}),vec![json!({"decision":"fail_closed_invalid_datagram","error":error.to_string()})]).await;},
    }
   }
  }).await;
        Ok(local)
    }
}
