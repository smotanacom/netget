pub mod actions;
pub async fn connect(ctx: crate::protocol::ConnectContext) -> anyhow::Result<std::net::SocketAddr> {
    let name = ctx
        .startup_params
        .as_ref()
        .map(|p| p.get_optional_string("nickname"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| crate::server::dc_peer::codec::DEFAULT_CONNECTOR_NICKNAME.into());
    crate::server::dc_peer::codec::nickname(&name)?;
    crate::server::p2p_support::connect(
        ctx,
        std::sync::Arc::new(actions::DcPeerClientProtocol),
        crate::server::dc_peer::codec::Scanner::with_nickname(name),
        [&actions::EVENTS[0], &actions::EVENTS[1]],
    )
    .await
}
