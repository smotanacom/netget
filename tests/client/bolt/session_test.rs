use super::common::*;
use netget::state::client_handles::ClientSendOutcome;
use serde_json::json;
use std::time::Duration;
#[tokio::test]
async fn native_paging_qid_discard_and_explicit_transaction_states_are_correlated() {
    let peer = MockPeer::start(8, "normal").await;
    let state = state();
    let id = authenticated_mock(&state, &peer).await;
    rejected(&state, id, json!({"type":"bolt_pull"}), "open result").await;
    let started = receipt(
        &state,
        id,
        json!({"type":"bolt_run","query":"RETURN 1"}),
        "bolt_query_started",
    )
    .await;
    assert_eq!(started["fields"], json!(["x"]));
    assert_eq!(started["qid"], 7);
    assert_eq!(started["metadata"]["t_first"], 2);
    let first = receipt(
        &state,
        id,
        json!({"type":"bolt_pull","n":1}),
        "bolt_result_page",
    )
    .await;
    assert_eq!(first["records"], json!([[1]]));
    assert_eq!(first["has_more"], true);
    assert!(first["summary"].get("bookmark").is_none());
    rejected(
        &state,
        id,
        json!({"type":"bolt_run","query":"RETURN 2"}),
        "no open result",
    )
    .await;
    let last = receipt(
        &state,
        id,
        json!({"type":"bolt_pull","n":3}),
        "bolt_result_page",
    )
    .await;
    assert_eq!(last["records"], json!([[2], [3]]));
    assert_eq!(last["has_more"], false);
    assert_eq!(last["summary"]["bookmark"], "opaque-bookmark");
    receipt(
        &state,
        id,
        json!({"type":"bolt_begin","mode":"read"}),
        "bolt_session_result",
    )
    .await;
    rejected(
        &state,
        id,
        json!({"type":"bolt_run","query":"RETURN 1","database":"other"}),
        "BEGIN",
    )
    .await;
    receipt(
        &state,
        id,
        json!({"type":"bolt_run","query":"RETURN 1"}),
        "bolt_query_started",
    )
    .await;
    rejected(
        &state,
        id,
        json!({"type":"bolt_commit"}),
        "without open result",
    )
    .await;
    let discarded = receipt(
        &state,
        id,
        json!({"type":"bolt_discard"}),
        "bolt_result_page",
    )
    .await;
    assert_eq!(discarded["records"], json!([]));
    assert_eq!(discarded["in_transaction"], true);
    let committed = receipt(
        &state,
        id,
        json!({"type":"bolt_commit"}),
        "bolt_session_result",
    )
    .await;
    assert_eq!(committed["in_transaction"], false);
    assert!(committed["metadata"].get("bookmark").is_none());
    {
        let seen = peer.seen.lock().unwrap();
        let discard = seen
            .iter()
            .find(|v| {
                matches!(
                    v,
                    netget::server::bolt::packstream::Value::Struct { tag: 0x2f, .. }
                )
            })
            .unwrap();
        let netget::server::bolt::packstream::Value::Struct { fields, .. } = discard else {
            panic!()
        };
        assert_eq!(
            fields[0]
                .get("n")
                .and_then(netget::server::bolt::packstream::Value::as_int),
            Some(-1)
        );
    }
    state.remove_client(id).await;
    wait_closed(&peer).await;
    peer.stop().await;
}
#[tokio::test]
async fn failure_and_ignored_withhold_tentative_records_and_reset_recovers() {
    for mode in ["failure-after-record", "ignored"] {
        let peer = MockPeer::start(8, mode).await;
        let state = state();
        let id = authenticated_mock(&state, &peer).await;
        receipt(
            &state,
            id,
            json!({"type":"bolt_run","query":"RETURN 1"}),
            "bolt_query_started",
        )
        .await;
        let after = latest(&state, id).await;
        let failure = receipt(
            &state,
            id,
            json!({"type":"bolt_pull","n":1}),
            "bolt_failure",
        )
        .await;
        assert_eq!(failure["records_discarded"], 1);
        assert_eq!(failure["ignored"], mode == "ignored");
        let logs = state
            .list_access_logs_for(
                Some(netget::state::AccessLogOwner::Client(id.as_u32())),
                None,
            )
            .await;
        assert!(!logs
            .iter()
            .any(|e| e.id > after && e.event_type == "bolt_result_page"));
        rejected(
            &state,
            id,
            json!({"type":"bolt_run","query":"RETURN 2"}),
            "ready phase",
        )
        .await;
        receipt(
            &state,
            id,
            json!({"type":"bolt_reset"}),
            "bolt_session_result",
        )
        .await;
        peer.set("normal");
        receipt(
            &state,
            id,
            json!({"type":"bolt_run","query":"RETURN 2"}),
            "bolt_query_started",
        )
        .await;
        state.remove_client(id).await;
        wait_closed(&peer).await;
        peer.stop().await;
    }
}
#[tokio::test]
async fn version_five_zero_authenticates_in_hello_and_refuses_logon_actions() {
    let peer = MockPeer::start(0, "normal").await;
    let state = state();
    let id = client(
        &state,
        peer.address.clone(),
        json!({"username":"neo4j","password":"selected"}),
        vec![static_handler("*", json!([]))],
    )
    .await;
    let connected = event(&state, id, "bolt_connected", 0).await.1;
    assert_eq!(connected["authentication_verified"], true);
    assert_eq!(connected["version"], "5.0");
    rejected(&state, id, json!({"type":"bolt_login"}), "5.1+").await;
    receipt(
        &state,
        id,
        json!({"type":"bolt_run","query":"RETURN 1"}),
        "bolt_query_started",
    )
    .await;
    assert!(!peer.seen.lock().unwrap().iter().any(|v| matches!(
        v,
        netget::server::bolt::packstream::Value::Struct { tag: 0x6a, .. }
    )));
    state.remove_client(id).await;
    peer.stop().await;
}
#[tokio::test]
async fn schema_and_byte_failures_close_without_publishing_partial_pages() {
    for mode in [
        "wrong-width",
        "too-many",
        "bad-summary",
        "oversize",
        "page-bytes",
    ] {
        let peer = MockPeer::start(8, mode).await;
        let state = state();
        let id = authenticated_mock(&state, &peer).await;
        receipt(
            &state,
            id,
            json!({"type":"bolt_run","query":"RETURN 1"}),
            "bolt_query_started",
        )
        .await;
        let after = latest(&state, id).await;
        send(
            &state,
            id,
            json!({"type":"bolt_pull","n":if mode=="page-bytes"{100}else{1}}),
        )
        .await;
        failed(&state, id, "backend outcome unknown").await;
        let logs = state
            .list_access_logs_for(
                Some(netget::state::AccessLogOwner::Client(id.as_u32())),
                None,
            )
            .await;
        assert!(
            !logs
                .iter()
                .any(|e| e.id > after && e.event_type == "bolt_result_page"),
            "{mode}"
        );
        assert!(!state.has_client_handle(id).await);
        state.remove_client(id).await;
        peer.stop().await;
    }
}
#[tokio::test]
async fn whole_operation_deadline_is_not_extended_by_peer_noops() {
    let peer = MockPeer::start(8, "noop").await;
    let state = state();
    let id = client(
        &state,
        peer.address.clone(),
        json!({"request_timeout_secs":1}),
        vec![static_handler("*", json!([]))],
    )
    .await;
    receipt(
        &state,
        id,
        json!({"type":"bolt_login"}),
        "bolt_authentication",
    )
    .await;
    send(&state, id, json!({"type":"bolt_run","query":"RETURN 1"})).await;
    failed(&state, id, "backend outcome unknown").await;
    state.remove_client(id).await;
    peer.stop().await;
}
#[tokio::test]
async fn pending_injection_refuses_other_actions_and_disconnect_or_removal_closes_owned_io() {
    for remove in [false, true] {
        let peer = MockPeer::start(8, "hang").await;
        let state = state();
        let id = authenticated_mock(&state, &peer).await;
        send(&state, id, json!({"type":"bolt_run","query":"RETURN 1"})).await;
        rejected(&state, id, json!({"type":"bolt_reset"}), "pending").await;
        if remove {
            state.remove_client(id).await;
        } else {
            assert!(matches!(
                state
                    .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(2))
                    .await
                    .unwrap(),
                ClientSendOutcome::Disconnected
            ));
        }
        wait_closed(&peer).await;
        if !remove {
            state.remove_client(id).await;
        }
        peer.stop().await;
    }
}
#[tokio::test]
async fn endpoint_and_startup_bounds_refuse_before_connecting() {
    let state = state();
    for (address, params, needle) in [
        ("neo4j://127.0.0.1:1", json!({}), "endpoint"),
        ("bolt://user:password@127.0.0.1:1", json!({}), "endpoint"),
        ("bolt://127.0.0.1:1/path", json!({}), "endpoint"),
        (
            "bolt://127.0.0.1:1",
            json!({"request_timeout_secs":0}),
            "1..30",
        ),
        (
            "bolt://127.0.0.1:1",
            json!({"request_timeout_secs":31}),
            "1..30",
        ),
        (
            "bolt://127.0.0.1:1",
            json!({"password":"private"}),
            "username and password",
        ),
    ] {
        let error = refused_start(&state, address.into(), params).await;
        assert!(error.contains(needle));
        assert!(!error.contains("private"));
    }
}

