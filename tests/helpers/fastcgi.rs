//! FastCGI fixtures: an echo policy, server and client through the shared forms, nginx in front
//! of NetGet's responder, the pinned flup peer and access-log waits.
// Shared by the server and client test binaries; each uses a different subset.
#![allow(dead_code)]
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, AccessLogOwner, ClientId, ClientStatus, ServerId},
};
use serde_json::{json, Value};
use std::{net::SocketAddr, path::PathBuf, time::Duration};
use tokio::sync::mpsc;

pub(crate) fn state() -> AppState {
    AppState::new_with_options(false, "http://127.0.0.1:1".into())
}

/// Answers by path: /teapot is a 418 with a stderr line, /big a 200 000-byte body, /binary
/// two raw bytes in hex, /submit reports the body it got; anything else echoes the request.
pub(crate) fn echo_policy() -> Vec<Value> {
    vec![
        json!({"event_pattern":"fastcgi_request","handler":{"type":"script","language":"python","code":concat!(
            "import json,sys\n",
            "e=json.load(sys.stdin)['event']\n",
            "uri=e.get('request_uri') or ''; path=uri.split('?')[0]\n",
            "if path=='/teapot': a={'status':418,'body':'short and stout','stderr':'teapot brewed'}\n",
            "elif path=='/big': a={'status':200,'body':'y'*200000}\n",
            "elif path=='/binary': a={'status':200,'headers':{'Content-Type':'application/octet-stream'},'body':'00ff','body_encoding':'hex'}\n",
            "elif path=='/submit': a={'status':201,'headers':{'Content-Type':'application/json'},'body':json.dumps({'got':len(e['body']),'encoding':e['body_encoding'],'type':e['content_type']})}\n",
            "else: a={'status':200,'headers':{'Content-Type':'application/json','X-From':'netget'},'body':json.dumps({'method':e['method'],'uri':uri,'query':e['query_string'],'x_test':e['headers'].get('x-test'),'keep':e['keep_conn']})}\n",
            "a['type']='fastcgi_respond'\n",
            "print(json.dumps({'actions':[a]}))\n"
        )}}),
    ]
}

pub(crate) async fn server_in(
    state: &AppState,
    handlers: Vec<Value>,
    params: Value,
) -> (ServerId, SocketAddr) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let id = ServerForm {
        protocol: "fastcgi".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        instruction: Some(String::new()),
        event_handlers: Some(handlers),
        startup_params: Some(params),
        ..Default::default()
    }
    .create(state, tx)
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(addr) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break addr;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    (id, addr)
}

pub(crate) async fn client_in(
    state: &AppState,
    remote: String,
    params: Value,
) -> anyhow::Result<ClientId> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let handlers = vec![
        json!({"event_pattern":"fastcgi_connected","handler":{"type":"static","actions":[]}}),
        json!({"event_pattern":"fastcgi_response","handler":{"type":"static","actions":[]}}),
        json!({"event_pattern":"fastcgi_values","handler":{"type":"static","actions":[]}}),
    ];
    let id = ClientForm {
        protocol: "fastcgi".into(),
        remote_addr: Some(remote),
        instruction: Some(String::new()),
        event_handlers: Some(handlers),
        startup_params: Some(params),
        ..Default::default()
    }
    .create(
        state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(15), async {
        while !state.has_client_handle(id).await {
            if let Some(ClientStatus::Error(e)) = state.get_client(id).await.map(|c| c.status) {
                anyhow::bail!(e);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Ok(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("FastCGI client did not connect"))??;
    Ok(id)
}

pub(crate) async fn logs(
    state: &AppState,
    owner: AccessLogOwner,
    kind: &str,
    count: usize,
) -> Vec<netget::state::app_state::AccessLogEntry> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let mut rows = state
                .list_access_logs_for(Some(owner), None)
                .await
                .into_iter()
                .filter(|e| e.event_type == kind)
                .collect::<Vec<_>>();
            if rows.len() >= count {
                rows.sort_by_key(|e| e.id);
                break rows;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {count} {kind} access-log entries"))
}

pub(crate) fn python() -> String {
    std::env::var("NETGET_FASTCGI_PYTHON").expect("NETGET_FASTCGI_PYTHON must name the Python from tests/server/fastcgi/install_peers.py (flup 1.0.3); this evidence never skips")
}
pub(crate) fn peer_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/server/fastcgi/peer.py")
}

pub(crate) const NGINX: super::real_server::InstallHint = super::real_server::InstallHint {
    brew: "nginx",
    apt: "nginx",
};

/// nginx, unprivileged and in the foreground, passing every request to NetGet's responder at
/// `fastcgi` over a kept-alive upstream connection. Its error log (stderr) is where FastCGI
/// STDERR lines land.
pub(crate) async fn nginx_in_front_of(
    fastcgi: SocketAddr,
) -> super::E2EResult<super::real_server::RealServer> {
    let conf = format!(
        r#"worker_processes 1;
daemon off;
pid {{dir}}/nginx.pid;
error_log stderr notice;
events {{ worker_connections 64; }}
http {{
    access_log off;
    client_body_temp_path {{dir}}/tmp_body;
    proxy_temp_path {{dir}}/tmp_proxy;
    fastcgi_temp_path {{dir}}/tmp_fastcgi;
    uwsgi_temp_path {{dir}}/tmp_uwsgi;
    scgi_temp_path {{dir}}/tmp_scgi;
    client_max_body_size 4m;
    fastcgi_buffering off;
    upstream netget {{ server {fastcgi}; keepalive 4; }}
    server {{
        listen 127.0.0.1:{{port}};
        location / {{
            fastcgi_pass netget;
            fastcgi_keep_conn on;
            fastcgi_param REQUEST_METHOD $request_method;
            fastcgi_param REQUEST_URI $request_uri;
            fastcgi_param SCRIPT_NAME $fastcgi_script_name;
            fastcgi_param QUERY_STRING $query_string;
            fastcgi_param CONTENT_TYPE $content_type;
            fastcgi_param CONTENT_LENGTH $content_length;
            fastcgi_param SERVER_PROTOCOL $server_protocol;
            fastcgi_param GATEWAY_INTERFACE CGI/1.1;
            fastcgi_param REMOTE_ADDR $remote_addr;
            fastcgi_param SERVER_NAME $server_name;
            fastcgi_param SERVER_PORT $server_port;
        }}
    }}
}}
"#
    );
    super::real_server::RealServer::builder("nginx", NGINX)
        .config_file("nginx.conf", &conf)
        .args(["-p", "{dir}", "-c", "{dir}/nginx.conf"])
        .ready_when_log_matches("start worker processes")
        .start()
        .await
}
