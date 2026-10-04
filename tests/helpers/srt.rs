//! SRT fixtures: a listener policy, server and client through the shared forms, libsrt's
//! srt-live-transmit and FFmpeg as tools, and a synthetic MPEG-TS stream.
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
if kind=='srt_closed': out({'type':'srt_ignore'})
if e['resource'].startswith('private/'): out({'type':'srt_reject','reason':'forbidden'})
out({'type':'srt_accept'})
"#;

/// Every caller is admitted except for resources under private/ (forbidden).
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
        protocol: "srt".into(),
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
        protocol: "srt".into(),
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
    .map_err(|_| anyhow::anyhow!("SRT client did not connect"))??;
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

/// A tool the evidence needs; fails (never skips) when absent. srt-live-transmit is the one
/// tests/server/srt/install_peers.py checked.
pub(crate) fn tool(name: &str) -> String {
    if name == "srt-live-transmit" {
        return std::env::var("NETGET_SRT_LIVE_TRANSMIT").expect("NETGET_SRT_LIVE_TRANSMIT must name libsrt's srt-live-transmit from tests/server/srt/install_peers.py; this evidence never skips");
    }
    super::real_server::find_binary(name)
        .unwrap_or_else(|| {
            panic!(
                "{}",
                super::real_server::missing_binary(
                    name,
                    super::real_server::InstallHint {
                        brew: "ffmpeg",
                        apt: "ffmpeg"
                    },
                    "not found on PATH"
                )
            )
        })
        .display()
        .to_string()
}

/// A free UDP port on loopback.
pub(crate) fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A shell pipeline in its own process group, killed with the group when the guard drops.
pub(crate) struct Group(pub tokio::process::Child);
impl Drop for Group {
    fn drop(&mut self) {
        if let Some(pid) = self.0.id() {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}
pub(crate) fn pipeline(script: &str) -> Group {
    let mut c = tokio::process::Command::new("sh");
    c.args(["-c", script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0);
    Group(c.spawn().unwrap())
}

/// A six-second 320x240 H.264 + AAC MPEG-TS clip.
pub(crate) async fn make_clip(dir: &Path) -> String {
    let path = dir.join("clip.ts").display().to_string();
    let out = tokio::process::Command::new(tool("ffmpeg"))
        .args([
            "-hide_banner",
            "-nostdin",
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
            "mpegts",
            &path,
        ])
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

/// ffprobe's codec names for a file.
pub(crate) async fn codecs(path: &str) -> Vec<String> {
    let out = tokio::process::Command::new(tool("ffprobe"))
        .args([
            "-v",
            "error",
            "-f",
            "mpegts",
            "-show_streams",
            "-of",
            "json",
            path,
        ])
        .output()
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    v["streams"]
        .as_array()
        .map(|s| {
            s.iter()
                .filter_map(|x| x["codec_name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// A synthetic MPEG-TS stream: a PAT, a PMT naming H.264 on PID 256 and AAC on PID 257, then
/// `n` payload packets alternating between them.
pub(crate) fn tiny_ts(n: usize) -> Vec<u8> {
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &b in data {
            crc ^= (b as u32) << 24;
            for _ in 0..8 {
                crc = if crc & 0x8000_0000 != 0 {
                    (crc << 1) ^ 0x04c1_1db7
                } else {
                    crc << 1
                };
            }
        }
        crc
    }
    fn packet(pid: u16, start: bool, payload: &[u8], cc: u8) -> Vec<u8> {
        let mut p = vec![
            0x47,
            ((start as u8) << 6) | (pid >> 8) as u8,
            pid as u8,
            0x10 | (cc & 0x0f),
        ];
        p.extend(payload);
        p.resize(188, 0xff);
        p
    }
    let mut out = Vec::new();
    let mut pat = vec![
        0x00, 0xb0, 0x0d, 0x00, 0x01, 0xc1, 0x00, 0x00, 0x00, 0x01, 0xf0, 0x00,
    ];
    pat.extend(crc32(&pat).to_be_bytes());
    out.extend(packet(0, true, &[&[0u8][..], &pat].concat(), 0));
    let mut pmt = vec![
        0x02, 0xb0, 0x17, 0x00, 0x01, 0xc1, 0x00, 0x00, 0xe1, 0x00, 0xf0, 0x00, 0x1b, 0xe1, 0x00,
        0xf0, 0x00, 0x0f, 0xe1, 0x01, 0xf0, 0x00,
    ];
    pmt.extend(crc32(&pmt).to_be_bytes());
    out.extend(packet(0x1000, true, &[&[0u8][..], &pmt].concat(), 0));
    for i in 0..n {
        out.extend(packet(
            if i % 2 == 0 { 256 } else { 257 },
            false,
            &[0xab; 4],
            (i / 2) as u8,
        ));
    }
    out
}
