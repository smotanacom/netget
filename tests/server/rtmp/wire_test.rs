//! NetGet's RTMP server from raw bytes and from NetGet's own client: the handshake echo, a
//! refused connect, AMF3 and oversized messages, a ping, a publish/play pair with a synthetic
//! FLV (metadata, sequence headers, keyframe join), injected data messages, and no answer.
use crate::helpers::rtmp::*;
use netget::server::rtmp::{amf0, chunk};
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn handshaken(addr: std::net::SocketAddr) -> TcpStream {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut c1 = vec![3u8];
    c1.extend((0..1536u32).map(|i| (i * 7) as u8));
    s.write_all(&c1).await.unwrap();
    let mut s012 = vec![0u8; 1 + 2 * 1536];
    s.read_exact(&mut s012).await.unwrap();
    assert_eq!(s012[0], 3);
    assert_eq!(&s012[1 + 1536..], &c1[1..], "S2 echoes C1");
    s.write_all(&s012[1..1 + 1536]).await.unwrap();
    s
}

async fn command(s: &mut TcpStream, values: &[Value]) {
    let m = chunk::Message {
        type_id: chunk::COMMAND_AMF0,
        stream_id: 0,
        timestamp: 0,
        payload: amf0::encode(values).unwrap(),
    };
    s.write_all(&chunk::Writer::default().encode(3, &m).unwrap())
        .await
        .unwrap();
}

