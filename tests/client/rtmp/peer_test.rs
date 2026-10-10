//! NetGet's RTMP client against MediaMTX (independent Go RTMP, unchanged): it plays a stream
//! FFmpeg publishes into MediaMTX and reports codecs, metadata and counts; it publishes an
//! FFmpeg-made FLV, from a second connection, that MediaMTX reports with H.264 and AAC tracks. Fails, never skips.
use crate::helpers::rtmp::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::json;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn client_works_against_mediamtx() {
    let mediamtx = start_mediamtx("  all_others:\n").await.unwrap();
    let api = format!("127.0.0.1:{}", mediamtx.extra_ports[0]);
    let dir = tempfile::tempdir().unwrap();
    let clip = make_clip(dir.path()).await;
    let url = format!("rtmp://{}/live/cam", mediamtx.addr());
    let publisher = ffmpeg_command(
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
    wait_ready(&api, "live/cam", Duration::from_secs(15)).await;

    let state = state();
    let cid = client_in(&state, mediamtx.addr(), json!({"app": "live"}))
        .await
        .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    assert!(logs(&state, owner, "rtmp_connected", 1).await[0].request["server"].is_object());
    let sent = state
        .send_to_client(
            cid,
            json!({"type": "rtmp_play", "stream": "cam", "seconds": 3}),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    assert!(matches!(sent, ClientSendOutcome::Sent { .. }), "{sent:?}");
    let play = &logs(&state, owner, "rtmp_report", 1).await[0].request;
    assert!(
        play["status_codes"]
            .as_array()
            .unwrap()
            .contains(&json!("NetStream.Play.Start")),
        "{play}"
    );
    assert_eq!(
        (play["video_codec"].as_str(), play["audio_codec"].as_str()),
        (Some("avc"), Some("aac")),
        "{play}"
    );
    assert!(
        play["video_messages"].as_u64().unwrap() >= 40 && play["keyframes"].as_u64().unwrap() >= 1,
        "{play}"
    );
    assert!(play["last_timestamp"].as_u64().unwrap() > play["first_timestamp"].as_u64().unwrap());

    // MediaMTX treats a connection as one reader or one publisher, so publishing is a second
    // client. While it publishes the clip, MediaMTX must see a ready path with both tracks.
    let pub_id = client_in(
        &state,
        mediamtx.addr(),
        json!({"app": "live", "media_root": dir.path()}),
    )
    .await
    .unwrap();
    let watcher = tokio::spawn({
        let api = api.clone();
        async move { wait_ready(&api, "live/netget", Duration::from_secs(20)).await }
    });
    let sent = state
        .send_to_client(
            pub_id,
            json!({"type": "rtmp_publish", "stream": "netget", "flv_file": clip}),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    assert!(matches!(sent, ClientSendOutcome::Sent { .. }), "{sent:?}");
    let seen = watcher.await.unwrap().to_string();
    assert!(
        seen.contains("H264") && seen.contains("MPEG-4 Audio"),
        "MediaMTX saw: {seen}"
    );
    let publish = &logs(
        &state,
        AccessLogOwner::Client(pub_id.as_u32()),
        "rtmp_report",
        1,
    )
    .await[0]
        .request;
    assert!(
        publish["status_codes"]
            .as_array()
            .unwrap()
            .contains(&json!("NetStream.Publish.Start")),
        "{publish}"
    );
    assert!(
        publish["video_messages"].as_u64().unwrap() >= 140,
        "{publish}"
    );
    drop(publisher);
    state.remove_client(pub_id).await;
    state.remove_client(cid).await;
}
