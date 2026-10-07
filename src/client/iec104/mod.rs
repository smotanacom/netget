pub mod actions;
use crate::protocol::ConnectContext;
use anyhow::Result;
use std::{net::SocketAddr, sync::Arc};
pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    crate::server::ics_support::connect(
        ctx,
        Arc::new(actions::Iec104ClientProtocol),
        crate::server::iec104::codec::Scanner::default(),
        &actions::EVENTS,
    )
    .await
}
