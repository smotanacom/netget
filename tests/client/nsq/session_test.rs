use super::common::*;
use netget::server::nsq::wire as nsq;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
async fn handshake(r: &mut BufReader<TcpStream>, features: Value) {
    let mut magic = [0; 4];
    r.read_exact(&mut magic).await.unwrap();
    assert_eq!(&magic, b"  V2");
    let mut line = String::new();
    r.read_line(&mut line).await.unwrap();
    assert_eq!(line, "IDENTIFY\n");
    let len = r.read_u32().await.unwrap();
    let mut b = vec![0; len as usize];
    r.read_exact(&mut b).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&b).unwrap()["feature_negotiation"],
        true
    );
    r.get_mut()
        .write_all(&nsq::response_frame(
            &serde_json::to_vec(&features).unwrap(),
        ))
        .await
        .unwrap();
}
fn features() -> Value {
    json!({"max_rdy_count":2500,"tls_v1":false,"snappy":false,"deflate":false,"auth_required":false})
}
#[tokio::test]
async fn netget_pair_publish_flow_requeue_and_recoverable_errors() {
    let state = state();
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let sid=netget::cli::management::ServerForm{protocol:"nsq".into(),port:Some(0),host:Some("127.0.0.1".into()),event_handlers:Some(vec![
        static_handler("nsq_publish",json!([{"type":"send_nsq_ok"}])),
        static_handler("nsq_subscribe",json!([{"type":"send_nsq_ok"}])),
        static_handler("nsq_ready",json!([{"type":"deliver_nsq_messages","messages":[{"body":"pair ✓"},{"body":"next"}]}])),
        static_handler("nsq_requeue",json!([{"type":"deliver_nsq_messages","messages":[{"body":"redelivery","attempts":2}]}])),
        static_handler("*",json!([])),
    ]),..Default::default()}.create(&state,tx).await.unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(a) = state.get_server(sid).await.and_then(|s| s.local_addr) {
                break a;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let id = connected_client(&state, addr.to_string()).await;
    request(
        &state,
        id,
        json!({"operation":"publish","topic":"a","body":"publish ✓"}),
    )
    .await;
    request(
        &state,
        id,
        json!({"operation":"subscribe","topic":"a","channel":"b"}),
    )
    .await;
    send(&state, id, json!({"operation":"ready","count":1})).await;
    let (first_id, first) = event(&state, id, "nsq_message", 0).await;
    assert_eq!(first["body"], "pair ✓");
    send(
        &state,
        id,
        json!({"operation":"touch","message_id":first["message_id"]}),
    )
    .await;
    send(
        &state,
        id,
        json!({"operation":"requeue","message_id":first["message_id"],"delay_ms":0}),
    )
    .await;
    let (next_id, next) = event(&state, id, "nsq_message", first_id).await;
    assert_eq!(next["body"], "next");
    send(
        &state,
        id,
        json!({"operation":"finish","message_id":next["message_id"]}),
    )
    .await;
    let (_, redelivery) = event(&state, id, "nsq_message", next_id).await;
    assert_eq!(redelivery["attempts"], 2);
    let after = latest(&state, id).await;
    send(
        &state,
        id,
        json!({"operation":"finish","message_id":"ffffffffffffffff"}),
    )
    .await;
    assert_eq!(
        event(&state, id, "nsq_error", after).await.1["code"],
        "E_FIN_FAILED"
    );
    assert_eq!(
        request(&state, id, json!({"operation":"close"})).await["status"],
        "CLOSE_WAIT"
    );
    state.remove_client(id).await;
    state.remove_server(sid).await;
}
#[tokio::test]
async fn partial_frame_is_not_cancelled_by_injected_command_and_final_event_survives_eof() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let fixture = tokio::spawn(async move {
        let (s, _) = listener.accept().await.unwrap();
        let mut r = BufReader::new(s);
        handshake(&mut r, features()).await;
        let mut line = String::new();
        r.read_line(&mut line).await.unwrap();
        assert_eq!(line, "PUB a\n");
        let size = r.read_u32().await.unwrap();
        let mut body = vec![0; size as usize];
        r.read_exact(&mut body).await.unwrap();
        let frame = nsq::response_frame(b"OK");
        r.get_mut().write_all(&frame[..3]).await.unwrap();
        tx.send(()).unwrap();
        line.clear();
        r.read_line(&mut line).await.unwrap();
        assert_eq!(line, "NOP\n");
        for byte in &frame[3..] {
            r.get_mut().write_all(&[*byte]).await.unwrap();
            tokio::task::yield_now().await;
        }
    });
    let state = state();
    let id = connected_client(&state, addr.to_string()).await;
    send(
        &state,
        id,
        json!({"operation":"publish","topic":"a","body":"hello"}),
    )
    .await;
    rx.await.unwrap();
    send(&state, id, json!({"operation":"nop"})).await;
    assert_eq!(event(&state, id, "nsq_response", 0).await.1["status"], "OK");
    fixture.await.unwrap();
    state.remove_client(id).await;
}
#[tokio::test]
async fn injected_disconnect_and_stop_close_stalled_identify_and_all_owned_tasks() {
    for remove in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let fixture = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut magic = [0; 4];
            s.read_exact(&mut magic).await.unwrap();
            tx.send(()).unwrap();
            let mut rest = Vec::new();
            s.read_to_end(&mut rest).await.unwrap();
        });
        let state = state();
        let id = client(
            &state,
            addr.to_string(),
            vec![static_handler("*", json!([]))],
        )
        .await;
        rx.await.unwrap();
        assert!(state.client_task_count(id).await >= 3);
        if remove {
            state.remove_client(id).await;
        } else {
            let outcome = state
                .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(1))
                .await
                .unwrap();
            assert!(matches!(
                outcome,
                netget::state::client_handles::ClientSendOutcome::Disconnected
            ));
        }
        tokio::time::timeout(Duration::from_secs(1), fixture)
            .await
            .unwrap()
            .unwrap();
        state.remove_client(id).await;
        assert_eq!(state.client_task_count(id).await, 0);
    }
}
#[tokio::test]
async fn auth_tls_compression_required_negotiation_fails_without_hiding_scope() {
    for key in ["auth_required", "tls_v1", "snappy", "deflate"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut f = features();
        f[key] = json!(true);
        let fixture = tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let mut r = BufReader::new(s);
            handshake(&mut r, f).await;
            let mut b = Vec::new();
            r.read_to_end(&mut b).await.unwrap();
            assert!(b.is_empty());
        });
        let state = state();
        let id = client(
            &state,
            addr.to_string(),
            vec![static_handler("*", json!([]))],
        )
        .await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let netget::state::ClientStatus::Error(s) =
                    state.get_client(id).await.unwrap().status
                {
                    assert!(s.contains(key), "{s}");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        fixture.await.unwrap();
        state.remove_client(id).await;
    }
}
#[tokio::test]
async fn response_followup_chain_stops_at_four() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        let (s, _) = listener.accept().await.unwrap();
        let mut r = BufReader::new(s);
        handshake(&mut r, features()).await;
        for _ in 0..4 {
            let mut line = String::new();
            r.read_line(&mut line).await.unwrap();
            assert_eq!(line, "PUB a\n");
            let n = r.read_u32().await.unwrap();
            let mut b = vec![0; n as usize];
            r.read_exact(&mut b).await.unwrap();
            r.get_mut()
                .write_all(&nsq::response_frame(b"OK"))
                .await
                .unwrap();
        }
        let mut line = String::new();
        assert!(
            tokio::time::timeout(Duration::from_millis(150), r.read_line(&mut line))
                .await
                .is_err()
        );
    });
    let state = state();
    let id = client(
        &state,
        addr.to_string(),
        vec![static_handler(
            "*",
            json!([{"type":"nsq_request","operation":"publish","topic":"a","body":"chain"}]),
        )],
    )
    .await;
    fixture.await.unwrap();
    state.remove_client(id).await;
}
#[tokio::test]
async fn opaque_binary_delivery_preserves_identity_for_explicit_finish() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        let (s, _) = listener.accept().await.unwrap();
        let mut r = BufReader::new(s);
        handshake(&mut r, features()).await;
        let mut line = String::new();
        r.read_line(&mut line).await.unwrap();
        assert_eq!(line, "SUB a b\n");
        r.get_mut()
            .write_all(&nsq::response_frame(b"OK"))
            .await
            .unwrap();
        line.clear();
        r.read_line(&mut line).await.unwrap();
        assert_eq!(line, "RDY 1\n");
        let mut message = 1i64.to_be_bytes().to_vec();
        message.extend(3u16.to_be_bytes());
        message.extend(b"0123456789abcdef");
        message.extend([0xff, 0, 1]);
        r.get_mut()
            .write_all(&nsq::encode_frame(nsq::FRAME_MESSAGE, &message))
            .await
            .unwrap();
        line.clear();
        r.read_line(&mut line).await.unwrap();
        assert_eq!(line, "FIN 0123456789abcdef\n");
    });
    let state = state();
    let id = connected_client(&state, addr.to_string()).await;
    request(
        &state,
        id,
        json!({"operation":"subscribe","topic":"a","channel":"b"}),
    )
    .await;
    send(&state, id, json!({"operation":"ready","count":1})).await;
    let (_, message) = event(&state, id, "nsq_message", 0).await;
    assert_eq!(message["body"], Value::Null);
    assert_eq!(message["body_utf8"], false);
    assert_eq!(message["body_bytes"], 3);
    assert_eq!(message["attempts"], 3);
    send(
        &state,
        id,
        json!({"operation":"finish","message_id":message["message_id"]}),
    )
    .await;
    fixture.await.unwrap();
    state.remove_client(id).await;
}
#[tokio::test]
async fn ready_reduction_keeps_messages_already_in_transit_valid() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        let (s, _) = listener.accept().await.unwrap();
        let mut r = BufReader::new(s);
        handshake(&mut r, features()).await;
        let mut line = String::new();
        r.read_line(&mut line).await.unwrap();
        assert_eq!(line, "SUB a b\n");
        r.get_mut()
            .write_all(&nsq::response_frame(b"OK"))
            .await
            .unwrap();
        line.clear();
        r.read_line(&mut line).await.unwrap();
        assert_eq!(line, "RDY 2\n");
        r.get_mut()
            .write_all(&nsq::message_frame(1, 1, b"0000000000000001", b"first"))
            .await
            .unwrap();
        // Both deliveries may be selected under RDY2; the second can arrive
        // after the client lowers RDY in the opposite TCP direction.
        line.clear();
        r.read_line(&mut line).await.unwrap();
        assert_eq!(line, "RDY 0\n");
        r.get_mut()
            .write_all(&nsq::message_frame(
                2,
                1,
                b"0000000000000002",
                b"already selected",
            ))
            .await
            .unwrap();
        for id in ["0000000000000001", "0000000000000002"] {
            line.clear();
            r.read_line(&mut line).await.unwrap();
            assert_eq!(line, format!("FIN {id}\n"));
        }
    });
    let state = state();
    let id = connected_client(&state, addr.to_string()).await;
    request(
        &state,
        id,
        json!({"operation":"subscribe","topic":"a","channel":"b"}),
    )
    .await;
    send(&state, id, json!({"operation":"ready","count":2})).await;
    let (first_id, first) = event(&state, id, "nsq_message", 0).await;
    send(&state, id, json!({"operation":"ready","count":0})).await;
    let (_, second) = event(&state, id, "nsq_message", first_id).await;
    assert_eq!(second["body"], "already selected");
    for message in [first, second] {
        send(
            &state,
            id,
            json!({"operation":"finish","message_id":message["message_id"]}),
        )
        .await;
    }
    fixture.await.unwrap();
    state.remove_client(id).await;
}
#[tokio::test]
async fn malformed_negotiation_frames_fail_and_fatal_refusals_are_dispatched_before_eof() {
    for frame in [
        nsq::response_frame(b"OK"),
        nsq::response_frame(b"{\"max_rdy_count\":0}"),
        nsq::response_frame(b"{\"max_rdy_count\":2500,\"auth_required\":\"false\"}"),
        nsq::encode_frame(nsq::FRAME_ERROR, b"invalid-error"),
        nsq::encode_frame(nsq::FRAME_MESSAGE, b"short"),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fixture = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut magic = [0; 4];
            s.read_exact(&mut magic).await.unwrap();
            let mut r = BufReader::new(s);
            let mut line = String::new();
            r.read_line(&mut line).await.unwrap();
            let n = r.read_u32().await.unwrap();
            let mut b = vec![0; n as usize];
            r.read_exact(&mut b).await.unwrap();
            r.get_mut().write_all(&frame).await.unwrap();
            let mut remaining = Vec::new();
            r.read_to_end(&mut remaining).await.unwrap();
        });
        let state = state();
        let id = client(
            &state,
            addr.to_string(),
            vec![static_handler("*", json!([]))],
        )
        .await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    state.get_client(id).await.unwrap().status,
                    netget::state::ClientStatus::Error(_)
                ) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        fixture.await.unwrap();
        state.remove_client(id).await;
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        let (s, _) = listener.accept().await.unwrap();
        let mut r = BufReader::new(s);
        handshake(&mut r, features()).await;
        r.get_mut()
            .write_all(&nsq::error_frame("E_BAD_BODY", "refused"))
            .await
            .unwrap();
    });
    let state = state();
    let id = client(
        &state,
        addr.to_string(),
        vec![static_handler("*", json!([]))],
    )
    .await;
    let (_, error) = event(&state, id, "nsq_error", 0).await;
    assert_eq!(error["fatal"], true);
    assert_eq!(error["code"], "E_BAD_BODY");
    fixture.await.unwrap();
    state.remove_client(id).await;
}
#[tokio::test]
async fn parked_delivery_handler_cannot_grow_event_queue_without_bound() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        let (s, _) = listener.accept().await.unwrap();
        let mut r = BufReader::new(s);
        handshake(&mut r, features()).await;
        let mut line = String::new();
        r.read_line(&mut line).await.unwrap();
        assert_eq!(line, "SUB a b\n");
        r.get_mut()
            .write_all(&nsq::response_frame(b"OK"))
            .await
            .unwrap();
        line.clear();
        r.read_line(&mut line).await.unwrap();
        assert_eq!(line, "RDY 20\n");
        let mut bytes = Vec::new();
        for n in 0..20u64 {
            let mut message = 1i64.to_be_bytes().to_vec();
            message.extend(1u16.to_be_bytes());
            message.extend(format!("{n:016x}").as_bytes());
            message.extend(b"waiting");
            bytes.extend(nsq::encode_frame(nsq::FRAME_MESSAGE, &message));
        }
        r.get_mut().write_all(&bytes).await.unwrap();
        let mut remaining = Vec::new();
        r.read_to_end(&mut remaining).await.unwrap();
    });
    let state = state();
    let id = client(
        &state,
        addr.to_string(),
        vec![
            static_handler("nsq_connected", json!([])),
            static_handler("nsq_response", json!([])),
            json!({"event_pattern":"nsq_message","handler":{"type":"manual","timeout_secs":60}}),
        ],
    )
    .await;
    event(&state, id, "nsq_connected", 0).await;
    request(
        &state,
        id,
        json!({"operation":"subscribe","topic":"a","channel":"b"}),
    )
    .await;
    send(&state, id, json!({"operation":"ready","count":20})).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let netget::state::ClientStatus::Error(error) =
                state.get_client(id).await.unwrap().status
            {
                assert!(error.contains("event queue full"), "{error}");
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    fixture.await.unwrap();
    state.remove_client(id).await;
}
