//! Zenoh from NetGet's side: action validation, NetGet's client against NetGet's router (a get
//! answered by the handler, an echo through a subscription), a handler that cannot answer
//! failing the query closed with a category rather than its error, and a bad startup key.
use crate::helpers::zenoh::*;
use netget::cli::management::ServerForm;
use netget::server::zenoh::node;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::json;
use std::time::Duration;

#[test]
fn validation() {
    for good in [
        json!({"type": "zenoh_put", "key": "a/b", "payload": "x"}),
        json!({"type": "zenoh_put", "key": "a/b", "payload": "00ff", "payload_encoding": "hex"}),
        json!({"type": "zenoh_get", "selector": "a/**?x=1"}),
        json!({"type": "zenoh_reply", "payload": "x"}),
        json!({"type": "zenoh_reply_error", "payload": "no"}),
    ] {
        node::validate(&good).unwrap_or_else(|e| panic!("{good}: {e}"));
    }
    for bad in [
        json!({"type": "zenoh_put", "key": "a//b", "payload": "x"}),
        json!({"type": "zenoh_put", "key": "a/b", "payload": "zz", "payload_encoding": "hex"}),
        json!({"type": "zenoh_get", "selector": "**/**/a?"}),
        json!({"type": "zenoh_delete"}),
        json!({"type": "zenoh_explode"}),
    ] {
        assert!(node::validate(&bad).is_err(), "{bad}");
    }
    assert_eq!(
        node::payload_from(&json!({"payload": "00ff", "payload_encoding": "hex"})).unwrap(),
        vec![0, 255]
    );
    assert!(node::keys(Some(&vec![json!("ok/**"), json!("bad//key")])).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_client_against_netget_router() {
    let state = state();
    let (sid, addr) = server_in(
        &state,
        json!({"subscribe": ["demo/**"], "queryable": ["demo/q/**"]}),
    )
    .await;
    let cid = client_in(
        &state,
        addr.to_string(),
        json!({"subscribe": ["demo/echo/out"]}),
    )
    .await
    .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    let send = |a: serde_json::Value| state.send_to_client(cid, a, Duration::from_secs(20));
    assert!(matches!(
        send(json!({"type": "zenoh_get", "selector": "demo/q/x"}))
            .await
            .unwrap(),
        ClientSendOutcome::Sent { .. }
    ));
    let r = wait_for(&state, owner, "zenoh_get_result", |_| true).await;
    assert_eq!(
        r["replies"],
        json!([{"key": "demo/q/x", "kind": "put", "payload": "answer for demo/q/x", "payload_encoding": "utf8", "encoding": "text/plain"}])
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        assert!(matches!(
            send(json!({"type": "zenoh_put", "key": "demo/echo/in", "payload": "ping"}))
                .await
                .unwrap(),
            ClientSendOutcome::Sent { .. }
        ));
        let got = state
            .list_access_logs_for(Some(owner), None)
            .await
            .into_iter()
            .any(|e| e.event_type == "zenoh_sample" && e.request["payload"] == "echo: ping");
        if got {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the echo never arrived"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(matches!(
        send(json!({"type": "zenoh_reply", "payload": "x"}))
            .await
            .unwrap(),
        ClientSendOutcome::Rejected { .. }
    ));
    state.remove_client(cid).await;
    state.remove_server(sid).await;

    // A handler whose answer is invalid: the query fails closed with a category.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let broken = ServerForm {
        protocol: "zenoh".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        instruction: Some(String::new()),
        event_handlers: Some(vec![json!({"event_pattern": "zenoh_query", "handler": {"type": "static", "actions": [{"type": "zenoh_reply", "key": "not//a key", "payload": "secret detail"}]}})]),
        startup_params: Some(json!({"queryable": ["demo/**"]})),
        ..Default::default()
    }
    .create(&state, tx.clone())
    .await
    .unwrap();
    let baddr = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(a) = state.get_server(broken).await.and_then(|s| s.local_addr) {
                break a;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let c2 = client_in(&state, baddr.to_string(), json!({}))
        .await
        .unwrap();
    assert!(matches!(
        state
            .send_to_client(
                c2,
                json!({"type": "zenoh_get", "selector": "demo/x"}),
                Duration::from_secs(20)
            )
            .await
            .unwrap(),
        ClientSendOutcome::Sent { .. }
    ));
    let r = wait_for(
        &state,
        AccessLogOwner::Client(c2.as_u32()),
        "zenoh_get_result",
        |_| true,
    )
    .await;
    let error = r["replies"][0]["error"]
        .as_str()
        .unwrap_or_else(|| panic!("{r}"));
    assert!(
        !error.contains("secret") && !error.contains("key"),
        "{error}"
    );
    state.remove_client(c2).await;
    state.remove_server(broken).await;

    let refused = ServerForm {
        protocol: "zenoh".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        instruction: Some(String::new()),
        startup_params: Some(json!({"subscribe": ["bad//key"]})),
        ..Default::default()
    }
    .create(&state, tx)
    .await;
    match refused {
        Err(e) => assert!(e.to_string().contains("key expression"), "{e}"),
        Ok(id) => {
            let status = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let Some(netget::state::server::ServerStatus::Error(e)) =
                        state.get_server(id).await.map(|s| s.status)
                    {
                        break e;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert!(status.contains("key expression"), "{status}");
        }
    }
}
