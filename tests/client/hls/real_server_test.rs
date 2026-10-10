//! NetGet's HLS client against streams two independent implementations wrote:
//!
//! - **ffmpeg**'s HLS muxer: a VOD stream with a master playlist naming two variants (H.264 +
//!   AAC at two sizes). NetGet reads the master playlist, plays the highest variant to its
//!   end, and the run is checked against what ffmpeg wrote, and against which files the
//!   server says were fetched.
//! - **GStreamer**'s `hlssink2`: a live stream whose playlist keeps three segments and slides
//!   while NetGet plays it (H.264 + MP3), so the reload path has to keep up without a gap.
//!
//! Both are served by Python's `http.server`. ffmpeg and gst-launch-1.0 (with the `bad` and
//! `ugly` plugin sets) are required; the tests fail rather than skip without them. No LLM
//! calls: a python chain is the model.
use netget::cli::management::ClientForm;
use netget::state::app_state::AppState;
use netget::state::client_handles::ClientSendOutcome;
use netget::state::{AccessLogOwner, ClientId};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;
use tokio::sync::mpsc;

/// Read the playlist on connect; play a master playlist's highest variant.
const VOD_CHAIN: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
a=[]
if t=='hls_ready': a=[{'type':'hls_get_playlist'}]
elif t=='hls_playlist' and e.get('kind')=='master': a=[{'type':'hls_play','variant':'highest','max_segments':10}]
print(json.dumps({'actions':a}))"#;

/// Play five segments of the live stream on connect.
const LIVE_CHAIN: &str = r#"import json,sys
i=json.load(sys.stdin)
a=[{'type':'hls_play','max_segments':5}] if i['event_type_id']=='hls_ready' else []
print(json.dumps({'actions':a}))"#;

