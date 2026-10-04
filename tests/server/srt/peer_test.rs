//! libsrt (srt-live-transmit, independent, unchanged) against NetGet's listener: a paced H.264/AAC
//! MPEG-TS stream published by one libsrt caller is read back by another and ffprobe finds both
//! elementary streams; libsrt is refused a resource the policy forbids; the publisher's closing
//! statistics. Fails, never skips.
use crate::helpers::srt::*;
use netget::state::AccessLogOwner;
use serde_json::json;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn libsrt_against_netget_listener() {
    let slt = tool("srt-live-transmit");
    let ffmpeg = tool("ffmpeg");
    let state = state();
    let (sid, addr) = server_in(&state, policy(), json!({})).await;
    let dir = tempfile::tempdir().unwrap();
    let clip = make_clip(dir.path()).await;
    let url = |mode: &str, r: &str| format!("srt://{addr}?streamid=#!::r={r},m={mode}");
    let publisher = pipeline(&format!("'{ffmpeg}' -hide_banner -nostdin -re -stream_loop -1 -i '{clip}' -c copy -f mpegts - | '{slt}' -q file://con '{}'", url("publish", "live/cam")));
    let owner = AccessLogOwner::Server(sid.as_u32());
    let connect = logs(&state, owner, "srt_connect", 1).await;
    assert_eq!(
        (
            connect[0].request["resource"].as_str(),
            connect[0].request["mode"].as_str()
        ),
        (Some("live/cam"), Some("publish"))
    );

    // A second libsrt caller reads four seconds of the relayed stream.
    let out_path = dir.path().join("read.ts");
    let mut reader = tokio::process::Command::new(&slt)
        .args(["-q", &url("request", "live/cam"), "file://con"])
        .stdout(std::fs::File::create(&out_path).unwrap())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    // Read until libsrt has written enough of the stream to probe (about four seconds of it).
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0) <= 120_000 {
        assert!(
            std::time::Instant::now() < deadline,
            "libsrt read too little of the relayed stream"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    reader.kill().await.unwrap();
    let size = std::fs::metadata(&out_path).unwrap().len();
    assert!(size > 80_000, "read {size} bytes");
    let found = codecs(&out_path.display().to_string()).await;
    assert!(
        found.contains(&"h264".to_owned()) && found.contains(&"aac".to_owned()),
        "{found:?}"
    );

    // libsrt reports the rejection (and, with -a:no, does not retry); its exit status is 0
    // either way, so the evidence is what it printed.
    let refused = tokio::process::Command::new(&slt)
        .args(["-a:no", &url("publish", "private/x"), "file://con"])
        .stdin(std::process::Stdio::null())
        .output();
    let refused = tokio::time::timeout(Duration::from_secs(20), refused)
        .await
        .expect("srt-live-transmit kept waiting")
        .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&refused.stdout),
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(
        said.contains("2403") || said.to_ascii_lowercase().contains("reject"),
        "libsrt was not refused:\n{said}"
    );
    drop(publisher);
    let all = state.list_access_logs_for(Some(owner), None).await;
    assert!(all
        .iter()
        .any(|e| e.event_type == "srt_connect" && e.request["resource"] == "private/x"));
    let closed = logs(&state, owner, "srt_closed", 2).await;
    let pubc = closed
        .iter()
        .find(|c| c.request["mode"] == "publish")
        .unwrap_or_else(|| {
            panic!(
                "{:?}",
                closed.iter().map(|c| &c.request).collect::<Vec<_>>()
            )
        });
    // Several seconds of a ~250 kbit/s stream, in whatever message size libsrt chose.
    let stats = &pubc.request["statistics"];
    assert!(
        stats["rx_bytes"].as_u64().unwrap_or(0) > 150_000
            && stats["rx_packets"].as_u64().unwrap_or(0) > 50,
        "{}",
        pubc.request
    );
    state.remove_server(sid).await;
}
