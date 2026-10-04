//! WebTransport client: opens one session over the vendored wtransport and runs the server's
//! `Session` loop on it, with handler turns going through `call_llm_for_client`.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::webtransport::session::Session;
use crate::state::ClientStatus;
pub use actions::WebTransportClientProtocol;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use wtransport::endpoint::ConnectOptions;
use wtransport::tls::{CertificateChain, Sha256Digest};
use wtransport::{ClientConfig, Endpoint};

pub const DEFAULT_PATH: &str = "/";

fn tls(
    builder: wtransport::config::ClientConfigBuilder<wtransport::config::states::WantsRootStore>,
    pin: Option<String>,
    ca: Option<CertificateChain>,
) -> Result<
    wtransport::config::ClientConfigBuilder<wtransport::config::states::WantsTransportConfigClient>,
> {
    Ok(match (pin, ca) {
        (Some(_), Some(_)) => bail!("give certificate_sha256 or ca_cert_path, not both"),
        (Some(pin), None) => {
            let bytes: [u8; 32] = hex::decode(pin.trim())
                .ok()
                .and_then(|b| b.try_into().ok())
                .context("certificate_sha256 is 64 hex digits")?;
            builder.with_server_certificate_hashes([Sha256Digest::new(bytes)])
        }
        (None, Some(chain)) => {
            let mut roots = rustls::RootCertStore::empty();
            for c in chain.as_slice() {
                roots.add(rustls::pki_types::CertificateDer::from(c.der().to_vec()))?;
            }
            builder.with_custom_tls(wtransport::tls::client::build_default_tls_config(
                Arc::new(roots),
                None,
            ))
        }
        (None, None) => builder.with_native_certs(),
    })
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let get = |name| {
        p.map(|p| p.get_optional_string(name))
            .transpose()
            .map(Option::flatten)
    };
    let path = get("path")?.unwrap_or_else(|| DEFAULT_PATH.into());
    ensure!(
        path.starts_with('/') && !path.chars().any(|c| c.is_whitespace() || c.is_control()),
        "path starts with / and carries no spaces or control characters"
    );
    let ca = match get("ca_cert_path")? {
        Some(file) => Some(
            CertificateChain::load_pemfile(&file)
                .await
                .with_context(|| format!("loading {file}"))?,
        ),
        None => None,
    };
    let idle = p
        .map(|p| p.get_optional_u64("idle_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(crate::server::webtransport::IDLE_TIMEOUT.as_secs());
    ensure!((1..=3600).contains(&idle), "idle_timeout_secs is 1 to 3600");
    let mut options = ConnectOptions::builder(format!("https://{}{path}", ctx.remote_addr));
    if let Some(headers) = p
        .map(|p| p.get_optional_object("headers"))
        .transpose()?
        .flatten()
    {
        ensure!(headers.len() <= 16, "at most 16 extra headers");
        for (k, v) in headers {
            let v = v.as_str().context("header values are strings")?;
            ensure!(
                !v.chars().any(char::is_control),
                "the {k} header carries control characters"
            );
            options = options.add_header(k.to_ascii_lowercase(), v);
        }
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = tls(
        ClientConfig::builder().with_bind_default(),
        get("certificate_sha256")?,
        ca,
    )?
    .max_idle_timeout(Some(Duration::from_secs(idle)))?
    .build();
    let endpoint = Endpoint::client(config)?;
    let url = options.build();
    let shown = url.url().to_owned();
    let conn = tokio::time::timeout(
        crate::server::webtransport::HANDSHAKE_TIMEOUT,
        endpoint.connect(url),
    )
    .await
    .context("the session was not established in time")?
    .with_context(|| format!("opening {shown}"))?;
    let local = endpoint.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("WebTransport session open: {shown}"));
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let commands =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let ask_ctx = ctx.clone();
    let session = Session::new(
        conn,
        Arc::new(move |event| {
            let ctx = ask_ctx.clone();
            Box::pin(async move { ask(&ctx, event).await })
        }),
        Arc::new(|_, _| Box::pin(async {})),
        ctx.status_tx.clone(),
        "WebTransport client",
    );
    let run_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let _endpoint = endpoint;
        let first = Event::new(&actions::CONNECTED_EVENT, json!({"url": shown}));
        let reason = session.run(vec![], Some(first), commands).await;
        Log::new(Some(&run_ctx.status_tx)).info(format!("WebTransport session closed: {reason}"));
        run_ctx
            .state
            .update_client_status(run_ctx.client_id, ClientStatus::Disconnected)
            .await;
        run_ctx.state.remove_client_handle(run_ctx.client_id).await;
        let _ = run_ctx.status_tx.send("__UPDATE_UI__".into());
    });
    ctx.state.register_client_task(ctx.client_id, task).await;
    Ok(local)
}

async fn ask(ctx: &ConnectContext, event: Event) -> Result<Vec<Value>> {
    let instruction = ctx
        .state
        .get_instruction_for_client(ctx.client_id)
        .await
        .unwrap_or_default();
    let memory = ctx
        .state
        .get_memory_for_client(ctx.client_id)
        .await
        .unwrap_or_default();
    let r = call_llm_for_client(
        &ctx.llm_client,
        &ctx.state,
        ctx.client_id.to_string(),
        &instruction,
        &memory,
        Some(&event),
        &WebTransportClientProtocol,
        &ctx.status_tx,
    )
    .await?;
    if let Some(m) = r.memory_updates {
        ctx.state.set_memory_for_client(ctx.client_id, m).await;
    }
    Ok(r.actions)
}
