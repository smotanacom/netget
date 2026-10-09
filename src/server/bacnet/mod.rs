pub mod actions;
pub mod codec;
use crate::{
    llm::actions::protocol_trait::ActionResult,
    protocol::{Event, SpawnContext},
};
use anyhow::{ensure, Result};
use std::{net::SocketAddr, sync::Arc, time::Duration};
pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let device = ctx
        .startup_params
        .as_ref()
        .map(|p| p.get_optional_u32("device_id"))
        .transpose()?
        .flatten()
        .unwrap_or(codec::DEFAULT_DEVICE_ID);
    ensure!(device <= 4194303, "device id out of range");
    let socket = Arc::new(tokio::net::UdpSocket::bind(ctx.legacy_listen_addr()).await?);
    let addr = socket.local_addr()?;
    let state = ctx.state.clone();
    let sid = ctx.server_id;
    state
        .spawn_server_task(sid, async move {
            let cap = Arc::new(tokio::sync::Semaphore::new(256));
            let mut buf = [0; codec::MAX_FRAME + 1];
            while let Ok((n, peer)) = socket.recv_from(&mut buf).await {
                let Ok(a) = codec::unwrap(&buf[..n]) else {
                    continue;
                };
                let Ok((automatic, request)) = codec::receive(a, device) else {
                    continue;
                };
                if !automatic.is_empty() {
                    let _ = socket.send_to(&automatic, peer).await;
                }
                let Some(request) = request else { continue };
                let Ok(permit) = cap.clone().try_acquire_owned() else {
                    continue;
                };
                let c = ctx.clone();
                let socket = socket.clone();
                ctx.state
                    .spawn_server_task(sid, async move {
                        let _permit = permit;
                        let event = Event::new(&actions::EVENTS[0], request.clone());
                        let result = tokio::time::timeout(
                            Duration::from_secs(10),
                            crate::llm::action_helper::call_llm(
                                &c.llm_client,
                                &c.state,
                                sid,
                                None,
                                &event,
                                &actions::BacnetProtocol,
                            ),
                        )
                        .await;
                        let tag = match &result {
                            Err(_) | Ok(Err(_)) => "decision=fail_closed_llm_error",
                            Ok(Ok(r)) if r.protocol_results.is_empty() => "decision=model_silent",
                            _ => "decision=model_answer",
                        };
                        crate::logging::emit::Log::new(Some(&c.status_tx))
                            .debug(format!("BACnet/IP {tag}"));
                        let answer =
                            result
                                .as_ref()
                                .ok()
                                .and_then(|r| r.as_ref().ok())
                                .and_then(|r| {
                                    r.protocol_results.iter().find_map(|a| {
                                        if let ActionResult::Custom { data, .. } = a {
                                            Some(data)
                                        } else {
                                            None
                                        }
                                    })
                                });
                        if let Ok(b) = codec::answer(&request, answer) {
                            let _ = socket.send_to(&b, peer).await;
                        }
                    })
                    .await;
            }
        })
        .await;
    Ok(addr)
}
