use super::common::*;
use netget::state::client_handles::ClientSendOutcome;
use serde_json::{json, Value};
use std::{sync::atomic::Ordering, time::Duration};
use tokio_tungstenite::tungstenite::Message;
#[tokio::test]
async fn correlation_live_eose_local_close_and_late_selected_frames_keep_their_meaning() {
    let peer = mock_peer("normal").await;
    let state = state();
    let id = client(
        &state,
        peer.address.clone(),
        json!({"secret_key":SECRET}),
        vec![static_handler("*", json!([]))],
    )
    .await;
    let (_, connected) = event(&state, id, "nostr_connected", 0).await;
    assert_eq!(
        connected["pubkey"],
        netget::server::nostr::wire::RelayKey::from_hex(SECRET)
            .unwrap()
            .pubkey_hex()
    );
    let after = latest(&state, id).await;
    send(
        &state,
        id,
        json!({"type":"nostr_publish","kind":1,"content":"film ☃","created_at":1700000000}),
    )
    .await;
    let (_, result) = event(&state, id, "nostr_publish_result", after).await;
    assert_eq!(result["accepted"], true);
    assert_eq!(result["status"], "ok");
    assert_eq!(result["id"], peer.seen()[0][1]["id"]);
    peer.mode("reject");
    let after = latest(&state, id).await;
    send(
        &state,
        id,
        json!({"type":"nostr_publish","kind":1,"content":"rejected"}),
    )
    .await;
    let (_, result) = event(&state, id, "nostr_publish_result", after).await;
    assert_eq!(result["accepted"], false);
    assert_eq!(result["reason_prefix"], "blocked");
    let after = latest(&state, id).await;
    send(&state,id,json!({"type":"nostr_subscribe","subscription_id":"live","filters":[{"kinds":[1],"limit":0}]})).await;
    assert_eq!(
        event(&state, id, "nostr_subscription", after).await.1["status"],
        "eose"
    );
    let after = latest(&state, id).await;
    peer.frame(json!(["EVENT", "live", signed("future film")]))
        .await;
    let (_, received) = event(&state, id, "nostr_received_event", after).await;
    assert_eq!(received["stored_phase"], false);
    assert_eq!(received["event"]["content"], "future film");
    assert_eq!(received["matches_current_filters"], true);
    let after = latest(&state, id).await;
    send(
        &state,
        id,
        json!({"type":"nostr_close","subscription_id":"live"}),
    )
    .await;
    assert_eq!(
        event(&state, id, "nostr_subscription", after).await.1["status"],
        "local_close"
    );
    let after = latest(&state, id).await;
    for frame in [
        json!(["EVENT", "live", signed("already selected")]),
        json!(["EOSE", "live"]),
        json!(["CLOSED", "live", "error: already selected"]),
        json!(["OK", "a".repeat(64), true, ""]),
    ] {
        peer.frame(frame).await;
    }
    peer.raw(Message::Ping(vec![59, 60])).await;
    wait(|| peer.seen().iter().any(|v| v["pong"] == json!([59, 60]))).await;
    let entries = state
        .list_access_logs_for(
            Some(netget::state::AccessLogOwner::Client(id.as_u32())),
            None,
        )
        .await;
    assert!(!entries.iter().any(|e| e.id > after
        && matches!(
            e.event_type.as_str(),
            "nostr_received_event" | "nostr_subscription" | "nostr_publish_result"
        )));
    assert!(!serde_json::to_string(&entries).unwrap().contains(SECRET));
    assert!(state.has_client_handle(id).await);
    state.remove_client(id).await;
    wait(|| peer.closed.load(Ordering::SeqCst)).await;
    peer.stop().await;
}
#[tokio::test]
async fn replacement_reports_current_filter_match_and_native_closed_removes_only_that_id() {
    let peer = mock_peer("silent").await;
    let state = state();
    let id = connected_client(&state, peer.address.clone()).await;
    send(
        &state,
        id,
        json!({"type":"nostr_subscribe","subscription_id":"same","filters":[{"kinds":[1]}]}),
    )
    .await;
    send(
        &state,
        id,
        json!({"type":"nostr_subscribe","subscription_id":"same","filters":[{"kinds":[7]}]}),
    )
    .await;
    let after = latest(&state, id).await;
    peer.frame(json!(["EVENT", "same", signed("old selected frame")]))
        .await;
    assert_eq!(
        event(&state, id, "nostr_received_event", after).await.1["matches_current_filters"],
        false
    );
    let after = latest(&state, id).await;
    peer.frame(json!(["CLOSED", "same", "auth-required: sign in"]))
        .await;
    let (_, receipt) = event(&state, id, "nostr_subscription", after).await;
    assert_eq!(receipt["status"], "closed");
    assert_eq!(receipt["reason_prefix"], "auth-required");
    rejected(
        &state,
        id,
        json!({"type":"nostr_close","subscription_id":"same"}),
        "not open",
    )
    .await;
    state.remove_client(id).await;
    peer.stop().await;
}
#[tokio::test]
async fn limits_refuse_before_wire_and_publish_timeout_does_not_claim_rejection() {
    let peer = mock_peer("no_ok").await;
    let state = state();
    let id = client(
        &state,
        peer.address.clone(),
        json!({"request_timeout_secs":1}),
        vec![static_handler("*", json!([]))],
    )
    .await;
    event(&state, id, "nostr_connected", 0).await;
    let publish = json!({"type":"nostr_publish","kind":1,"content":"same","created_at":1700000000});
    let after = latest(&state, id).await;
    send(&state, id, publish.clone()).await;
    rejected(&state, id, publish, "duplicate pending").await;
    let (_, timeout) = event(&state, id, "nostr_publish_result", after).await;
    assert_eq!(timeout["status"], "timeout");
    assert_eq!(timeout["accepted"], Value::Null);
    for n in 0..20 {
        send(
            &state,
            id,
            json!({"type":"nostr_subscribe","subscription_id":format!("s{n}"),"filters":[{}]}),
        )
        .await;
    }
    rejected(
        &state,
        id,
        json!({"type":"nostr_subscribe","subscription_id":"overflow","filters":[{}]}),
        "subscription limit",
    )
    .await;
    send(
        &state,
        id,
        json!({"type":"nostr_subscribe","subscription_id":"s0","filters":[{"limit":0}]}),
    )
    .await;
    send(
        &state,
        id,
        json!({"type":"nostr_close","subscription_id":"s0"}),
    )
    .await;
    send(
        &state,
        id,
        json!({"type":"nostr_subscribe","subscription_id":"overflow","filters":[{}]}),
    )
    .await;
    for n in 0..16 {
        send(&state,id,json!({"type":"nostr_publish","kind":1,"content":format!("pending{n}"),"created_at":1700000000})).await;
    }
    wait(|| {
        peer.seen()
            .iter()
            .filter(|value| value[0] == "EVENT")
            .count()
            == 17
    })
    .await;
    let seen = peer.seen().len();
    rejected(
        &state,
        id,
        json!({"type":"nostr_publish","kind":1,"content":"overflow"}),
        "pending publish limit",
    )
    .await;
    let mut deep = Value::String(SECRET.into());
    for _ in 0..10000 {
        deep = Value::Array(vec![deep]);
    }
    rejected(&state, id, deep, "depth/node/retained-content").await;
    assert_eq!(peer.seen().len(), seen);
    state.remove_client(id).await;
    peer.stop().await;
}
#[tokio::test]
async fn relay_info_deadline_keeps_websocket_reads_live_and_cancels_owned_http() {
    let peer = mock_peer("info_stall").await;
    let state = state();
    let id = client(
        &state,
        peer.address.clone(),
        json!({"request_timeout_secs":1}),
        vec![static_handler("*", json!([]))],
    )
    .await;
    event(&state, id, "nostr_connected", 0).await;
    let after = latest(&state, id).await;
    send(&state, id, json!({"type":"nostr_relay_info"})).await;
    wait(|| peer.info_seen.load(Ordering::SeqCst)).await;
    rejected(
        &state,
        id,
        json!({"type":"nostr_relay_info"}),
        "already pending",
    )
    .await;
    peer.frame(json!(["NOTICE", "live while metadata is pending"]))
        .await;
    assert_eq!(
        event(&state, id, "nostr_notice", after).await.1["message"],
        "live while metadata is pending"
    );
    let after = latest(&state, id).await;
    event(&state, id, "nostr_request_error", after).await;
    wait(|| peer.info_closed.load(Ordering::SeqCst)).await;
    peer.mode("normal");
    let after = latest(&state, id).await;
    send(&state, id, json!({"type":"nostr_relay_info"})).await;
    let (_, info) = event(&state, id, "nostr_relay_information", after).await;
    assert_eq!(info["information"]["supported_nips"], json!([1, 11, 42]));
    assert!(info["information"].get("unknown").is_none());
    peer.info_seen.store(false, Ordering::SeqCst);
    peer.info_closed.store(false, Ordering::SeqCst);
    peer.mode("info_stall");
    send(&state, id, json!({"type":"nostr_relay_info"})).await;
    wait(|| peer.info_seen.load(Ordering::SeqCst)).await;
    assert!(matches!(
        state
            .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(2))
            .await
            .unwrap(),
        ClientSendOutcome::Disconnected
    ));
    wait(|| peer.info_closed.load(Ordering::SeqCst) && peer.closed.load(Ordering::SeqCst)).await;
    state.remove_client(id).await;
    peer.stop().await;
}
#[tokio::test]
async fn unsupported_extensions_never_echo_challenges_and_handler_followups_are_bounded() {
    let peer = mock_peer("normal").await;
    let state = state();
    let publish = json!({"type":"nostr_publish","kind":1,"content":"bounded handler","created_at":1700000000});
    let id = client(
        &state,
        peer.address.clone(),
        json!({}),
        vec![
            static_handler("nostr_connected", json!([publish.clone()])),
            static_handler("nostr_publish_result", json!([publish])),
            static_handler("*", json!([])),
        ],
    )
    .await;
    let mut after = 0;
    for _ in 0..4 {
        after = event(&state, id, "nostr_publish_result", after).await.0;
    }
    peer.raw(Message::Ping(vec![4])).await;
    wait(|| peer.seen().iter().any(|v| v["pong"] == json!([4]))).await;
    assert_eq!(peer.seen().iter().filter(|v| v[0] == "EVENT").count(), 4);
    for frame in [
        json!(["AUTH", "challenge must never be echoed"]),
        json!(["COUNT","sub",{"count":9}]),
    ] {
        let after = latest(&state, id).await;
        peer.frame(frame).await;
        let (_, notice) = event(&state, id, "nostr_notice", after).await;
        assert_eq!(notice["supported"], false);
        assert!(!notice.to_string().contains("challenge must never"));
    }
    state.remove_client(id).await;
    peer.stop().await;
}
#[tokio::test]
async fn manual_connected_dispatch_has_injection_channel_and_removal_closes_peer() {
    let peer = mock_peer("normal").await;
    let state = state();
    let id = client(
        &state,
        peer.address.clone(),
        json!({}),
        vec![json!({"event_pattern":"*","handler":{"type":"manual"}})],
    )
    .await;
    assert!(state.has_client_handle(id).await);
    send(
        &state,
        id,
        json!({"type":"nostr_publish","kind":1,"content":"injection during manual event"}),
    )
    .await;
    wait(|| peer.seen().iter().any(|v| v[0] == "EVENT")).await;
    state.remove_client(id).await;
    wait(|| peer.closed.load(Ordering::SeqCst)).await;
    peer.stop().await;
}
#[tokio::test]
async fn malformed_binary_or_invalid_signed_events_end_the_bounded_session() {
    for frame in [
        Message::Text("[\"OK\",\"bad\",true,\"\"]".into()),
        Message::Binary(vec![1]),
        Message::Text(json!(["EVENT","missing",{"id":"bad"}]).to_string()),
    ] {
        let peer = mock_peer("normal").await;
        let state = state();
        let id = connected_client(&state, peer.address.clone()).await;
        peer.raw(frame).await;
        failed(&state, id, "bounded session failure").await;
        wait(|| peer.closed.load(Ordering::SeqCst)).await;
        state.remove_client(id).await;
        peer.stop().await;
    }
    let state = state();
    for (url, params) in [
        ("http://localhost:7777", json!({})),
        ("ws://user:password@localhost:7777", json!({})),
        ("ws://localhost:7777/?token=secret", json!({})),
        (
            "ws://localhost:7777/",
            json!({"secret_key":SECRET.to_owned()+"bad"}),
        ),
        ("ws://localhost:7777/", json!({"request_timeout_secs":31})),
    ] {
        let error = refused_start(&state, url.into(), params).await;
        assert!(!error.contains(SECRET));
    }
}
#[tokio::test]
async fn a_parked_manual_handler_cannot_retain_an_unbounded_relay_queue() {
    let peer = mock_peer("normal").await;
    let state = state();
    let id = client(
        &state,
        peer.address.clone(),
        json!({}),
        vec![json!({"event_pattern":"*","handler":{"type":"manual"}})],
    )
    .await;
    peer.raw(Message::Ping(vec![8])).await;
    wait(|| peer.seen().iter().any(|v| v["pong"] == json!([8]))).await;
    for _ in 0..20 {
        peer.frame(json!(["NOTICE", "bounded queued notice"])).await;
    }
    failed(&state, id, "bounded session failure").await;
    wait(|| peer.closed.load(Ordering::SeqCst)).await;
    assert!(!state.has_client_handle(id).await);
    state.remove_client(id).await;
    peer.stop().await;
}
