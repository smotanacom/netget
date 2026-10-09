//! Typed Connect RPC binding for the shared owned HTTP/1.1 client.
pub mod actions;
pub struct ConnectRpcClient;
impl ConnectRpcClient {
    pub async fn connect(
        ctx: crate::protocol::ConnectContext,
    ) -> anyhow::Result<std::net::SocketAddr> {
        crate::client::grpc::http1::HttpClient::connect(
            ctx,
            crate::client::grpc::http1::Binding::ConnectRpc,
        )
        .await
    }
}
