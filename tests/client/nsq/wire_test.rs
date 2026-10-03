use netget::client::nsq::wire;
use netget::server::nsq::wire as nsq;
use serde_json::json;
use tokio::io::AsyncWriteExt;
#[test]
fn typed_commands_roundtrip_and_names_ids_sizes_refused_before_io() {
    for action in [
        json!({"operation":"publish","topic":"a","body":"hello ✓\nRDY 200"}),
        json!({"operation":"publish_many","topic":"a","messages":["a","b"]}),
        json!({"operation":"publish_deferred","topic":"a","body":"later","delay_ms":10}),
        json!({"operation":"subscribe","topic":"a","channel":"b#ephemeral"}),
        json!({"operation":"ready","count":0}),
        json!({"operation":"finish","message_id":"0123456789abcdef"}),
        json!({"operation":"requeue","message_id":"0123456789abcdef","delay_ms":100}),
        json!({"operation":"touch","message_id":"0123456789abcdef"}),
        json!({"operation":"close"}),
        json!({"operation":"nop"}),
    ] {
        let mut v = action;
        v["type"] = json!("nsq_request");
        let command = wire::request(&v).unwrap();
        let bytes = nsq::encode_command(&command);
        assert_eq!(nsq::parse_command(&bytes).unwrap().unwrap().0, command);
    }
    for action in [
        json!({"operation":"publish","topic":"x\nNOP","body":"hello"}),
        json!({"operation":"publish","topic":"a","body":""}),
        json!({"operation":"publish","topic":"a","body":"x".repeat(nsq::MAX_MSG_SIZE+1)}),
        json!({"operation":"publish_many","topic":"a","messages":[]}),
        json!({"operation":"publish_many","topic":"a","messages":vec!["x";wire::MAX_BATCH_MESSAGES+1]}),
        json!({"operation":"publish_many","topic":"a","messages":vec!["x".repeat(nsq::MAX_MSG_SIZE);5]}),
        json!({"operation":"subscribe","topic":"a","channel":"bad\r\nNOP"}),
        json!({"operation":"ready","count":2501}),
        json!({"operation":"ready","count":-1}),
        json!({"operation":"ready","count":"1"}),
        json!({"operation":"requeue","message_id":"0123456789abcdef","delay_ms":3600001}),
        json!({"operation":"finish","message_id":"xxxxxxxxxxxxxxxx"}),
        json!({"operation":"finish","message_id":"0123456789abcdef\nNOP"}),
    ] {
        let mut v = action;
        v["type"] = json!("nsq_request");
        assert!(wire::request(&v).is_err(), "{v:?}");
    }
    let v = json!({"type":"nsq_request","operation":"publish","topic":"a","body":"x".repeat(nsq::MAX_MSG_SIZE)});
    assert!(wire::request(&v).is_ok());
}
#[tokio::test]
async fn frame_bounds_are_checked_from_size_and_invalid_types_refused() {
    for bytes in [
        0u32.to_be_bytes().to_vec(),
        (nsq::MAX_FRAME_DATA as u32 + 5).to_be_bytes().to_vec(),
        [4u32.to_be_bytes(), 3u32.to_be_bytes()].concat(),
    ] {
        let (mut w, mut r) = tokio::io::duplex(16);
        w.write_all(&bytes).await.unwrap();
        assert!(wire::frame(&mut r).await.is_err());
    }
    let (mut w, mut r) = tokio::io::duplex(nsq::MAX_FRAME_DATA + 8);
    w.write_all(&nsq::encode_frame(
        nsq::FRAME_MESSAGE,
        &vec![0; nsq::MAX_FRAME_DATA],
    ))
    .await
    .unwrap();
    assert_eq!(
        wire::frame(&mut r).await.unwrap().unwrap().data.len(),
        nsq::MAX_FRAME_DATA
    );
}
#[tokio::test(start_paused = true)]
async fn partial_header_and_partial_body_have_deadline() {
    for bytes in [vec![0], vec![0, 0, 0, 8, 0, 0, 0, 0, 1]] {
        let (mut w, mut r) = tokio::io::duplex(16);
        w.write_all(&bytes).await.unwrap();
        let result = wire::frame(&mut r).await.unwrap_err();
        assert!(result.to_string().contains("deadline"));
    }
}
#[tokio::test]
async fn blocked_writes_reject_busy_commands_and_disconnect_cancels_without_waiting_deadline() {
    use netget::state::client_handles::{ClientCommand, ClientSendOutcome};
    let (mut writer, mut reader) = tokio::io::duplex(1);
    let (tx, mut rx) = tokio::sync::mpsc::channel(2);
    let (busy_tx, busy_rx) = tokio::sync::oneshot::channel();
    tx.send(ClientCommand {
        action: json!({"type":"nsq_request","operation":"nop"}),
        reply_tx: busy_tx,
    })
    .await
    .unwrap();
    let (disconnect_tx, disconnect_rx) = tokio::sync::oneshot::channel();
    tx.send(ClientCommand {
        action: json!({"type":"disconnect"}),
        reply_tx: disconnect_tx,
    })
    .await
    .unwrap();
    assert!(!tokio::time::timeout(
        std::time::Duration::from_secs(1),
        netget::client::nsq::write_interruptible(&mut writer, b"PUB a\nlong body", &mut rx)
    )
    .await
    .unwrap()
    .unwrap());
    assert!(matches!(
        busy_rx.await.unwrap().unwrap(),
        ClientSendOutcome::Rejected { .. }
    ));
    assert!(matches!(
        disconnect_rx.await.unwrap().unwrap(),
        ClientSendOutcome::Disconnected
    ));
    let mut bytes = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut bytes)
        .await
        .unwrap();
    assert!(bytes.len() < 15);
}
#[tokio::test(start_paused = true)]
async fn blocked_write_has_a_whole_write_deadline() {
    let (mut writer, _reader) = tokio::io::duplex(1);
    let (_tx, mut rx) = tokio::sync::mpsc::channel(1);
    let error =
        netget::client::nsq::write_interruptible(&mut writer, b"large pending write", &mut rx)
            .await
            .unwrap_err();
    assert!(error.to_string().contains("write deadline"));
}

#[test]
fn exact_mpub_body_and_batch_count_limits_are_accepted() {
    let mut messages = vec!["x".repeat(nsq::MAX_MSG_SIZE); 4];
    messages.push("x".repeat(nsq::MAX_MSG_SIZE - 24));
    let command = wire::request(
        &json!({"type":"nsq_request","operation":"publish_many","topic":"a","messages":messages}),
    )
    .unwrap();
    let encoded = nsq::encode_command(&command);
    assert_eq!(
        u32::from_be_bytes(encoded[7..11].try_into().unwrap()) as usize,
        nsq::MAX_BODY_SIZE
    );
    assert!(wire::request(&json!({"type":"nsq_request","operation":"publish_many","topic":"a","messages":vec!["x";wire::MAX_BATCH_MESSAGES]})).is_ok());
}
