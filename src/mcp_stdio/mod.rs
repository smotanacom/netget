//! MCP server mode (STDIO and HTTP transports)
//!
//! Runs NetGet as an MCP server, exposing network protocol management as MCP
//! tools. Protocol servers are driven by NetGet's own configured LLM backend
//! (Ollama or an OpenAI-compatible endpoint).

pub mod control;
pub mod docs;
#[cfg(feature = "mcp-stdio")]
pub mod drain;
pub mod tools;

use anyhow::Result;
use tracing::info;

use crate::cli::Args;
use crate::settings::Settings;

/// Main entry point for MCP STDIO server mode
#[cfg(feature = "mcp-stdio")]
pub async fn run_mcp_stdio(args: &Args, settings: Settings) -> Result<()> {
    use rmcp::ServiceExt;

    // Logging goes to stderr only (stdout = JSON-RPC)
    // The CLI setup already handles this for non-interactive mode

    info!("Starting NetGet MCP STDIO server");

    // Create the MCP server service
    let service = tools::NetGetMcpService::new(args, settings).await?;

    // Serve over stdin/stdout. rmcp's own stdio transport, wrapped so that a request still
    // being handled when stdin closes is answered before the server stops (see `drain`).
    let (stdin, stdout) = rmcp::transport::stdio();
    let server = service
        .serve(drain::drain_on_eof_transport(stdin, stdout))
        .await?;

    info!("MCP STDIO server initialized, waiting for requests...");

    // Wait for the server to complete: stdin closed and every request answered, or shutdown.
    server.waiting().await?;

    info!("MCP STDIO server shut down");
    Ok(())
}

/// Entry point for MCP HTTP/SSE server mode
///
/// Serves the same tools as STDIO mode over the MCP Streamable HTTP transport,
/// allowing remote or web-based MCP clients to connect. All HTTP sessions share
/// a single `SharedState` (servers/clients started in one session are visible to all).
#[cfg(feature = "mcp-http")]
pub async fn run_mcp_http(args: &Args, settings: Settings, port: u16) -> Result<()> {
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };
    use std::sync::Arc;

    // Shared state created once, reused by every HTTP session
    let shared_state = tools::NetGetMcpService::create_shared_state(args, settings).await?;

    let service = StreamableHttpService::new(
        move || {
            Ok(tools::NetGetMcpService::new_with_shared_state(
                shared_state.clone(),
            ))
        },
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );

    let listen_addr = args.listen_addr.as_deref().unwrap_or("127.0.0.1");
    let bind = format!("{}:{}", listen_addr, port);

    let app = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(&bind).await?;

    info!("NetGet MCP HTTP server listening on http://{}/mcp", bind);
    axum::serve(listener, app).await?;
    Ok(())
}
