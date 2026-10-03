//! Binary gRPC-Web binding for the shared owned HTTP/1.1 client.
pub mod actions;
pub struct GrpcWebClient;
impl GrpcWebClient {
    pub async fn connect(
        ctx: crate::protocol::ConnectContext,
    ) -> anyhow::Result<std::net::SocketAddr> {
        crate::client::grpc::http1::HttpClient::connect(
            ctx,
            crate::client::grpc::http1::Binding::GrpcWeb,
        )
        .await
    }
}
