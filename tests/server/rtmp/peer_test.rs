//! Independent RTMP peers, unchanged, against NetGet's server: FFmpeg publishes an H.264/AAC
//! clip, ffprobe and FFmpeg play it back (decoding frames), and MediaMTX (its own Go RTMP stack)
//! pulls it and reports its tracks; FFmpeg is refused an app, a stream key and a play the
//! policy rejects. Fails, never skips.
use crate::helpers::rtmp::*;
use netget::state::AccessLogOwner;
use serde_json::{json, Value};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn ffmpeg_and_mediamtx_against_netget_server() {
    let state = state();
    let (sid, addr) = server_in(&state, policy(), json!({})).await;
    let dir = tempfile::tempdir().unwrap();
    let clip = make_clip(dir.path()).await;
    let url = format!("rtmp://{addr}/live/cam");
    // The clip loops so the stream is live for as long as the players and MediaMTX need it.
    let mut publisher = ffmpeg_command(
        "ffmpeg",
        &[
            "-re",
            "-stream_loop",
            "-1",
            "-i",
            &clip,
            "-c",
            "copy",
            "-f",
            "flv",
            &url,
        ],
    )
    .stderr(std::process::Stdio::null())
    .spawn()
    .unwrap();
    let owner = AccessLogOwner::Server(sid.as_u32());
    logs(&state, owner, "rtmp_publish", 1).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let probe = ffmpeg_command(
        "ffprobe",
        &[
            "-v",
            "error",
            "-rw_timeout",
            "10000000",
            "-show_streams",
            "-of",
            "json",
            &url,
        ],
    )
    .output()
    .await
    .unwrap();
    assert!(
        probe.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&probe.stderr)
    );
    let streams: Value = serde_json::from_slice(&probe.stdout).unwrap();
    let codecs: Vec<&str> = streams["streams"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["codec_name"].as_str().unwrap())
        .collect();
    assert!(
        codecs.contains(&"h264") && codecs.contains(&"aac"),
        "{streams}"
    );
    let video = streams["streams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["codec_name"] == "h264")
        .unwrap();
    assert_eq!(
        (video["width"].as_u64(), video["height"].as_u64()),
        (Some(320), Some(240))
    );

    let play = ffmpeg_command(
        "ffmpeg",
        &[
            "-rw_timeout",
            "10000000",
            "-i",
            &url,
            "-t",
            "2",
            "-f",
            "null",
            "-",
        ],
    )
    .output()
    .await
    .unwrap();
    let err = String::from_utf8_lossy(&play.stderr);
    assert!(play.status.success(), "ffmpeg play failed:\n{err}");
    let frames: u64 = err
        .rsplit("frame=")
        .next()
        .and_then(|s| s.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    assert!(frames >= 25, "decoded {frames} frames:\n{err}");

    let mediamtx = start_mediamtx(&format!("  cam:\n    source: {url}\n"))
        .await
        .unwrap();
    let path = wait_ready(
        &format!("127.0.0.1:{}", mediamtx.extra_ports[0]),
        "cam",
        Duration::from_secs(15),
    )
    .await;
    let text = path.to_string();
    assert!(
        text.contains("H264") && text.contains("MPEG-4 Audio"),
        "MediaMTX tracks: {path}"
    );
    drop(mediamtx);

    // Stopping the publisher ends the stream; the handler is told what was published.
    publisher.kill().await.unwrap();
    let ended = logs(&state, owner, "rtmp_publish_ended", 1).await;
    let e = &ended[0].request;
    assert_eq!(
        (
            e["stream"].as_str(),
            e["has_video_header"].as_bool(),
            e["has_audio_header"].as_bool()
        ),
        (Some("cam"), Some(true), Some(true)),
        "{e}"
    );
    assert!(
        e["video_messages"].as_u64().unwrap() >= 140 && e["keyframes"].as_u64().unwrap() >= 5,
        "{e}"
    );

    // Refusals: FFmpeg fails, and the server asked the policy each time.
    for target in [
        format!("rtmp://{addr}/live/forbidden"),
        format!("rtmp://{addr}/blocked/x"),
    ] {
        let out = ffmpeg_command(
            "ffmpeg",
            &[
                "-re", "-i", &clip, "-t", "1", "-c", "copy", "-f", "flv", &target,
            ],
        )
        .output()
        .await
        .unwrap();
        assert!(!out.status.success(), "{target} was accepted");
    }
    let probe = ffmpeg_command(
        "ffprobe",
        &[
            "-v",
            "error",
            "-rw_timeout",
            "5000000",
            &format!("rtmp://{addr}/live/secret"),
        ],
    )
    .output()
    .await
    .unwrap();
    assert!(!probe.status.success(), "a refused play succeeded");
    let all = state.list_access_logs_for(Some(owner), None).await;
    let saw = |kind: &str, key: &str, value: &str| {
        all.iter()
            .any(|e| e.event_type == kind && e.request[key] == value)
    };
    assert!(
        saw("rtmp_publish", "stream", "forbidden")
            && saw("rtmp_connect", "app", "blocked")
            && saw("rtmp_play", "stream", "secret")
    );
    state.remove_server(sid).await;
}
