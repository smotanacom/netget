pub mod actions;
pub mod codec;
use crate::protocol::SpawnContext;
use anyhow::{ensure, Result};
use std::{net::SocketAddr, sync::Arc};
pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let types = ctx
        .startup_params
        .as_ref()
        .map(|p| p.get_optional_array("attribute_types"))
        .transpose()?
        .flatten()
        .cloned()
        .unwrap_or_default();
    ensure!(types.len() <= 128, "at most 128 attribute schemas");
    for t in &types {
        let mut a = t.clone();
        a["type"] = serde_json::json!("ethernet_ip_get");
        codec::validate(&a)?;
    }
    // Acquire both sockets before starting any task. A UDP port conflict must not leave
    // the TCP adapter alive after startup fails.
    let listener =
        crate::server::socket_helpers::create_reusable_tcp_listener(ctx.legacy_listen_addr())
            .await?;
    let addr = listener.local_addr()?;
    let socket = tokio::net::UdpSocket::bind(addr).await?;
    let schemas = types.clone();
    let addr = crate::server::ics_support::spawn_accept_bounded(
        ctx.clone(),
        Arc::new(actions::EthernetIpProtocol),
        move || {
            let mut device = codec::Device::default();
            device.types = schemas.clone();
            device
        },
        &actions::EVENTS,
        listener,
    )
    .await?;
    let state = ctx.state.clone();
    let sid = ctx.server_id;
    state
        .spawn_server_task(sid, async move {
            let mut buf = [0; codec::MAX_FRAME + 1];
            while let Ok((n, peer)) = socket.recv_from(&mut buf).await {
                if n == 24
                    && buf[0..4] == [0x63, 0, 0, 0]
                    && buf[4..12] == [0; 8]
                    && buf[20..24] == [0; 4]
                {
                    let mut context = [0; 8];
                    context.copy_from_slice(&buf[12..20]);
                    let response = codec::encapsulate(0x63, 0, context, 0, &codec::identity(addr));
                    let _ = socket.send_to(&response, peer).await;
                }
            }
        })
        .await;
    Ok(addr)
}
