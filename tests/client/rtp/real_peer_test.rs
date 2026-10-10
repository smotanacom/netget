//! NetGet's RTP endpoint against **ffmpeg** in both directions. ffmpeg receives NetGet's
//! stream through an SDP and decodes it into a WAV whose tone is measured; ffmpeg's own RTP
//! muxer streams a sine to NetGet, whose decoded tone and packet accounting are asserted.
//! ffmpeg is required (it is installed wherever the RTP suites run); the test fails rather
//! than skips without it. No LLM calls: python chains are the model.
use netget::cli::management::ClientForm;
use netget::client::rtp::analyse;
use netget::state::app_state::AppState;
use netget::state::{AccessLogOwner, ClientId};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

fn ffmpeg() -> &'static str {
    let ok = std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(
        ok,
        "ffmpeg is required: apt install ffmpeg / brew install ffmpeg"
    );
    "ffmpeg"
}

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn client(state: &AppState, remote: String, listen: String, chain: &str) -> ClientId {
    let (tx, _) = mpsc::unbounded_channel();
    ClientForm {
        protocol: "rtp".into(),
        remote_addr: Some(remote),
        instruction: Some("Play tones".into()),
        startup_params: Some(json!({"listen": listen})),
        event_handlers: Some(vec![
            json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":chain}}),
        ]),
        ..Default::default()
    }
    .create(state, netget::llm::OllamaClient::new("http://127.0.0.1:1"), tx)
    .await
    .expect("connect")
}

async fn wait_event(state: &AppState, id: ClientId, kind: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let hit = state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .iter()
                .map(|e| serde_json::to_value(e).unwrap())
                .find(|e| e["event_type"] == kind)
                .map(|e| e["request"].clone());
            if let Some(e) = hit {
                return e;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {kind} event"))
}

/// The 16-bit samples of a PCM WAV file.
fn wav_samples(bytes: &[u8]) -> Vec<i16> {
    let mut i = 12;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let len = u32::from_le_bytes(bytes[i + 4..i + 8].try_into().unwrap()) as usize;
        if id == b"data" {
            let end = (i + 8 + len).min(bytes.len());
            return bytes[i + 8..end]
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect();
        }
        i += 8 + len + (len & 1);
    }
    panic!("no data chunk in the WAV");
}

#[tokio::test]
async fn ffmpeg_decodes_what_netget_sends() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_udp_port();
    let sdp = dir.path().join("in.sdp");
    std::fs::write(&sdp, format!("v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=netget\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {port} RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\n")).unwrap();
    let wav = dir.path().join("out.wav");
    let mut receiver = tokio::process::Command::new(ffmpeg())
        .args([
            "-nostdin",
            "-loglevel",
            "error",
            "-protocol_whitelist",
            "file,udp,rtp",
            "-i",
        ])
        .arg(&sdp)
        .args(["-t", "1.5", "-y"])
        .arg(&wav)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    // ffmpeg binds its port as it opens the SDP; give it that long.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let chain = r#"import json,sys
i=json.load(sys.stdin)
a=[{'type':'send_rtp_audio','payload_type':'pcma','content':'tone','tone_hz':440,'duration_ms':3000}] if i['event_type_id']=='rtp_ready' else []
print(json.dumps({'actions':a}))"#;
    let id = client(
        &state,
        format!("127.0.0.1:{port}"),
        "127.0.0.1:0".into(),
        chain,
    )
    .await;
    let status = tokio::time::timeout(Duration::from_secs(30), receiver.wait())
        .await
        .expect("ffmpeg did not finish")
        .unwrap();
    assert!(status.success(), "ffmpeg failed");
    let samples = wav_samples(&std::fs::read(&wav).unwrap());
    assert!(
        samples.len() >= 8000,
        "ffmpeg decoded {} samples",
        samples.len()
    );
    let (tone, level) = analyse(&samples, 8000.0);
    let tone = tone.expect("no tone in what ffmpeg decoded");
    assert!((tone - 440.0).abs() < 10.0, "ffmpeg heard {tone} Hz");
    assert!(level > -20.0, "the tone is {level} dBFS");
    let sent = wait_event(&state, id, "rtp_sent").await;
    assert_eq!(sent["packets"], 150, "{sent}");
    assert_eq!(sent["complete"], true, "{sent}");
}

#[tokio::test]
async fn netget_hears_what_ffmpeg_sends() {
    let port = free_udp_port();
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let id = client(
        &state,
        "127.0.0.1:9".into(),
        format!("127.0.0.1:{port}"),
        "import json\nprint(json.dumps({'actions':[]}))",
    )
    .await;
    wait_event(&state, id, "rtp_ready").await;
    let out = tokio::process::Command::new(ffmpeg())
        .args([
            "-nostdin",
            "-loglevel",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:duration=2:sample_rate=8000",
        ])
        .args([
            "-ac",
            "1",
            "-c:a",
            "pcm_mulaw",
            "-payload_type",
            "0",
            "-f",
            "rtp",
        ])
        .arg(format!("rtp://127.0.0.1:{port}"))
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let started = wait_event(&state, id, "rtp_stream_started").await;
    assert_eq!(started["payload_type"], 0);
    assert_eq!(started["codec"], "pcmu");
    let ended = wait_event(&state, id, "rtp_stream_ended").await;
    assert_eq!(ended["ssrc"], started["ssrc"], "{ended}");
    assert_eq!(ended["lost"], 0, "{ended}");
    assert_eq!(
        ended["octets"], 16000,
        "two seconds at 8000 bytes a second: {ended}"
    );
    let ms = ended["duration_ms"].as_u64().unwrap();
    assert!((1900..=2100).contains(&ms), "{ended}");
    let tone = ended["tone_hz"].as_f64().unwrap();
    assert!(
        (tone - 1000.0).abs() < 15.0,
        "NetGet decoded {tone} Hz: {ended}"
    );
}