fn require(binary: &str, version_flag: &str, install: &str) {
    let ok = std::process::Command::new(binary)
        .arg(version_flag)
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(ok, "{binary} is required: {install}");
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Python's http.server over `dir`; its request log goes to `log`.
async fn serve(dir: &Path, log: &Path) -> (tokio::process::Child, u16) {
    let port = free_port();
    let log = std::fs::File::create(log).unwrap();
    let child = tokio::process::Command::new("python3")
        .args(["-m", "http.server", "--bind", "127.0.0.1"])
        .arg(port.to_string())
        .arg("--directory")
        .arg(dir)
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .kill_on_drop(true)
        .spawn()
        .expect("python3");
    tokio::time::timeout(Duration::from_secs(20), async {
        while tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err()
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("http.server never listened");
    (child, port)
}

async fn client(state: &AppState, url: String, chain: &str) -> ClientId {
    let (tx, _) = mpsc::unbounded_channel();
    ClientForm {
        protocol: "hls".into(),
        remote_addr: Some(url),
        instruction: Some("Play the stream".into()),
        event_handlers: Some(vec![
            json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":chain}}),
        ]),
        ..Default::default()
    }
    .create(state, netget::llm::OllamaClient::new("http://127.0.0.1:1"), tx)
    .await
    .expect("connect")
}

async fn wait_event(state: &AppState, id: ClientId, kind: &str, secs: u64) -> Value {
    tokio::time::timeout(Duration::from_secs(secs), async {
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

fn names(v: &Value) -> Vec<&str> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn netget_plays_an_ffmpeg_vod_stream_with_variants() {
    require(
        "ffmpeg",
        "-version",
        "apt install ffmpeg / brew install ffmpeg",
    );
    let dir = tempfile::tempdir().unwrap();
    let media = dir.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let out = tokio::process::Command::new("ffmpeg")
        .current_dir(&media)
        .args(["-nostdin", "-loglevel", "error"])
        .args(["-f", "lavfi", "-i", "testsrc=size=320x240:rate=25"])
        .args(["-f", "lavfi", "-i", "sine=frequency=1000:sample_rate=44100"])
        .args([
            "-t", "4", "-map", "0:v", "-map", "1:a", "-map", "0:v", "-map", "1:a",
        ])
        .args([
            "-c:v",
            "libx264",
            "-c:a",
            "aac",
            "-g",
            "25",
            "-keyint_min",
            "25",
        ])
        .args([
            "-b:v:0", "400k", "-s:v:0", "320x240", "-b:v:1", "150k", "-s:v:1", "160x120",
        ])
        .args(["-f", "hls", "-hls_time", "1", "-hls_playlist_type", "vod"])
        .args([
            "-master_pl_name",
            "master.m3u8",
            "-var_stream_map",
            "v:0,a:0 v:1,a:1",
        ])
        .args(["-hls_segment_filename", "v%v/seg%03d.ts", "v%v/index.m3u8"])
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let log = dir.path().join("http.log");
    let (_server, port) = serve(&media, &log).await;

    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let id = client(
        &state,
        format!("http://127.0.0.1:{port}/master.m3u8"),
        VOD_CHAIN,
    )
    .await;

    let master = wait_event(&state, id, "hls_playlist", 30).await;
    assert_eq!(master["status"], 200, "{master}");
    assert_eq!(master["kind"], "master", "{master}");
    let variants = master["variants"].as_array().unwrap();
    assert_eq!(variants.len(), 2, "{master}");
    assert_eq!(variants[0]["uri"], "v0/index.m3u8", "{master}");
    assert_eq!(variants[0]["resolution"], "320x240", "{master}");
    assert_eq!(variants[1]["resolution"], "160x120", "{master}");
    assert!(
        variants[0]["bandwidth"].as_u64() > variants[1]["bandwidth"].as_u64(),
        "{master}"
    );
    assert!(
        variants[0]["codecs"].as_str().unwrap().contains("avc1."),
        "{master}"
    );

    let played = wait_event(&state, id, "hls_played", 60).await;
    assert!(played.get("error").is_none(), "{played}");
    assert_eq!(
        played["variant"]["index"], 0,
        "the highest variant: {played}"
    );
    assert!(
        played["media_playlist"]
            .as_str()
            .unwrap()
            .ends_with("/v0/index.m3u8"),
        "{played}"
    );
    assert_eq!(played["segments"], 4, "{played}");
    assert_eq!(played["first_sequence"], 0, "{played}");
    assert_eq!(played["last_sequence"], 3, "{played}");
    assert_eq!(played["endlist"], true, "{played}");
    assert_eq!(played["gaps"], 0, "{played}");
    assert_eq!(
        played["reloads"], 0,
        "a VOD playlist is read once: {played}"
    );
    assert_eq!(played["failed"], json!([]), "{played}");
    assert_eq!(played["containers"], json!(["mpegts"]), "{played}");
    assert_eq!(names(&played["codecs"]), ["aac", "h264"], "{played}");
    assert_eq!(played["continuity_errors"], 0, "{played}");
    assert_eq!(played["playlist_duration"], 4.0, "{played}");
    let ms = played["duration_ms"].as_u64().unwrap();
    assert!(
        (3500..=4100).contains(&ms),
        "four one-second segments: {played}"
    );
    let bytes: u64 = std::fs::read_dir(media.join("v0"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "ts"))
        .map(|p| std::fs::metadata(p).unwrap().len())
        .sum();
    assert_eq!(played["bytes"], bytes, "every byte ffmpeg wrote: {played}");

    // What the server saw: the master playlist, the chosen variant's playlist and all four of
    // its segments, and nothing of the other variant.
    let served = std::fs::read_to_string(&log).unwrap();
    for path in [
        "/master.m3u8",
        "/v0/index.m3u8",
        "/v0/seg000.ts",
        "/v0/seg003.ts",
    ] {
        assert!(
            served.contains(&format!("GET {path} ")),
            "{path} never fetched:\n{served}"
        );
    }
    assert!(
        !served.contains("/v1/"),
        "the lower variant was fetched:\n{served}"
    );

    // One segment on request, resolved against the media playlist just played.
    let outcome = state
        .send_to_client(
            id,
            json!({"type": "hls_get_segment", "uri": "seg001.ts"}),
            Duration::from_secs(20),
        )
        .await
        .unwrap();
    let ClientSendOutcome::Executed { detail } = outcome else {
        panic!("{outcome:?}")
    };
    let segment: Value = serde_json::from_str(&detail).unwrap();
    assert!(
        segment["url"].as_str().unwrap().ends_with("/v0/seg001.ts"),
        "{segment}"
    );
    let size = std::fs::metadata(media.join("v0/seg001.ts")).unwrap().len();
    assert_eq!(segment["bytes"], size, "{segment}");
    assert_eq!(segment["ts_packets"], size / 188, "{segment}");
    assert_eq!(segment["sync_errors"], 0, "{segment}");
    let codecs: Vec<&str> = segment["streams"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["codec"].as_str().unwrap())
        .collect();
    assert_eq!(
        codecs,
        ["h264", "aac"],
        "ffmpeg's PIDs 0x100 and 0x101: {segment}"
    );

    // A URI on another origin is refused before anything is sent.
    let outcome = state
        .send_to_client(
            id,
            json!({"type": "hls_get_segment", "uri": "http://127.0.0.2:9/x.ts"}),
            Duration::from_secs(20),
        )
        .await
        .unwrap();
    let ClientSendOutcome::Executed { detail } = outcome else {
        panic!("{outcome:?}")
    };
    assert!(detail.contains("bound to"), "{detail}");
}

#[tokio::test]
async fn netget_follows_a_live_gstreamer_playlist_as_it_slides() {
    require(
        "gst-launch-1.0",
        "--version",
        "apt install gstreamer1.0-tools gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly",
    );
    let dir = tempfile::tempdir().unwrap();
    let media = dir.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let gst_log = std::fs::File::create(dir.path().join("gst.log")).unwrap();
    // Live sources, one-second segments, a three-segment sliding playlist.
    let pipeline = format!(
        "videotestsrc is-live=true ! video/x-raw,width=320,height=240,framerate=25/1 \
         ! x264enc key-int-max=25 tune=zerolatency speed-preset=ultrafast ! h264parse ! hls.video \
         audiotestsrc is-live=true freq=1000 ! audio/x-raw,rate=44100 ! lamemp3enc ! mpegaudioparse ! hls.audio \
         hlssink2 name=hls target-duration=1 playlist-length=3 max-files=12 \
         location={dir}/seg%05d.ts playlist-location={dir}/index.m3u8",
        dir = media.display()
    );
    let _gst = tokio::process::Command::new("gst-launch-1.0")
        .arg("-q")
        .args(pipeline.split_whitespace())
        .stdout(gst_log.try_clone().unwrap())
        .stderr(gst_log)
        .kill_on_drop(true)
        .spawn()
        .expect("gst-launch-1.0");
    let playlist = media.join("index.m3u8");
    tokio::time::timeout(Duration::from_secs(30), async {
        while std::fs::read_to_string(&playlist)
            .unwrap_or_default()
            .matches("#EXTINF")
            .count()
            < 2
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "GStreamer never wrote a playlist:\n{}",
            std::fs::read_to_string(dir.path().join("gst.log")).unwrap_or_default()
        )
    });
    let log = dir.path().join("http.log");
    let (_server, port) = serve(&media, &log).await;

    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let id = client(
        &state,
        format!("http://127.0.0.1:{port}/index.m3u8"),
        LIVE_CHAIN,
    )
    .await;

    let played = wait_event(&state, id, "hls_played", 60).await;
    assert!(played.get("error").is_none(), "{played}");
    assert!(
        played.get("variant").is_none(),
        "a media playlist: {played}"
    );
    assert_eq!(played["segments"], 5, "{played}");
    assert_eq!(played["endlist"], false, "a live playlist: {played}");
    assert!(
        played["reloads"].as_u64().unwrap() >= 1,
        "five segments from a three-segment window need a reload: {played}"
    );
    let first = played["first_sequence"].as_u64().unwrap();
    assert_eq!(played["last_sequence"], first + 4, "{played}");
    assert_eq!(
        played["gaps"], 0,
        "no segment slid past unfetched: {played}"
    );
    assert_eq!(played["failed"], json!([]), "{played}");
    assert_eq!(played["containers"], json!(["mpegts"]), "{played}");
    assert_eq!(names(&played["codecs"]), ["h264", "mp3"], "{played}");
    assert_eq!(played["continuity_errors"], 0, "{played}");
    let ms = played["duration_ms"].as_u64().unwrap();
    assert!(
        (4000..=5600).contains(&ms),
        "five one-second segments: {played}"
    );

    // The server saw five consecutive segments requested.
    let served = std::fs::read_to_string(&log).unwrap();
    for seq in first..first + 5 {
        let path = format!("GET /seg{seq:05}.ts ");
        assert!(served.contains(&path), "{path} never fetched:\n{served}");
    }
}