#[tokio::test]
async fn startup_deadline_covers_selected_version_dribble_and_hello_without_registering_a_live_handle(
) {
    for mode in ["handshake-drip", "hello-hang"] {
        let peer = MockPeer::start(8, mode).await;
        let state = state();
        let error = refused_start(
            &state,
            peer.address.clone(),
            json!({"request_timeout_secs":1}),
        )
        .await;
        assert!(error.contains("deadline"), "{error}");
        wait_closed(&peer).await;
        peer.stop().await;
    }
    let peer = MockPeer::start(5, "normal").await;
    let state = state();
    let error = refused_start(&state, peer.address.clone(), json!({})).await;
    assert!(error.contains("unsupported negotiated version"));
    wait_closed(&peer).await;
    peer.stop().await;
}
#[tokio::test]
async fn stalled_manual_dispatch_has_a_bounded_event_queue_and_removal_cancels_both_tasks() {
    let peer = MockPeer::start(8, "normal").await;
    let state = state();
    let id = client(
        &state,
        peer.address.clone(),
        json!({"username":"neo4j","password":"private"}),
        vec![json!({"event_pattern":"*","handler":{"type":"manual"}})],
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await
        }
    })
    .await
    .unwrap();
    // Connected is parked in the dispatch task. Eight subsequent result events fit;
    // the ninth (RUN after four RUN/PULL pairs) closes on event-queue overflow.
    async fn idle(state: &netget::state::AppState, id: netget::state::ClientId) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match state
                    .send_to_client(id, json!({"type":"bolt_login"}), Duration::from_secs(2))
                    .await
                    .unwrap()
                {
                    ClientSendOutcome::Rejected { error }
                        if error.contains("authentication phase") =>
                    {
                        break
                    }
                    ClientSendOutcome::Rejected { error } if error.contains("pending") => {
                        tokio::time::sleep(Duration::from_millis(10)).await
                    }
                    other => panic!("idle phase probe: {other:?}"),
                }
            }
        })
        .await
        .unwrap();
    }
    for _ in 0..4 {
        send(&state, id, json!({"type":"bolt_run","query":"RETURN 1"})).await;
        idle(&state, id).await;
        send(&state, id, json!({"type":"bolt_pull","n":3})).await;
        idle(&state, id).await;
    }
    assert!(state.has_client_handle(id).await);
    send(&state, id, json!({"type":"bolt_run","query":"RETURN 1"})).await;
    failed(&state, id, "backend outcome unknown").await;
    wait_closed(&peer).await;
    state.remove_client(id).await;
    assert!(state.list_intercepts().await.is_empty());
    peer.stop().await;
}

#[tokio::test]
async fn native_cursor_summaries_cannot_claim_partial_pull_or_partial_discard_all() {
    for (mode, kind) in [
        ("short-more", "bolt_pull"),
        ("discard-more", "bolt_discard"),
    ] {
        let peer = MockPeer::start(8, mode).await;
        let state = state();
        let id = authenticated_mock(&state, &peer).await;
        receipt(
            &state,
            id,
            json!({"type":"bolt_run","query":"RETURN 1"}),
            "bolt_query_started",
        )
        .await;
        send(&state, id, json!({"type":kind,"n":1})).await;
        failed(&state, id, "backend outcome unknown").await;
        state.remove_client(id).await;
        peer.stop().await;
    }
}
