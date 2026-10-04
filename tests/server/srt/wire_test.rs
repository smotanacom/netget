//! NetGet's listener with NetGet's caller and the stream-ID parser: bad and forbidden stream IDs
//! and a second publisher refused in the handshake, the relay byte-for-byte with the MPEG-TS
//! programme read back, an injected text message, the closing statistics, and no answer.
use crate::helpers::srt::*;
use netget::server::srt::streamid::{parse, Target};
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::json;
use std::time::Duration;

#[test]
fn stream_ids() {
    let t = |r: &str, m: &str, u: Option<&str>| Target {
        resource: r.into(),
        mode: m.into(),
        user: u.map(str::to_owned),
    };
    assert_eq!(
        parse("#!::r=live/cam,m=publish,u=alice"),
        Ok(t("live/cam", "publish", Some("alice")))
    );
    assert_eq!(parse("#!::r=live/cam"), Ok(t("live/cam", "request", None)));
    assert_eq!(
        parse("publish:live/cam:alice:secret"),
        Ok(t("live/cam", "publish", Some("alice")))
    );
    assert_eq!(
        parse("read:live/cam?x=1"),
        Ok(t("live/cam", "request", None))
    );
    assert_eq!(parse("plain"), Ok(t("plain", "request", None)));
    assert!(parse("#!::m=bidirectional").is_err());
    assert!(parse("#!::r=x,m=sideways").is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn relay_refusals_and_injected_text() {
    let state = state();
    let (sid, addr) = server_in(&state, policy(), json!({"idle_timeout_secs": 2})).await;
    for (stream_id, why) in [
        ("", "an empty resource"),
        ("#!::r=private/x,m=publish", "a forbidden resource"),
        ("#!::m=bidirectional", "an unsupported mode"),
    ] {
        let r = client_in(&state, addr.to_string(), json!({"stream_id": stream_id})).await;
        assert!(r.is_err(), "{why} was admitted");
    }
    let reader = client_in(
        &state,
        addr.to_string(),
        json!({"stream_id": "#!::r=live/show,m=request"}),
    )
    .await
    .unwrap();
    let publisher = client_in(
        &state,
        addr.to_string(),
        json!({"stream_id": "#!::r=live/show,m=publish"}),
    )
    .await
    .unwrap();
    assert!(
        client_in(
            &state,
            addr.to_string(),
            json!({"stream_id": "#!::r=live/show,m=publish"})
        )
        .await
        .is_err(),
        "a second publisher is refused"
    );

    let dir = tempfile::tempdir().unwrap();
    let ts = dir.path().join("tiny.ts");
    let data = tiny_ts(600);
    std::fs::write(&ts, &data).unwrap();
    let receive = {
        let state = state.clone();
        tokio::spawn(async move {
            state
                .send_to_client(
                    reader,
                    json!({"type": "srt_receive", "seconds": 4}),
                    Duration::from_secs(30),
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let sent = state.send_to_client(publisher, json!({"type": "srt_send_file", "path": ts.display().to_string(), "bitrate_kbps": 2000}), Duration::from_secs(30)).await.unwrap();
    assert!(matches!(sent, ClientSendOutcome::Sent { .. }), "{sent:?}");
    let owner = AccessLogOwner::Server(sid.as_u32());
    let conn = logs(&state, owner, "srt_connect", 1)
        .await
        .iter()
        .find(|e| e.request["mode"] == "request")
        .unwrap()
        .connection_id
        .unwrap();
    let text = json!({"type": "srt_send_text", "text": "hello from the operator"});
    assert!(matches!(
        state
            .send_to_peer(sid, conn, text, Duration::from_secs(5))
            .await
            .unwrap(),
        ClientSendOutcome::Sent { .. }
    ));
    assert!(matches!(
        receive.await.unwrap().unwrap(),
        ClientSendOutcome::Sent { .. }
    ));
    let r = &logs(
        &state,
        AccessLogOwner::Client(reader.as_u32()),
        "srt_report",
        1,
    )
    .await[0]
        .request;
    assert_eq!(
        r["ts_packets"].as_u64(),
        Some(602),
        "every packet relayed: {r}"
    );
    assert_eq!(
        r["bytes"].as_u64(),
        Some(data.len() as u64 + "hello from the operator".len() as u64),
        "{r}"
    );
    assert_eq!(r["stream_types"], json!(["aac", "h264"]));
    assert_eq!(r["texts"], json!(["hello from the operator"]));
    // The idle publisher is closed after idle_timeout_secs, with its statistics.
    let closed = logs(&state, owner, "srt_closed", 1).await;
    let pubc = closed
        .iter()
        .find(|c| c.request["mode"] == "publish")
        .unwrap();
    assert_eq!(
        pubc.request["statistics"]["rx_packets"].as_u64(),
        Some(86),
        "602 packets in 86 messages of seven: {}",
        pubc.request
    );
    state.remove_client(reader).await;
    state.remove_client(publisher).await;

    // No answer from the handler: refused in the handshake.
    let (sid2, addr2) = server_in(&state, vec![], json!({})).await;
    assert!(client_in(
        &state,
        addr2.to_string(),
        json!({"stream_id": "#!::r=live/x"})
    )
    .await
    .is_err());
    state.remove_server(sid).await;
    state.remove_server(sid2).await;
}
