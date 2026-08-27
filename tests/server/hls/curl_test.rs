//! Real-client validation: fetch the HLS playlist and a segment with `curl`.
//!
//! `#[ignore]` because it shells out to `curl` (not guaranteed on CI). Run manually:
//!
//! ```bash
//! ./cargo-isolated.sh test --no-default-features --features hls \
//!     --test server -- --ignored --test-threads=1 hls::curl
//! ```

#![cfg(feature = "hls")]

use crate::server::helpers::*;
use std::time::Duration;

fn curl(url: &str, args: &[&str]) -> (String, String) {
    let mut a: Vec<&str> = vec!["-s", "-i"];
    a.extend_from_slice(args);
    a.push(url);
    let out = std::process::Command::new("curl")
        .args(&a)
        .output()
        .expect("run curl");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[tokio::test]
#[ignore = "requires curl installed; run manually"]
async fn curl_fetches_playlist_and_segment() -> E2EResult<()> {
    let prompt = "listen on port 0 via hls\n\nServe a 2-segment VOD playlist and a segment body.";
    let config = NetGetConfig::new(prompt)
        .with_log_level("off")
        .with_mock(|mock| {
            mock.on_instruction_containing("listen on port")
                .and_instruction_containing("hls")
                .respond_with_actions(serde_json::json!([
                    {"type": "open_server", "port": 0, "base_stack": "hls", "instruction": "hls"}
                ]))
                .expect_calls(1)
                .and()
                .on_event("hls_playlist_request")
                .respond_with_actions(serde_json::json!([{
                    "type": "hls_playlist_response", "target_duration": 6,
                    "segments": [{"uri": "seg0.ts", "duration": 6.0}, {"uri": "seg1.ts", "duration": 6.0}]
                }]))
                .and()
                .on_event("hls_segment_request")
                .respond_with_actions(serde_json::json!([{
                    "type": "hls_segment_response", "content_type": "video/mp2t",
                    "encoding": "hex", "data": "47400010"
                }]))
                .and()
        });

    let test_state = start_netget_server(config).await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let base = format!("http://127.0.0.1:{}", test_state.port);

    let (playlist, _) = {
        let url = format!("{base}/stream.m3u8");
        tokio::task::spawn_blocking(move || curl(&url, &[]))
            .await
            .unwrap()
    };
    println!("curl playlist:\n{playlist}");
    assert!(playlist.contains("200"), "curl playlist status: {playlist}");
    assert!(
        playlist.contains("application/vnd.apple.mpegurl"),
        "content type: {playlist}"
    );
    assert!(playlist.contains("#EXTM3U"), "m3u8 header: {playlist}");
    assert!(playlist.contains("seg0.ts"), "segment listed: {playlist}");

    let (segment, _) = {
        let url = format!("{base}/seg0.ts");
        tokio::task::spawn_blocking(move || curl(&url, &[]))
            .await
            .unwrap()
    };
    println!("curl segment:\n{segment:?}");
    assert!(
        segment.contains("video/mp2t"),
        "segment content type: {segment}"
    );

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    test_state.wait_for_mocks(30).await;
    test_state.verify_mocks().await?;
    test_state.stop().await?;
    Ok(())
}
