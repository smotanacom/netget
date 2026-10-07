pub mod actions;
use crate::protocol::ConnectContext;
use anyhow::Result;
use std::{net::SocketAddr, sync::Arc};
#[derive(Default)]
struct Session {
    invoke: u8,
}
#[async_trait::async_trait]
impl crate::server::ics_support::DatagramSession for Session {
    async fn exchange(
        &mut self,
        s: &mut tokio::net::UdpSocket,
        a: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        use crate::server::bacnet::codec;
        self.invoke = self.invoke.wrapping_add(1);
        s.send(&codec::request(a, self.invoke)?).await?;
        let mut b = [0; codec::MAX_FRAME + 1];
        let n = s.recv(&mut b).await?;
        codec::response(codec::unwrap(&b[..n])?, a, self.invoke)
    }
}
pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    crate::server::ics_support::connect_udp(
        ctx,
        Arc::new(actions::BacnetClientProtocol),
        Session::default(),
        &actions::EVENTS,
    )
    .await
}
