//! Independent authenticated aioquic server: semantic requests, bounds and lifecycle.
use crate::helpers::quic_peer::*;
use netget::state::client_handles::ClientSendOutcome;
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn http3_client_get_post_headers_trailers_and_multiplexing() {
    let mut peer = Peer::start("http3").await;
    let state = state();
    let mut params = peer.cert.trust();
    params["default_headers"] = json!({"x-default":"startup","x-replace":"old","te":"trailers"});
    let id = client(
        &state,
        "http3",
        peer.address(),
        params,
        vec![empty_handler()],
    )
    .await;
    let local = state
        .get_client(id)
        .await
        .unwrap()
        .connection
        .unwrap()
        .local_addr
        .unwrap();
    assert_ne!(local.port(), peer.port);
    assert_ne!(local.port(), 0);
    let requests=(0..8).map(|n| { let state=state.clone(); async move {
        state.send_to_client(id,json!({"type":"send_http3_request","method":"POST","path":format!("/echo/{n}"),"headers":{"x-replace":"new"},"body":"hello","priority":1,"trailers":{"x-request-tail":"yes"}}),Duration::from_secs(8)).await.unwrap()
    } });
    let results = futures::future::join_all(requests).await;
    assert!(results
        .iter()
        .all(|r| matches!(r,ClientSendOutcome::Executed{detail} if detail.contains("-> 200"))));
    wait_log(&state, id, "aioquic").await;
    let logs = state
        .list_access_logs_for(
            Some(netget::state::AccessLogOwner::Client(id.as_u32())),
            None,
        )
        .await;
    let logs = format!("{logs:?}");
    for expected in ["startup", "new", "u=1", "hello", "x-request-tail"] {
        assert!(logs.contains(expected), "missing {expected}");
    }
    state.remove_client(id).await;
    peer.close().await;
}

#[tokio::test]
async fn http3_client_enforces_headers_body_and_priority_bounds() {
    let mut peer = Peer::start("http3").await;
    let state = state();
    let id = client(
        &state,
        "http3",
        peer.address(),
        peer.cert.trust(),
        vec![empty_handler()],
    )
    .await;
    for action in [
        json!({"type":"send_http3_request","method":"GET","path":"/","headers":{"x-large":"x".repeat(32769)}}),
        // The regular fields fit exactly; request pseudo-fields push them over.
        json!({"type":"send_http3_request","method":"GET","path":"/","headers":{"x-large":"x".repeat(32768-39)}}),
        json!({"type":"send_http3_request","method":"POST","path":"/","body":"x".repeat(8*1024*1024+1)}),
        json!({"type":"send_http3_request","method":"GET","path":"https://elsewhere.invalid/"}),
        json!({"type":"send_http3_request","method":"GET","path":"/","headers":{"te":"gzip"}}),
        json!({"type":"send_http3_request","method":"GET","path":"/","trailers":{"te":"trailers"}}),
    ] {
        assert!(state
            .send_to_client(id, action, Duration::from_secs(5))
            .await
            .is_err());
    }
    assert!(matches!(
        state
            .send_to_client(
                id,
                json!({"type":"send_http3_request","method":"GET","path":"/","priority":256}),
                Duration::from_secs(5)
            )
            .await
            .unwrap(),
        ClientSendOutcome::Rejected { .. }
    ));
    for (path, expected) in [
        ("/oversized", "response body exceeds"),
        ("/large-header", "Header too big"),
        ("/te-response", "HTTP3 TE"),
        ("/te-trailer", "HTTP3 TE"),
    ] {
        let error = state
            .send_to_client(
                id,
                json!({"type":"send_http3_request","method":"GET","path":path}),
                Duration::from_secs(8),
            )
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{path}: {error:#}");
    }
    state.remove_client(id).await;
    peer.close().await;
}

#[tokio::test]
async fn http3_timeout_disconnect_and_removal_cancel_pending_reads() {
    let mut peer = Peer::start("http3").await;
    let state = state();
    let mut params = peer.cert.trust();
    params["exchange_timeout_secs"] = json!(1);
    let id = client(
        &state,
        "http3",
        peer.address(),
        params,
        vec![empty_handler()],
    )
    .await;
    assert!(state
        .send_to_client(
            id,
            json!({"type":"send_http3_request","method":"GET","path":"/silent"}),
            Duration::from_secs(4)
        )
        .await
        .is_err());
    let outcome = state
        .send_to_client(
            id,
            json!({"type":"send_http3_request","method":"GET","path":"/"}),
            Duration::from_secs(4),
        )
        .await
        .unwrap();
    assert!(
        matches!(outcome, ClientSendOutcome::Executed { .. }),
        "timeout must preserve connection"
    );
    let send_state = state.clone();
    let pending = tokio::spawn(async move {
        send_state
            .send_to_client(
                id,
                json!({"type":"send_http3_request","method":"GET","path":"/silent"}),
                Duration::from_secs(8),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let result = state
        .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(1))
        .await
        .unwrap();
    assert!(matches!(result, ClientSendOutcome::Disconnected));
    assert!(tokio::time::timeout(Duration::from_secs(1), pending)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    let id = client(
        &state,
        "http3",
        peer.address(),
        peer.cert.trust(),
        vec![empty_handler()],
    )
    .await;
    let send_state = state.clone();
    let pending = tokio::spawn(async move {
        send_state
            .send_to_client(
                id,
                json!({"type":"send_http3_request","method":"GET","path":"/silent"}),
                Duration::from_secs(8),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    state.remove_client(id).await;
    assert!(tokio::time::timeout(Duration::from_secs(1), pending)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    peer.close().await;
}

#[tokio::test]
async fn http3_client_rejects_untrusted_and_wrong_name_certificates() {
    let mut peer = Peer::start("http3").await;
    for params in [
        json!({}),
        json!({"ca_cert_path":peer.cert.cert(),"server_name":"wrong.invalid"}),
    ] {
        let params =
            netget::protocol::StartupParams::new(params, netget::utils::quic::client_parameters())
                .unwrap();
        assert!(
            netget::utils::quic::connect(&peer.address(), Some(&params), b"h3", 4)
                .await
                .is_err()
        );
    }
    peer.close().await;
}

#[tokio::test]
async fn http3_parked_handlers_reserve_capacity_and_disconnect_stays_responsive() {
    let mut peer = Peer::start("http3").await;
    let state = state();
    let id = client(
        &state,
        "http3",
        peer.address(),
        peer.cert.trust(),
        vec![json!({"event_pattern":"*","handler":{"type":"manual","timeout_secs":300}})],
    )
    .await;
    // The connected event occupies one slot. Every accepted exchange reserves
    // another until its response handler completes, even if the human parks it.
    for expected in 1..=32 {
        tokio::time::timeout(Duration::from_secs(3), async {
            while state.list_intercepts().await.len() != expected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("every response must reach its handler");
        if expected < 32 {
            assert!(matches!(
                state
                    .send_to_client(
                        id,
                        json!({"type":"send_http3_request","method":"GET","path":"/"}),
                        Duration::from_secs(3)
                    )
                    .await
                    .unwrap(),
                ClientSendOutcome::Executed { .. }
            ));
        }
    }
    let error = state
        .send_to_client(
            id,
            json!({"type":"send_http3_request","method":"GET","path":"/"}),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("32 active exchanges or handlers"));
    assert!(matches!(
        state
            .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(1))
            .await
            .unwrap(),
        ClientSendOutcome::Disconnected
    ));
    tokio::time::timeout(Duration::from_secs(1), async {
        while !state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    state.remove_client(id).await;
    peer.close().await;
}
