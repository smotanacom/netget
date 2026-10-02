use netget::client::gearman::wire as w;
use netget::server::gearman::wire as g;
use serde_json::json;
use tokio::io::AsyncWriteExt;
#[test]
fn typed_priority_background_worker_requests_and_validation() {
    for (priority, fg, bg) in [
        ("normal", g::SUBMIT_JOB, g::SUBMIT_JOB_BG),
        ("high", g::SUBMIT_JOB_HIGH, g::SUBMIT_JOB_HIGH_BG),
        ("low", g::SUBMIT_JOB_LOW, g::SUBMIT_JOB_LOW_BG),
    ] {
        for background in [false, true] {
            let r=w::request(&json!({"type":"gearman_request","operation":"submit","function_name":"work","unique_id":"u","workload":"payload ✓\u{0000}tail","priority":priority,"background":background})).unwrap();
            assert_eq!(r.packet_type, if background { bg } else { fg });
            let bytes = r.bytes();
            let h = g::parse_header(&bytes).unwrap();
            assert!(h.request);
            assert_eq!(h.size as usize, bytes.len() - g::HEADER_LEN);
            let a = g::split_args(&bytes[g::HEADER_LEN..], 3).unwrap();
            assert_eq!(a[2], "payload ✓\u{0000}tail".as_bytes());
        }
    }
    for (op, t) in [
        ("register", g::CAN_DO),
        ("unregister", g::CANT_DO),
        ("reset", g::RESET_ABILITIES),
        ("grab", g::GRAB_JOB),
        ("grab_unique", g::GRAB_JOB_UNIQ),
        ("sleep", g::PRE_SLEEP),
        ("set_id", g::SET_CLIENT_ID),
        ("progress", g::WORK_STATUS),
        ("data", g::WORK_DATA),
        ("warning", g::WORK_WARNING),
        ("complete", g::WORK_COMPLETE),
        ("fail", g::WORK_FAIL),
        ("exception", g::WORK_EXCEPTION),
    ] {
        let r=w::request(&json!({"type":"gearman_worker","operation":op,"function_name":"work","client_id":"worker","job_handle":"H:test:1","numerator":1,"denominator":2,"data":"data","result":"result","text":"error"})).unwrap();
        assert_eq!(r.packet_type, t);
        assert!(r.worker_only());
    }
    for v in [
        json!({"type":"gearman_request","operation":"submit","function_name":"bad\u{0000}function","workload":"x"}),
        json!({"type":"gearman_request","operation":"submit","function_name":"work","workload":"x","background":"true"}),
        json!({"type":"gearman_request","operation":"submit","function_name":"work","workload":"x","unique_id":"x".repeat(65)}),
        json!({"type":"gearman_worker","operation":"progress","job_handle":"H:test:1","numerator":2,"denominator":1}),
        json!({"type":"gearman_worker","operation":"progress","job_handle":"H:test:1","numerator":0,"denominator":0}),
        json!({"type":"gearman_request","operation":"admin"}),
        json!({"type":"gearman_worker","operation":"grab_all"}),
    ] {
        assert!(w::request(&v).is_err(), "{v}");
    }
    assert!(w::Role::parse("submitter").is_ok());
    assert!(w::Role::parse("worker").is_ok());
    assert!(w::Role::parse("both").is_err());
}
#[tokio::test]
async fn exact_body_limit_is_accepted_and_oversized_request_response_magic_and_size_fail() {
    let payload = "x".repeat(g::MAX_PACKET_BYTES - 3);
    assert_eq!(w::request(&json!({"type":"gearman_request","operation":"submit","function_name":"f","workload":payload})).unwrap().bytes().len(),g::HEADER_LEN+g::MAX_PACKET_BYTES);
    assert!(w::request(&json!({"type":"gearman_request","operation":"submit","function_name":"f","workload":"x".repeat(g::MAX_PACKET_BYTES-2)})).is_err());
    for (magic, size) in [
        (g::MAGIC_REQ.as_slice(), 0u32),
        (g::MAGIC_RES.as_slice(), g::MAX_PACKET_BYTES as u32 + 1),
    ] {
        let mut packet = magic.to_vec();
        packet.extend(g::ECHO_RES.to_be_bytes());
        packet.extend(size.to_be_bytes());
        let mut reader = packet.as_slice();
        assert!(w::frame(&mut reader).await.is_err());
    }
    let bytes = g::response(g::ECHO_RES, &[&vec![b'x'; g::MAX_PACKET_BYTES]]);
    let mut reader = bytes.as_slice();
    assert_eq!(
        w::frame(&mut reader).await.unwrap().unwrap().data.len(),
        g::MAX_PACKET_BYTES
    );
}
#[tokio::test(start_paused = true)]
async fn partial_headers_bodies_and_blocked_writes_have_whole_deadlines() {
    for packet in [vec![0], {
        let mut p = g::response(g::ECHO_RES, &[b"payload"]);
        p.pop();
        p
    }] {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(&packet).await.unwrap();
        assert!(w::frame(&mut reader)
            .await
            .unwrap_err()
            .to_string()
            .contains("deadline"));
    }
    let (mut writer, _reader) = tokio::io::duplex(1);
    let (_tx, mut rx) = tokio::sync::mpsc::channel(2);
    assert!(
        netget::client::gearman::write_interruptible(&mut writer, &[0; 10], &mut rx)
            .await
            .unwrap_err()
            .to_string()
            .contains("deadline")
    );
}
#[tokio::test]
async fn blocked_write_disconnect_interrupts_and_busy_command_is_rejected() {
    use netget::state::client_handles::{ClientCommand, ClientSendOutcome};
    let (mut writer, _reader) = tokio::io::duplex(1);
    let (tx, mut rx) = tokio::sync::mpsc::channel(2);
    let (busy_tx, busy_rx) = tokio::sync::oneshot::channel();
    tx.send(ClientCommand {
        action: json!({"type":"gearman_request","operation":"echo","data":"busy"}),
        reply_tx: busy_tx,
    })
    .await
    .unwrap();
    let (close_tx, close_rx) = tokio::sync::oneshot::channel();
    tx.send(ClientCommand {
        action: json!({"type":"disconnect"}),
        reply_tx: close_tx,
    })
    .await
    .unwrap();
    assert!(
        !netget::client::gearman::write_interruptible(&mut writer, &[0; 10], &mut rx)
            .await
            .unwrap()
    );
    assert!(matches!(
        busy_rx.await.unwrap().unwrap(),
        ClientSendOutcome::Rejected { .. }
    ));
    assert!(matches!(
        close_rx.await.unwrap().unwrap(),
        ClientSendOutcome::Disconnected
    ));
}

