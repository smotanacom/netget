pub mod actions;
pub async fn connect(ctx: crate::protocol::ConnectContext) -> anyhow::Result<std::net::SocketAddr> {
    let scanner = crate::server::adc_peer::codec::Scanner::with_identity(
        ctx.startup_params
            .as_ref()
            .map(|p| p.get_optional_string("cid"))
            .transpose()?
            .flatten(),
        ctx.startup_params
            .as_ref()
            .map(|p| p.get_optional_string("token"))
            .transpose()?
            .flatten(),
    )?;
    crate::server::p2p_support::connect(
        ctx,
        std::sync::Arc::new(actions::AdcPeerClientProtocol),
        scanner,
        [&actions::EVENTS[0], &actions::EVENTS[1]],
    )
    .await
}
