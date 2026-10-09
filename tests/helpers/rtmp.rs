//! RTMP fixtures: a server policy, server and client through the shared forms, FFmpeg as a
//! tool, MediaMTX as a server (`tests/server/rtmp/install_peers.py`), and a synthetic FLV.
// Shared by the server and client test binaries; each uses a different subset.
#![allow(dead_code)]
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, AccessLogOwner, ClientId, ClientStatus, ServerId},
};
use serde_json::{json, Value};
use std::{net::SocketAddr, path::Path, time::Duration};
use tokio::sync::mpsc;

pub(crate) fn state() -> AppState {
    AppState::new_with_options(false, "http://127.0.0.1:1".into())
}

const POLICY: &str = r#"import json,sys
d=json.load(sys.stdin); kind=d['event_type_id']; e=d['event']
def out(a): print(json.dumps({'actions':[a]})); sys.exit()
if kind=='rtmp_connect' and e['app']=='blocked': out({'type':'rtmp_reject','description':'application blocked is closed'})
if kind=='rtmp_publish' and e['stream']=='forbidden': out({'type':'rtmp_reject','description':'stream key not recognised'})
if kind=='rtmp_play' and e['stream']=='secret': out({'type':'rtmp_reject','description':'not for you'})
out({'type':'rtmp_ignore'} if kind=='rtmp_publish_ended' else {'type':'rtmp_accept'})
"#;

/// Every app but "blocked", every stream key but "forbidden", and every play but "secret".
pub(crate) fn policy() -> Vec<Value> {
    vec![
        json!({"event_pattern": "*", "handler": {"type":"script","language":"python","code": POLICY}}),
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
        protocol: "rtmp".into(),
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
    let handlers = vec![json!({"event_pattern": "*", "handler": {"type":"static","actions":[]}})];
    let id = ClientForm {
        protocol: "rtmp".into(),
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
    .map_err(|_| anyhow::anyhow!("RTMP client did not connect"))??;
    Ok(id)
}

pub(crate) async fn logs(
    state: &AppState,
    owner: AccessLogOwner,
    kind: &str,
    count: usize,
) -> Vec<netget::state::app_state::AccessLogEntry> {
    tokio::time::timeout(Duration::from_secs(60), async {
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
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {count} {kind} access-log entries"))
}

const FFMPEG: super::real_server::InstallHint = super::real_server::InstallHint {
    brew: "ffmpeg",
    apt: "ffmpeg",
};

/// FFmpeg's own tools, which this evidence needs; fails (never skips) when absent.
pub(crate) fn tool(name: &str) -> String {
    super::real_server::find_binary(name)
        .unwrap_or_else(|| {
            panic!(
                "{}",
                super::real_server::missing_binary(name, FFMPEG, "not found on PATH")
            )
        })
        .display()
        .to_string()
}

/// A command for an FFmpeg tool, quiet and killed with the test.
pub(crate) fn ffmpeg_command(name: &str, args: &[&str]) -> tokio::process::Command {
    let mut c = tokio::process::Command::new(tool(name));
    c.arg("-hide_banner");
    if name == "ffmpeg" {
        c.arg("-nostdin");
    }
    c.args(args).kill_on_drop(true);
    c
}

/// A six-second 320x240 25 fps H.264 + 44.1 kHz AAC FLV, keyframe every second.
pub(crate) async fn make_clip(dir: &Path) -> String {
    let path = dir.join("clip.flv").display().to_string();
    let out = ffmpeg_command(
        "ffmpeg",
        &[
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=25",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=44100",
            "-t",
            "6",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-g",
            "25",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "64k",
            "-f",
            "flv",
            &path,
        ],
    )
    .output()
    .await
    .unwrap();
    assert!(
        out.status.success(),
        "ffmpeg could not make the clip:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    path
}

/// MediaMTX with RTMP on {port} and its API on {port1}; `paths` is its YAML paths section.
pub(crate) async fn start_mediamtx(
    paths: &str,
) -> super::E2EResult<super::real_server::RealServer> {
    let bin = std::env::var("NETGET_RTMP_MEDIAMTX").expect("NETGET_RTMP_MEDIAMTX must name the MediaMTX binary from tests/server/rtmp/install_peers.py; this evidence never skips");
    super::real_server::RealServer::builder(&bin, super::real_server::InstallHint { brew: "python (then tests/server/rtmp/install_peers.py)", apt: "python3 (then tests/server/rtmp/install_peers.py)" })
        .extra_ports(1)
        .config_file(
            "mediamtx.yml",
            &format!("logLevel: info\nrtsp: no\nrtmp: yes\nrtmpAddress: 127.0.0.1:{{port}}\nhls: no\nwebrtc: no\nsrt: no\napi: yes\napiAddress: 127.0.0.1:{{port1}}\npaths:\n{paths}\n"),
        )
        .args(["{dir}/mediamtx.yml"])
        .startup_timeout(Duration::from_secs(30))
        .start()
        .await
}

/// GET MediaMTX's view of a path until it is ready; its JSON.
pub(crate) async fn wait_ready(api: &str, path: &str, timeout: Duration) -> Value {
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = format!("http://{api}/v3/paths/get/{path}");
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(r) = http.get(&url).send().await {
            if let Ok(v) = r.json::<Value>().await {
                if v["ready"] == true {
                    return v;
                }
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "MediaMTX path {path} never became ready"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A synthetic FLV (no codec data the server would need to decode): onMetaData, an AVC
/// sequence header, an AAC sequence header, then `seconds` of 25 fps video (keyframe each
/// second) and ~43 fps audio.
pub(crate) fn tiny_flv(seconds: u32) -> Vec<u8> {
    fn tag(out: &mut Vec<u8>, kind: u8, ts: u32, data: &[u8]) {
        out.push(kind);
        out.extend(&(data.len() as u32).to_be_bytes()[1..]);
        out.extend(&ts.to_be_bytes()[1..]);
        out.push((ts >> 24) as u8);
        out.extend([0, 0, 0]);
        out.extend(data);
        out.extend((11 + data.len() as u32).to_be_bytes());
    }
    let mut f = b"FLV\x01\x05\x00\x00\x00\x09\x00\x00\x00\x00".to_vec();
    let mut meta = netget::server::rtmp::amf0::encode(&[json!("onMetaData")]).unwrap();
    meta.extend(
        netget::server::rtmp::amf0::encode_ecma(
            json!({"width": 64.0, "height": 48.0, "framerate": 25.0})
                .as_object()
                .unwrap(),
        )
        .unwrap(),
    );
    tag(&mut f, 18, 0, &meta);
    tag(&mut f, 9, 0, &[0x17, 0x00, 0, 0, 0, 1, 0x64, 0, 0x0a]);
    tag(&mut f, 8, 0, &[0xaf, 0x00, 0x12, 0x10]);
    let mut events: Vec<(u8, u32, Vec<u8>)> = Vec::new();
    for i in 0..seconds * 25 {
        let key = i % 25 == 0;
        events.push((
            9,
            i * 40,
            vec![if key { 0x17 } else { 0x27 }, 0x01, 0, 0, 0, 0xde, 0xad],
        ));
    }
    for i in 0..seconds * 43 {
        events.push((8, i * 1000 / 43, vec![0xaf, 0x01, 0x21, 0x10]));
    }
    events.sort_by_key(|e| e.1);
    for (k, ts, d) in events {
        tag(&mut f, k, ts, &d);
    }
    f
}
