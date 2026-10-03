use super::common::*;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
#[tokio::test]
async fn startup_public_probe_is_not_authentication_and_health_codes_keep_their_meaning() {
    let peer = mock_peer("normal").await;
    let state = state();
    let id = client(
        &state,
        peer.address.clone(),
        json!({"token":TOKEN}),
        vec![static_handler("*", json!([]))],
    )
    .await;
    let (_, connected) = event(&state, id, "vault_connected", 0).await;
    assert_eq!(connected["token_present"], true);
    assert_eq!(connected["authentication_verified"], false);
    assert!(
        peer.seen()[0].token.is_none(),
        "public startup must not send configured token"
    );
    for code in [200, 429, 472, 473, 474, 501, 503, 530] {
        peer.mode(&format!("health_{code}"));
        let response = request(&state, id, json!({"operation":"health"})).await;
        assert_eq!(response["status"], code);
        assert_eq!(response["data"]["initialized"], true);
    }
    assert!(peer
        .seen()
        .iter()
        .skip(1)
        .all(|r| r.token.as_deref() == Some(TOKEN)));
    state.remove_client(id).await;
    peer.stop().await;
}
#[tokio::test]
async fn login_installs_one_validated_token_and_redacts_later_reflections_and_access_logs() {
    let peer = mock_peer("reflect").await;
    let state = state();
    let id = connected_client(&state, peer.address.clone()).await;
    let after = latest(&state, id).await;
    send(
        &state,
        id,
        json!({"type":"vault_userpass_login","username":"reader","password":PASSWORD}),
    )
    .await;
    let (_, auth) = event(&state, id, "vault_authentication", after).await;
    assert_eq!(auth["authentication_verified"], true);
    assert_eq!(auth["token_present"], true);
    assert_eq!(auth["auth"]["token_type"], "service");
    assert_eq!(auth["auth"]["lease_duration"], 3600);
    assert!(auth["auth"].get("client_token").is_none());
    let read = request(&state, id, json!({"operation":"read","path":"fixture/app"})).await;
    assert_eq!(read["data"]["data"]["data"]["answer"], 42);
    assert_eq!(read["data"]["data"]["metadata"]["version"], 2);
    let seen = peer.seen();
    let login = seen
        .iter()
        .find(|r| r.path.starts_with("/v1/auth/"))
        .unwrap();
    assert_eq!(login.method, "POST");
    assert!(login.token.is_none());
    assert_eq!(login.body["password"], PASSWORD);
    assert_eq!(seen.last().unwrap().token.as_deref(), Some(TOKEN));
    let logs = state
        .list_access_logs_for(
            Some(netget::state::AccessLogOwner::Client(id.as_u32())),
            None,
        )
        .await;
    let shown = serde_json::to_string(&logs).unwrap();
    assert!(!shown.contains(PASSWORD));
    assert!(!shown.contains(TOKEN));
    let after = latest(&state, id).await;
    send(&state, id, json!({"type":"vault_clear_token"})).await;
    let (_, clear) = event(&state, id, "vault_authentication", after).await;
    assert_eq!(clear["status"], json!(null));
    assert_eq!(clear["token_present"], false);
    assert_eq!(clear["authentication_verified"], false);
    let after = latest(&state, id).await;
    send(&state, id, json!({"operation":"read","path":"fixture/app"})).await;
    assert_eq!(
        event(&state, id, "vault_request_error", after).await.1["status"],
        403
    );
    assert!(peer.seen().last().unwrap().token.is_none());
    state.remove_client(id).await;
    peer.stop().await;
}
#[tokio::test]
async fn failed_or_incomplete_login_discards_previous_credential_without_partial_success() {
    for mode in ["bad_login", "mfa"] {
        let peer = mock_peer(mode).await;
        let state = state();
        let id = client(
            &state,
            peer.address.clone(),
            json!({"token":TOKEN}),
            vec![static_handler("*", json!([]))],
        )
        .await;
        event(&state, id, "vault_connected", 0).await;
        let after = latest(&state, id).await;
        send(
            &state,
            id,
            json!({"type":"vault_userpass_login","username":"reader","password":PASSWORD}),
        )
        .await;
        let (_, error) = event(&state, id, "vault_request_error", after).await;
        assert_eq!(error["token_present"], false);
        assert_eq!(
            error["category"],
            if mode == "mfa" { "schema" } else { "http" }
        );
        assert!(!error.to_string().contains(PASSWORD));
        assert!(!error.to_string().contains(TOKEN));
        let after = latest(&state, id).await;
        send(&state, id, json!({"operation":"read","path":"fixture/app"})).await;
        assert_eq!(
            event(&state, id, "vault_request_error", after).await.1["status"],
            403
        );
        assert!(peer.seen().iter().all(|r| r.token.is_none()));
        state.remove_client(id).await;
        peer.stop().await;
    }
}
#[tokio::test]
async fn schema_encoding_body_and_redirect_refusals_leave_session_available() {
    let peer = mock_peer("normal").await;
    let state = state();
    let id = connected_client(&state, peer.address.clone()).await;
    for mode in [
        "malformed",
        "content_type",
        "compressed",
        "redirect",
        "large",
    ] {
        peer.mode(mode);
        let after = latest(&state, id).await;
        send(&state, id, json!({"operation":"health"})).await;
        let (_, error) = event(&state, id, "vault_request_error", after).await;
        assert_eq!(
            error["category"],
            if mode == "redirect" {
                "http"
            } else if mode == "large" {
                "transport"
            } else {
                "schema"
            }
        );
        assert!(!peer
            .seen()
            .iter()
            .any(|r| r.path.contains("should-not-follow")));
        peer.mode("normal");
        assert_eq!(
            request(&state, id, json!({"operation":"health"})).await["status"],
            200
        );
    }
    for (mode, errors) in [
        ("http_multi", json!(["first", "second"])),
        ("http_empty", json!([])),
    ] {
        peer.mode(mode);
        let after = latest(&state, id).await;
        send(&state, id, json!({"operation":"health"})).await;
        assert_eq!(
            event(&state, id, "vault_request_error", after).await.1["errors"],
            errors
        );
    }
    rejected(
        &state,
        id,
        json!({"type":"vault_request","operation":"health","path":"/sys/mounts"}),
        "invalid selected",
    )
    .await;
    state.remove_client(id).await;
    peer.stop().await;
}
#[tokio::test]
async fn whole_request_deadline_covers_silent_head_and_partial_body_then_recovers() {
    for mode in ["stall", "stall_body"] {
        let peer = mock_peer(mode).await;
        let state = state();
        let id = client(
            &state,
            peer.address.clone(),
            json!({"request_timeout_secs":1}),
            vec![static_handler("*", json!([]))],
        )
        .await;
        event(&state, id, "vault_connected", 0).await;
        let after = latest(&state, id).await;
        send(&state, id, json!({"operation":"health"})).await;
        assert_eq!(
            event(&state, id, "vault_request_error", after).await.1["category"],
            "transport"
        );
        peer.mode("normal");
        assert_eq!(
            request(&state, id, json!({"operation":"health"})).await["status"],
            200
        );
        state.remove_client(id).await;
        peer.stop().await;
    }
}
#[tokio::test]
async fn pending_io_and_manual_event_are_cancelled_by_owned_disconnect_and_removal() {
    for remove in [false, true] {
        let peer = mock_peer("stall").await;
        let state = state();
        let id = client(
            &state,
            peer.address.clone(),
            json!({}),
            vec![json!({"event_pattern":"*","handler":{"type":"manual"}})],
        )
        .await;
        send(&state, id, json!({"operation":"health"})).await;
        peer.wait_for("/v1/sys/health").await;
        rejected(
            &state,
            id,
            json!({"type":"vault_request","operation":"leader"}),
            "pending",
        )
        .await;
        let start = Instant::now();
        if remove {
            state.remove_client(id).await;
        } else {
            let outcome = state
                .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(2))
                .await
                .unwrap();
            assert!(matches!(
                outcome,
                netget::state::client_handles::ClientSendOutcome::Disconnected
            ));
            tokio::time::timeout(Duration::from_secs(2), async {
                while state.has_client_handle(id).await {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            state.remove_client(id).await;
        }
        assert!(start.elapsed() < Duration::from_secs(2));
        peer.stop().await;
    }
}
#[tokio::test]
async fn repeated_handler_followups_are_bounded_and_fresh_injection_resets_depth() {
    let peer = mock_peer("normal").await;
    let state = state();
    let id = client(
        &state,
        peer.address.clone(),
        json!({}),
        vec![static_handler(
            "*",
            json!([{"type":"vault_request","operation":"health"}]),
        )],
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while state
            .list_access_logs_for(
                Some(netget::state::AccessLogOwner::Client(id.as_u32())),
                None,
            )
            .await
            .iter()
            .filter(|e| e.event_type == "vault_response")
            .count()
            < 4
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        request(&state, id, json!({"operation":"leader"})).await["operation"],
        "leader"
    );
    state.remove_client(id).await;
    peer.stop().await;
}
#[tokio::test]
async fn endpoint_and_startup_parameters_refuse_raw_paths_credentials_or_unsafe_limits() {
    let state = state();
    for address in [
        "ftp://127.0.0.1:1",
        "http://user:password@127.0.0.1:1",
        "http://127.0.0.1:1/v1/sys/health",
        "http://127.0.0.1:1?token=x",
    ] {
        refused_start(&state, address.into(), json!({})).await;
    }
    for params in [
        json!({"token":"bad token"}),
        json!({"kv_mount":"sys"}),
        json!({"auth_mount":"../auth"}),
        json!({"request_timeout_secs":0}),
        json!({"request_timeout_secs":31}),
    ] {
        refused_start(&state, "127.0.0.1:1".into(), params).await;
    }
}

#[tokio::test]
async fn deeply_constructed_injected_values_are_rejected_without_copying_or_wire_io() {
    fn deep(disconnect: bool) -> Value {
        let mut leaf = Value::String(TOKEN.into());
        for _ in 0..10000 {
            leaf = Value::Array(vec![leaf]);
        }
        let mut action = if disconnect {
            json!({"type":"disconnect"})
        } else {
            json!({"type":"vault_request","operation":"write","path":"fixture/app","data":{}})
        };
        action["nested"] = leaf;
        action
    }
    let peer = mock_peer("normal").await;
    let state = state();
    let id = client(
        &state,
        peer.address.clone(),
        json!({"request_timeout_secs":1}),
        vec![static_handler("*", json!([]))],
    )
    .await;
    event(&state, id, "vault_connected", 0).await;
    let before = peer.seen().len();
    for disconnect in [false, true] {
        rejected(&state, id, deep(disconnect), "depth/node/retained-content").await;
    }
    assert_eq!(peer.seen().len(), before);
    peer.mode("stall_body");
    let after = latest(&state, id).await;
    send(&state, id, json!({"operation":"health"})).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while peer.seen().len() == before {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    for disconnect in [false, true] {
        rejected(&state, id, deep(disconnect), "depth/node/retained-content").await;
    }
    assert_eq!(peer.seen().len(), before + 1);
    assert_eq!(
        event(&state, id, "vault_request_error", after).await.1["category"],
        "transport"
    );
    peer.mode("normal");
    assert_eq!(
        request(&state, id, json!({"operation":"health"})).await["status"],
        200
    );
    state.remove_client(id).await;
    peer.stop().await;
}

#[tokio::test]
async fn public_probe_reflections_are_redacted_in_success_and_schema_failure() {
    let peer = mock_peer("probe_reflect").await;
    let state = state();
    let id = client(
        &state,
        peer.address.clone(),
        json!({"token":TOKEN}),
        vec![static_handler("*", json!([]))],
    )
    .await;
    let (_, connected) = event(&state, id, "vault_connected", 0).await;
    assert_eq!(connected["seal"]["version"], "<redacted>");
    state.remove_client(id).await;
    peer.stop().await;
    let peer = mock_peer("probe_invalid").await;
    let error = refused_start(&state, peer.address.clone(), json!({"token":TOKEN})).await;
    assert!(!error.contains(TOKEN));
    peer.stop().await;
}
