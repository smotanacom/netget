pub mod actions;
pub async fn connect(ctx: crate::protocol::ConnectContext) -> anyhow::Result<std::net::SocketAddr> {
    crate::server::p2p_support::connect(
        ctx,
        std::sync::Arc::new(actions::SoulseekPeerClientProtocol),
        crate::server::soulseek_peer::codec::Scanner::default(),
        [&actions::EVENTS[0], &actions::EVENTS[1]],
    )
    .await
}