#[test]
fn function_unique_handle_and_progress_numeric_boundaries() {
    assert!(w::request(&json!({"type":"gearman_request","operation":"submit","function_name":"x".repeat(g::MAX_FUNCTION_NAME),"unique_id":"u".repeat(g::MAX_UNIQUE),"workload":"x"})).is_ok());
    for function in [String::new(), "x".repeat(g::MAX_FUNCTION_NAME + 1)] {
        assert!(w::request(
            &json!({"type":"gearman_worker","operation":"register","function_name":function})
        )
        .is_err());
    }
    assert!(w::handle(&vec![b'H'; 63]).is_ok());
    assert!(w::handle(&vec![b'H'; 64]).is_err());
    assert!(w::handle(b"bad handle").is_err());
    assert!(w::request(&json!({"type":"gearman_worker","operation":"progress","job_handle":"H:test:1","numerator":u32::MAX,"denominator":u32::MAX})).is_ok());
    assert!(w::request(&json!({"type":"gearman_worker","operation":"progress","job_handle":"H:test:1","numerator":0,"denominator":u32::MAX as u64+1})).is_err());
    assert_eq!(w::number(b"4294967295").unwrap(), u32::MAX as u64);
    assert!(w::number(b"4294967296").is_err());
    assert!(w::number(b"-1").is_err());
    assert!(w::number(b"00x").is_err());
}
