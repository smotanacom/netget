pub mod actions;
pub mod transport;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::protocol::{ConnectContext, Event};
use crate::server::ipfix::codec::Batch;
use crate::state::{
    client_handles::{ClientCommand, ClientSendOutcome},
    AccessLogOwner, ClientStatus,
};
use crate::{console_error, console_info};
pub use actions::IpfixClientProtocol;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{collections::VecDeque, future::Future, pin::Pin, sync::Arc, time::Duration};
pub const MAX_QUEUED_EVENTS: usize = 32;
pub const MAX_HANDLER_ACTIONS: usize = 32;
pub const MAX_FOLLOWUP_DEPTH: usize = 8;
type Handler = Pin<Box<dyn Future<Output = Result<crate::llm::ClientLlmResult>> + Send>>;
type SendFuture = Pin<Box<dyn Future<Output = Result<usize>> + Send>>;
struct Inflight {
    send: SendFuture,
    prepared: Option<transport::Prepared>,
    command: Option<ClientCommand>,
    depth: usize,
}
fn handler(ctx: ConnectContext, event: Event) -> Handler {
    Box::pin(async move {
        let instruction = ctx
            .state
            .get_instruction_for_client(ctx.client_id)
            .await
            .unwrap_or_default();
        let configured = ctx
            .state
            .get_client_event_handler_config(ctx.client_id)
            .await
            .is_some_and(|c| c.find_handler(event.id()).is_some());
        if instruction.is_empty() && !configured {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "IPFIX",
                    None,
                    event.id(),
                    event.data.clone(),
                    vec![],
                )
                .await;
            return Ok(crate::llm::ClientLlmResult {
                actions: vec![],
                memory_updates: None,
            });
        }
        let memory = ctx
            .state
            .get_memory_for_client(ctx.client_id)
            .await
            .unwrap_or_default();
        crate::client::llm_budget::call_llm_for_client(
            &ctx.llm_client,
            &ctx.state,
            ctx.client_id.to_string(),
            &instruction,
            &memory,
            Some(&event),
            &IpfixClientProtocol::new(),
            &ctx.status_tx,
        )
        .await
    })
}
pub struct IpfixClient;
impl IpfixClient {
    pub async fn connect(ctx: ConnectContext) -> Result<std::net::SocketAddr> {
        anyhow::ensure!(
            ctx.remote_addr.len() <= 1024,
            "IPFIX destination length bound"
        );
        let refresh = ctx
            .startup_params
            .as_ref()
            .map(|p| p.get_optional_u64("template_refresh_seconds"))
            .transpose()?
            .flatten()
            .unwrap_or(transport::DEFAULT_REFRESH_SECONDS);
        anyhow::ensure!(
            (1..=3600).contains(&refresh),
            "template refresh must be1..3600seconds"
        );
        let peer = tokio::time::timeout(
            transport::IO_TIMEOUT,
            tokio::net::lookup_host(&ctx.remote_addr),
        )
        .await
        .context("IPFIX resolve deadline")??
        .next()
        .context("collector resolved to no address")?;
        let socket = Arc::new(
            tokio::net::UdpSocket::bind(if peer.is_ipv4() {
                "0.0.0.0:0"
            } else {
                "[::]:0"
            })
            .await?,
        );
        socket.connect(peer).await?;
        let local = socket.local_addr()?;
        let mut commands =
            crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id)
                .await;
        ctx.state
            .update_client_status(ctx.client_id, ClientStatus::Connected)
            .await;
        let owner = ctx.state.clone();
        let id = ctx.client_id;
        console_info!(
            ctx.status_tx,
            "IPFIX UDP exporter {} ready for {}",
            id,
            peer
        );
        owner.spawn_client_task(id,async move{
   let protocol=IpfixClientProtocol::new();let mut catalog=transport::Catalog::default();let mut current=Some((handler(ctx.clone(),Event::new(&actions::IPFIX_CONNECTED_EVENT,json!({"remote_addr":peer.to_string(),"local_addr":local.to_string()}))),0));let mut events=VecDeque::new();let mut actions=VecDeque::new();let mut pending:Option<Inflight>=None;
   let period=Duration::from_secs(refresh);let mut refresh_tick=tokio::time::interval_at(tokio::time::Instant::now()+period,period);refresh_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);let mut incoming=[0u8;1];
   'session:loop{
    if current.is_none(){if let Some((event,depth))=events.pop_front(){current=Some((handler(ctx.clone(),event),depth));}}
    if pending.is_none(){if let Some((action,depth))=actions.pop_front(){match prepare(&protocol,action,depth,&catalog){Ok(Prepared::Write(p))=>{pending=Some(inflight(socket.clone(),p,None,depth));},Ok(Prepared::Disconnect)=>break,Err(e)=>console_error!(ctx.status_tx,"IPFIX handler action rejected: {}",e)}continue;}}
    tokio::select!{
     result=socket.recv(&mut incoming)=>{match result{Ok(_)=>console_error!(ctx.status_tx,"IPFIX collector sent unexpected UDP reply; closing"),Err(e)=>console_error!(ctx.status_tx,"IPFIX UDP transport failed: {}",e)}break;},
     result=async{pending.as_mut().unwrap().send.as_mut().await},if pending.is_some()=>{
      let p=pending.take().unwrap();match result{
       Ok(count)=>if let Some(prepared)=p.prepared{catalog.domains.insert(prepared.info.observation_domain_id,prepared.domain);if let Some(command)=p.command{finish(&ctx,command,Ok(ClientSendOutcome::Executed{detail:format!("IPFIX local UDP send: {} records",prepared.info.record_count)})).await;}
        if events.len()==MAX_QUEUED_EVENTS{console_error!(ctx.status_tx,"IPFIX export-event queue limit");break;}events.push_back((Event::new(&actions::IPFIX_EXPORTED_EVENT,serde_json::to_value(prepared.info).unwrap_or(Value::Null)),p.depth));}else{ctx.state.record_access_log(AccessLogOwner::Client(id.as_u32()),"IPFIX",None,"ipfix_template_refresh",json!({"message_count":count,"local_transport_only":true}),vec![]).await;},
       Err(e)=>{if let Some(command)=p.command{finish(&ctx,command,Err(anyhow::anyhow!("IPFIX UDP send failed"))).await;}console_error!(ctx.status_tx,"IPFIX UDP send failed: {}",e);break;}
      }
     },
     result=async{current.as_mut().unwrap().0.as_mut().await},if current.is_some()=>{
      let(_,depth)=current.take().unwrap();match result{Ok(result)=>{if let Some(memory)=result.memory_updates{ctx.state.set_memory_for_client(id,memory).await;}
       if result.actions.len()>MAX_HANDLER_ACTIONS||actions.len()+result.actions.len()>MAX_HANDLER_ACTIONS{console_error!(ctx.status_tx,"IPFIX handler action count limit");break;}actions.extend(result.actions.into_iter().map(|a|(a,depth+1)));},Err(e)=>console_error!(ctx.status_tx,"IPFIX handler failed: {}",e)}
     },
     command=commands.recv()=>{let Some(command)=command else{break;};match prepare(&protocol,command.action.clone(),0,&catalog){Ok(Prepared::Disconnect)=>{finish(&ctx,command,Ok(ClientSendOutcome::Disconnected)).await;break 'session;},Ok(Prepared::Write(p))if pending.is_none()=>pending=Some(inflight(socket.clone(),p,Some(command),0)),Ok(Prepared::Write(_))=>finish(&ctx,command,Ok(ClientSendOutcome::Rejected{error:"One UDP send is already in flight".into()})).await,Err(e)=>finish(&ctx,command,Ok(ClientSendOutcome::Rejected{error:e.to_string()})).await}},
     _=refresh_tick.tick(),if pending.is_none()&&!catalog.domains.is_empty()=>{match catalog.refresh(){Ok(messages)=>pending=Some(Inflight{send:Box::pin(transport::send(socket.clone(),messages)),prepared:None,command:None,depth:0}),Err(e)=>{console_error!(ctx.status_tx,"IPFIX template refresh failed: {}",e);break;}}},
    }let _=ctx.status_tx.send("__UPDATE_UI__".into());
   }
   if let Some(p)=pending.take(){if let Some(command)=p.command{finish(&ctx,command,Err(anyhow::anyhow!("IPFIX UDP send cancelled"))).await;}}
   current.take();events.clear();actions.clear();
   ctx.state.remove_client_handle(id).await;ctx.state.update_client_status(id,ClientStatus::Disconnected).await;let _=ctx.status_tx.send("__UPDATE_UI__".into());
  }).await;
        Ok(local)
    }
}
enum Prepared {
    Disconnect,
    Write(transport::Prepared),
}
fn prepare(
    protocol: &IpfixClientProtocol,
    action: Value,
    depth: usize,
    catalog: &transport::Catalog,
) -> Result<Prepared> {
    match protocol.execute_action(action)? {
        ClientActionResult::Disconnect => Ok(Prepared::Disconnect),
        ClientActionResult::Custom { name, data } if name == "export_ipfix_records" => {
            anyhow::ensure!(depth <= MAX_FOLLOWUP_DEPTH, "IPFIX followup depth bound8");
            let batch: Batch = serde_json::from_value(data)?;
            Ok(Prepared::Write(catalog.prepare(&batch)?))
        }
        _ => anyhow::bail!("unsupported IPFIX action result"),
    }
}
fn inflight(
    socket: Arc<tokio::net::UdpSocket>,
    p: transport::Prepared,
    command: Option<ClientCommand>,
    depth: usize,
) -> Inflight {
    let bytes = p.bytes.clone();
    Inflight {
        send: Box::pin(transport::send(socket, vec![bytes])),
        prepared: Some(p),
        command,
        depth,
    }
}
async fn finish(ctx: &ConnectContext, command: ClientCommand, result: Result<ClientSendOutcome>) {
    let outcome = match &result {
        Ok(outcome) => serde_json::to_value(outcome).unwrap_or(Value::Null),
        Err(e) => json!({"error":e.to_string()}),
    };
    ctx.state
        .record_access_log(
            AccessLogOwner::Client(ctx.client_id.as_u32()),
            "IPFIX",
            None,
            "injected_action",
            command.action.clone(),
            vec![outcome],
        )
        .await;
    crate::client::command_support::reply(command, result);
}