async fn until_command(s: &mut TcpStream, r: &mut chunk::Reader, name: &str) -> Vec<Value> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let m = r.read(s).await.unwrap();
            if m.type_id == chunk::SET_CHUNK_SIZE {
                r.set_chunk_size(&m.payload).unwrap();
            }
            if m.type_id == chunk::COMMAND_AMF0 {
                let v = amf0::decode_all(&m.payload).unwrap();
                if v[0] == name {
                    return v;
                }
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn wire_rules() {
    let state = state();
    let (sid, addr) = server_in(&state, policy(), json!({})).await;
    // A refused application: _error NetConnection.Connect.Rejected, then close.
    let mut s = handshaken(addr).await;
    command(
        &mut s,
        &[
            json!("connect"),
            json!(1),
            json!({"app": "blocked", "tcUrl": "rtmp://x/blocked"}),
        ],
    )
    .await;
    let mut r = chunk::Reader::default();
    let e = until_command(&mut s, &mut r, "_error").await;
    assert_eq!(
        (e[3]["code"].as_str(), e[3]["description"].as_str()),
        (
            Some("NetConnection.Connect.Rejected"),
            Some("application blocked is closed")
        )
    );
    // Accepted: _result with NetConnection.Connect.Success and objectEncoding 0; a ping is answered.
    let mut s = handshaken(addr).await;
    command(
        &mut s,
        &[json!("connect"), json!(1), json!({"app": "live"})],
    )
    .await;
    let mut r = chunk::Reader::default();
    let ok = until_command(&mut s, &mut r, "_result").await;
    assert_eq!(
        (ok[3]["code"].as_str(), ok[3]["objectEncoding"].as_f64()),
        (Some("NetConnection.Connect.Success"), Some(0.0))
    );
    s.write_all(
        &chunk::Writer::default()
            .encode(2, &chunk::user_control(6, 1234))
            .unwrap(),
    )
    .await
    .unwrap();
    let pong = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let m = r.read(&mut s).await.unwrap();
            if m.type_id == chunk::USER_CONTROL && m.payload[..2] == [0, 7] {
                return m;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(&pong.payload[2..], &1234u32.to_be_bytes());
    // An AMF3 command closes the connection.
    let m = chunk::Message {
        type_id: chunk::COMMAND_AMF3,
        stream_id: 0,
        timestamp: 0,
        payload: vec![0, 2, 0, 1, b'x'],
    };
    s.write_all(&chunk::Writer::default().encode(3, &m).unwrap())
        .await
        .unwrap();
    let mut rest = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut rest))
        .await
        .expect("closed");
    // A message header announcing more than 8 MiB closes the connection before the body.
    let mut s = handshaken(addr).await;
    let mut header = vec![0x03, 0, 0, 0];
    header.extend(&(9u32 * 1024 * 1024).to_be_bytes()[1..]);
    header.extend([chunk::VIDEO, 1, 0, 0, 0]);
    s.write_all(&header).await.unwrap();
    let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut [0u8; 16]))
        .await
        .expect("closed")
        .unwrap_or(0);
    assert_eq!(n, 0);
    // No answer from the handler: the connect is rejected.
    let (sid2, addr2) = server_in(&state, vec![], json!({})).await;
    let mut s = handshaken(addr2).await;
    command(
        &mut s,
        &[json!("connect"), json!(1), json!({"app": "live"})],
    )
    .await;
    let e = until_command(&mut s, &mut chunk::Reader::default(), "_error").await;
    assert_eq!(e[3]["code"], "NetConnection.Connect.Rejected");
    state.remove_server(sid).await;
    state.remove_server(sid2).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_publisher_and_player_with_injected_data() {
    let state = state();
    let (sid, addr) = server_in(&state, policy(), json!({})).await;
    let dir = tempfile::tempdir().unwrap();
    let flv = dir.path().join("tiny.flv");
    std::fs::write(&flv, tiny_flv(4)).unwrap();
    let publisher = client_in(&state, addr.to_string(), json!({"app": "live"}))
        .await
        .unwrap();
    let player = client_in(&state, addr.to_string(), json!({"app": "live"}))
        .await
        .unwrap();
    let play = {
        let state = state.clone();
        tokio::spawn(async move {
            state
                .send_to_client(
                    player,
                    json!({"type": "rtmp_play", "stream": "show", "seconds": 6}),
                    Duration::from_secs(30),
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let publish = {
        let state = state.clone();
        let flv = flv.display().to_string();
        tokio::spawn(async move {
            state
                .send_to_client(
                    publisher,
                    json!({"type": "rtmp_publish", "stream": "show", "flv_file": flv}),
                    Duration::from_secs(30),
                )
                .await
        })
    };
    let owner = AccessLogOwner::Server(sid.as_u32());
    let conn = logs(&state, owner, "rtmp_publish", 1).await[0]
        .connection_id
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let caption = json!({"type": "rtmp_send_data", "handler": "onTextData", "data": {"text": "hello viewers"}});
    let sent = state
        .send_to_peer(sid, conn, caption, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(
        matches!(sent, ClientSendOutcome::Sent { bytes_sent: 1 }),
        "{sent:?}"
    );
    assert!(matches!(
        publish.await.unwrap().unwrap(),
        ClientSendOutcome::Sent { .. }
    ));
    assert!(matches!(
        play.await.unwrap().unwrap(),
        ClientSendOutcome::Sent { .. }
    ));
    let report = |c: netget::state::ClientId| {
        let state = state.clone();
        async move {
            logs(&state, AccessLogOwner::Client(c.as_u32()), "rtmp_report", 1).await[0]
                .request
                .clone()
        }
    };
    let watched = report(player).await;
    assert!(
        watched["status_codes"]
            .as_array()
            .unwrap()
            .contains(&json!("NetStream.Play.Start")),
        "{watched}"
    );
    assert_eq!(
        watched["metadata"]["width"], 64.0,
        "cached onMetaData reached the player: {watched}"
    );
    assert_eq!(watched["video_codec"], "avc");
    assert!(
        watched["video_messages"].as_u64().unwrap() >= 50,
        "{watched}"
    );
    assert_eq!(
        watched["data_messages"],
        json!([{"handler": "onTextData", "data": {"text": "hello viewers"}}])
    );
    let published = report(publisher).await;
    assert_eq!(
        published["video_messages"], 101,
        "100 frames and the AVC sequence header: {published}"
    );
    let ended = logs(&state, owner, "rtmp_publish_ended", 1).await;
    assert_eq!(
        (
            ended[0].request["keyframes"].as_u64(),
            ended[0].request["players"].as_u64()
        ),
        (Some(5), Some(1)),
        "{}",
        ended[0].request
    );
    state.remove_client(publisher).await;
    state.remove_client(player).await;
    state.remove_server(sid).await;
}
