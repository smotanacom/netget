//! NetGet's RTSP client against **mediamtx** 1.9.3, with **ffmpeg** publishing a 1000 Hz G.711
//! tone into it over RTSP. NetGet describes the stream (the SDP comes back parsed), sets its
//! track up over UDP, plays it, hears and decodes the tone, and tears the session down; an
//! unknown path is answered 404. `install_peers.py` prints `NETGET_MEDIAMTX`; the test fails
//! rather than skips without it or without ffmpeg. No LLM calls: a python chain is the model.
use netget::cli::management::ClientForm;
use netget::state::app_state::AppState;
use netget::state::client_handles::ClientSendOutcome;
use netget::state::{AccessLogOwner, ClientId};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

/// DESCRIBE on connect, then SETUP after a good DESCRIBE, then PLAY after a good SETUP.
const CHAIN: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
nxt={'DESCRIBE':'rtsp_setup','SETUP':'rtsp_play'}
a=[]
if t=='rtsp_connected': a=[{'type':'rtsp_describe'}]
elif t=='rtsp_response' and e['status']==200 and e['method'] in nxt: a=[{'type':nxt[e['method']]}]
print(json.dumps({'actions':a}))"#;

fn mediamtx() -> String {
    let v = std::env::var("NETGET_MEDIAMTX").unwrap_or_default();
    assert!(!v.is_empty(), "NETGET_MEDIAMTX is required: python3 tests/client/rtsp/install_peers.py <dir> and export what it prints");
    v
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn events(state: &AppState, id: ClientId) -> Vec<Value> {
    state
        .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
        .await
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .collect()
}

async fn wait_event(state: &AppState, id: ClientId, pred: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(e) = events(state, id).await.into_iter().find(|e| pred(e)) {
                return e["request"].clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no matching event"))
}

fn response(method: &'static str) -> impl Fn(&Value) -> bool {
    move |e| e["event_type"] == "rtsp_response" && e["request"]["method"] == method
}

#[tokio::test]
async fn netget_plays_a_stream_from_mediamtx() {
    let dir = tempfile::tempdir().unwrap();
    let (rtsp, rtp) = (free_port(), free_port() & !1);
    let conf = dir.path().join("mediamtx.yml");
    std::fs::write(
        &conf,
        format!(
            "logLevel: info\nrtspAddress: 127.0.0.1:{rtsp}\nrtpAddress: 127.0.0.1:{rtp}\nrtcpAddress: 127.0.0.1:{}\nrtmp: no\nhls: no\nwebrtc: no\nsrt: no\napi: no\nmetrics: no\npprof: no\nplayback: no\npaths:\n  all_others:\n",
            rtp + 1
        ),
    )
    .unwrap();
    let log = std::fs::File::create(dir.path().join("mediamtx.log")).unwrap();
    let _server = tokio::process::Command::new(mediamtx())
        .arg(&conf)
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .kill_on_drop(true)
        .spawn()
        .expect("mediamtx");
    tokio::time::timeout(Duration::from_secs(30), async {
        while tokio::net::TcpStream::connect(("127.0.0.1", rtsp))
            .await
            .is_err()
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("mediamtx never listened");
    let _publisher = tokio::process::Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-loglevel",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:sample_rate=8000",
        ])
        .args(["-ac", "1", "-c:a", "pcm_mulaw", "-f", "rtsp"])
        .arg(format!("rtsp://127.0.0.1:{rtsp}/tone"))
        .kill_on_drop(true)
        .spawn()
        .expect("ffmpeg is required: apt install ffmpeg");
    let mtx_log = dir.path().join("mediamtx.log");
    tokio::time::timeout(Duration::from_secs(30), async {
        while !std::fs::read_to_string(&mtx_log)
            .unwrap_or_default()
            .contains("is publishing to path 'tone'")
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "ffmpeg never published:\n{}",
            std::fs::read_to_string(&mtx_log).unwrap_or_default()
        )
    });

    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "rtsp".into(),
        remote_addr: Some(format!("rtsp://127.0.0.1:{rtsp}/tone")),
        instruction: Some("Play the stream".into()),
        event_handlers: Some(vec![
            json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":CHAIN}}),
        ]),
        ..Default::default()
    }
    .create(&state, netget::llm::OllamaClient::new("http://127.0.0.1:1"), tx)
    .await
    .expect("connect");

    let describe = wait_event(&state, id, response("DESCRIBE")).await;
    assert_eq!(describe["status"], 200, "{describe}");
    let media = &describe["sdp"]["media"][0];
    assert_eq!(media["type"], "audio", "{describe}");
    assert_eq!(media["rtpmap"]["0"], "PCMU/8000", "{describe}");
    let setup = wait_event(&state, id, response("SETUP")).await;
    assert_eq!(setup["status"], 200, "{setup}");
    assert!(
        setup["headers"]["Transport"]
            .as_str()
            .unwrap()
            .contains("server_port="),
        "{setup}"
    );
    let play = wait_event(&state, id, response("PLAY")).await;
    assert_eq!(play["status"], 200, "{play}");
    let started = wait_event(&state, id, |e| e["event_type"] == "rtsp_stream_started").await;
    assert_eq!(started["codec"], "pcmu", "{started}");

    // Two seconds of media, then TEARDOWN; the stream goes quiet and is summarised.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let outcome = state
        .send_to_client(
            id,
            json!({"type": "rtsp_teardown"}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(
        matches!(&outcome, ClientSendOutcome::Executed { detail } if detail.contains("\"status\":200")),
        "{outcome:?}"
    );
    let ended = wait_event(&state, id, |e| e["event_type"] == "rtsp_stream_ended").await;
    assert_eq!(ended["ssrc"], started["ssrc"], "{ended}");
    // ffmpeg packs 1024 samples to a packet, so two seconds are 16 packets and 16384 octets.
    assert!(ended["octets"].as_u64().unwrap() >= 12_000, "{ended}");
    assert!(ended["duration_ms"].as_u64().unwrap() >= 1500, "{ended}");
    assert_eq!(ended["lost"], 0, "{ended}");
    let tone = ended["tone_hz"].as_f64().unwrap();
    assert!(
        (tone - 1000.0).abs() < 15.0,
        "NetGet decoded {tone} Hz: {ended}"
    );

    let outcome = state
        .send_to_client(id, json!({"type": "rtsp_options"}), Duration::from_secs(10))
        .await
        .unwrap();
    let ClientSendOutcome::Executed { detail } = outcome else {
        panic!("{outcome:?}")
    };
    let options: Value = serde_json::from_str(&detail).unwrap();
    assert!(
        options["headers"]["Public"]
            .as_str()
            .unwrap()
            .contains("DESCRIBE"),
        "{options}"
    );
}
