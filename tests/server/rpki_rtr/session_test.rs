//! NetGet's router against NetGet's cache, and both against hand-written misbehaving peers:
//! notify-driven incremental updates, Cache Reset recovery, version 0, version negotiation,
//! refusals with the right Error Report codes, and stopping with a query parked for a human.
use crate::helpers::rpki_rtr::*;
use netget::server::rpki_rtr::codec::{self, Packet, Pdu};
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner, ClientStatus};
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn read_pdu(stream: &mut TcpStream) -> Option<Packet> {
    let bytes = tokio::time::timeout(
        Duration::from_secs(10),
        codec::read_frame(stream, Duration::from_secs(10)),
    )
    .await
    .ok()?
    .ok()?;
    Some(Packet::decode(&bytes).unwrap())
}

async fn closed(stream: &mut TcpStream) -> bool {
    let mut b = [0u8; 1];
    matches!(
        tokio::time::timeout(Duration::from_secs(10), stream.read(&mut b)).await,
        Ok(Ok(0)) | Ok(Err(_))
    )
}

fn error_code(p: &Packet) -> u16 {
    match &p.pdu {
        Pdu::ErrorReport { code, .. } => *code,
        other => panic!("expected an Error Report, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn router_resyncs_on_notify_and_injected_serial_query() {
    let state = state();
    let (sid, addr) = server_in(
        &state,
        cache_policy(),
        json!({"session_id": SESSION, "refresh_interval_secs": 900, "expire_interval_secs": 3600}),
    )
    .await;
    let cid = client_in(&state, addr.to_string(), quiet_router(), json!({}))
        .await
        .unwrap();
    let router = AccessLogOwner::Client(cid.as_u32());
    let first = &logs(&state, router, "rpki_rtr_synchronized", 1).await[0].request;
    assert_eq!(first["kind"], "reset");
    assert_eq!(
        (first["serial"].as_u64(), first["session_id"].as_u64()),
        (Some(7), Some(u64::from(SESSION)))
    );
    assert_eq!(
        (first["announced"].as_u64(), first["withdrawn"].as_u64()),
        (Some(2), Some(0))
    );
    assert_eq!(
        first["intervals"]["refresh"], 900,
        "version-1 End of Data timers reach the router"
    );
    assert_eq!(first["intervals"]["expire"], 3600);
    let server = AccessLogOwner::Server(sid.as_u32());
    let conn = logs(&state, server, "rpki_rtr_reset_query", 1).await[0]
        .connection_id
        .unwrap();
    let sent = state
        .send_to_peer(
            sid,
            conn,
            json!({"type":"rpki_rtr_serial_notify","serial":8}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(matches!(sent, ClientSendOutcome::Sent { .. }));
    let second = &logs(&state, router, "rpki_rtr_synchronized", 2).await[1].request;
    assert_eq!(second["kind"], "incremental");
    assert_eq!(
        (
            second["serial"].as_u64(),
            second["announced"].as_u64(),
            second["withdrawn"].as_u64()
        ),
        (Some(8), Some(1), Some(1))
    );
    let withdrawn = second["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["announcement"] == false)
        .unwrap();
    assert_eq!(withdrawn["prefix"], "192.0.2.0/24");
    let poll = state
        .send_to_client(
            cid,
            json!({"type":"rpki_rtr_serial_query"}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(matches!(poll, ClientSendOutcome::Sent { .. }));
    let third = &logs(&state, router, "rpki_rtr_synchronized", 3).await[2].request;
    assert_eq!(
        (third["serial"].as_u64(), third["announced"].as_u64()),
        (Some(8), Some(0)),
        "nothing changed since 8"
    );
    state.remove_client(cid).await;
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cache_reset_sends_the_router_back_to_a_full_reset() {
    let state = state();
    let mut policy = cache_policy();
    policy[1] = json!({"event_pattern":"rpki_rtr_serial_query","handler":{"type":"static","actions":[{"type":"rpki_rtr_response","cache_reset":true}]}});
    let (sid, addr) = server_in(&state, policy, json!({"session_id": SESSION})).await;
    let cid = client_in(&state, addr.to_string(), quiet_router(), json!({}))
        .await
        .unwrap();
    let router = AccessLogOwner::Client(cid.as_u32());
    logs(&state, router, "rpki_rtr_synchronized", 1).await;
    state
        .send_to_client(
            cid,
            json!({"type":"rpki_rtr_serial_query"}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    let reset = logs(&state, router, "rpki_rtr_cache_reset", 1).await;
    assert_eq!(reset[0].request["previous_serial"], 7);
    let again = &logs(&state, router, "rpki_rtr_synchronized", 2).await[1].request;
    assert_eq!(again["kind"], "reset");
    assert_eq!(
        logs(
            &state,
            AccessLogOwner::Server(sid.as_u32()),
            "rpki_rtr_reset_query",
            2
        )
        .await
        .len(),
        2
    );
    state.remove_client(cid).await;
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn version_0_router_and_cache_agree_without_timers() {
    let state = state();
    let (sid, addr) = server_in(&state, cache_policy(), json!({"session_id": SESSION})).await;
    let cid = client_in(
        &state,
        addr.to_string(),
        quiet_router(),
        json!({"version": 0, "refresh_interval_secs": 120}),
    )
    .await
    .unwrap();
    let sync = &logs(
        &state,
        AccessLogOwner::Client(cid.as_u32()),
        "rpki_rtr_synchronized",
        1,
    )
    .await[0]
        .request;
    assert_eq!(sync["serial"], 7);
    assert_eq!(
        sync["intervals"]["refresh"], 120,
        "version 0 carries no timers; the router's own refresh stands"
    );
    let query = &logs(
        &state,
        AccessLogOwner::Server(sid.as_u32()),
        "rpki_rtr_reset_query",
        1,
    )
    .await[0]
        .request;
    assert_eq!(query["version"], 0);
    state.remove_client(cid).await;
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_cache_refuses_with_the_rfc_error_code() {
    let state = state();
    let (sid, addr) = server_in(&state, cache_policy(), json!({"session_id": SESSION})).await;
    // (first PDU bytes, expected code)
    for (bytes, code) in [
        (vec![2u8, 2, 0, 0, 0, 0, 0, 8], 4u16), // version 2: negotiate down
        (vec![1, 11, 0, 0, 0, 0, 0, 8], 5),     // unknown PDU type
        (
            Packet {
                version: 1,
                pdu: Pdu::CacheReset,
            }
            .encode()
            .unwrap(),
            3,
        ), // a cache PDU from a router
        (vec![1, 2, 0, 0, 0, 0, 0x13, 0x88], 0), // 5000-byte length
        (vec![1, 2, 0, 0, 0, 0, 0, 9, 0], 0),   // reset query with a body
    ] {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(&bytes).await.unwrap();
        let reply = read_pdu(&mut s)
            .await
            .unwrap_or_else(|| panic!("no Error Report for {bytes:?}"));
        assert_eq!(error_code(&reply), code, "{bytes:?}");
        if code == 4 {
            assert_eq!(
                reply.version, 1,
                "Unsupported Protocol Version is sent in the cache's highest version"
            );
        }
        assert!(closed(&mut s).await, "{bytes:?} left the session open");
    }
    // Version change within a session.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        &Packet {
            version: 1,
            pdu: Pdu::ResetQuery,
        }
        .encode()
        .unwrap(),
    )
    .await
    .unwrap();
    while !matches!(read_pdu(&mut s).await.unwrap().pdu, Pdu::EndOfData { .. }) {}
    s.write_all(
        &Packet {
            version: 0,
            pdu: Pdu::ResetQuery,
        }
        .encode()
        .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(error_code(&read_pdu(&mut s).await.unwrap()), 8);
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn handler_failures_answer_internal_error_and_no_data_keeps_the_session() {
    let state = state();
    let policy = vec![
        json!({"event_pattern":"rpki_rtr_reset_query","handler":{"type":"static","actions":[{"type":"rpki_rtr_response","no_data":true}]}}),
        // An answer older than the router's serial.
        json!({"event_pattern":"rpki_rtr_serial_query","handler":{"type":"static","actions":[{"type":"rpki_rtr_response","serial":5,"records":[]}]}}),
    ];
    let (sid, addr) = server_in(&state, policy, json!({"session_id": SESSION})).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    for _ in 0..2 {
        s.write_all(
            &Packet {
                version: 1,
                pdu: Pdu::ResetQuery,
            }
            .encode()
            .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            error_code(&read_pdu(&mut s).await.unwrap()),
            2,
            "No Data Available, and the session stays open for a retry"
        );
    }
    s.write_all(
        &Packet {
            version: 1,
            pdu: Pdu::SerialQuery {
                session: SESSION,
                serial: 7,
            },
        }
        .encode()
        .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        error_code(&read_pdu(&mut s).await.unwrap()),
        1,
        "a serial going backwards is the cache's own failure"
    );
    assert!(closed(&mut s).await);
    state.remove_server(sid).await;

    // No handler and an unreachable model: Internal Error, never data.
    let state = crate::helpers::rpki_rtr::state();
    let (sid, addr) = server_in(&state, vec![], json!({})).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        &Packet {
            version: 1,
            pdu: Pdu::ResetQuery,
        }
        .encode()
        .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(error_code(&read_pdu(&mut s).await.unwrap()), 1);
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_cache_ends_a_router_parked_on_a_manual_answer() {
    let state = state();
    let policy = vec![
        json!({"event_pattern":"rpki_rtr_reset_query","handler":{"type":"manual","timeout_secs":300}}),
    ];
    let (sid, addr) = server_in(&state, policy, json!({})).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        &Packet {
            version: 1,
            pdu: Pdu::ResetQuery,
        }
        .encode()
        .unwrap(),
    )
    .await
    .unwrap();
    assert!(
        wait_until(Duration::from_secs(20), || async {
            !state.list_intercepts().await.is_empty()
        })
        .await
    );
    state.remove_server(sid).await;
    assert!(
        closed(&mut s).await,
        "the router's socket outlived the stopped cache"
    );
    assert!(
        wait_until(Duration::from_secs(5), || async {
            state.list_intercepts().await.is_empty()
        })
        .await
    );
}

/// A fake cache: accept, swallow the Reset Query, then send `pdus`; return what the router sends back.
async fn misbehaving_cache(
    pdus: Vec<Vec<u8>>,
    params: serde_json::Value,
) -> (Option<Packet>, ClientStatus) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = state();
    let accept = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let query = read_pdu(&mut s).await.unwrap();
        assert!(matches!(query.pdu, Pdu::ResetQuery));
        for p in pdus {
            s.write_all(&p).await.unwrap();
        }
        read_pdu(&mut s).await
    });
    // The router may refuse and close before its handle is observable, so only create it.
    let cid = client_create(&state, addr.to_string(), quiet_router(), params)
        .await
        .unwrap();
    let reply = accept.await.unwrap();
    assert!(
        wait_until(Duration::from_secs(10), || async {
            state
                .get_client(cid)
                .await
                .is_none_or(|c| c.status != ClientStatus::Connected)
        })
        .await
    );
    let status = state
        .get_client(cid)
        .await
        .map(|c| c.status)
        .unwrap_or(ClientStatus::Disconnected);
    (reply, status)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_router_refuses_out_of_order_and_contradictory_cache_pdus() {
    let prefix = |announce: bool| {
        Packet {
            version: 1,
            pdu: Pdu::Prefix(codec::Record {
                prefix: "192.0.2.0/24".into(),
                max_length: 24,
                asn: 1,
                announcement: announce,
            }),
        }
        .encode()
        .unwrap()
    };
    let response = Packet {
        version: 1,
        pdu: Pdu::CacheResponse { session: 1 },
    }
    .encode()
    .unwrap();
    for (pdus, code) in [
        (vec![prefix(true)], 0u16),                 // prefix before Cache Response
        (vec![response.clone(), prefix(false)], 6), // withdrawal during a reset
        (
            vec![Packet {
                version: 0,
                pdu: Pdu::CacheResponse { session: 1 },
            }
            .encode()
            .unwrap()],
            8,
        ), // version changed
        (
            vec![Packet {
                version: 1,
                pdu: Pdu::ResetQuery,
            }
            .encode()
            .unwrap()],
            3,
        ), // a query from the cache
        (
            vec![
                response.clone(),
                Packet {
                    version: 1,
                    pdu: Pdu::EndOfData {
                        session: 2,
                        serial: 1,
                        intervals: codec::Intervals::default(),
                    },
                }
                .encode()
                .unwrap(),
            ],
            0,
        ),
    ] {
        let (reply, status) = misbehaving_cache(pdus.clone(), json!({})).await;
        let reply = reply.unwrap_or_else(|| panic!("router sent no Error Report for {pdus:?}"));
        assert_eq!(error_code(&reply), code, "{pdus:?}");
        assert!(matches!(status, ClientStatus::Error(_)), "{status:?}");
    }
}
