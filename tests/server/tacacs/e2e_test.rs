use crate::helpers::tacacs::{
    accounting, authentication, authorization, client, header, intercept, logs, policies, read,
    reply, server,
};
use netget::{
    server::tacacs::codec::*,
    state::{client_handles::ClientSendOutcome, AccessLogOwner},
};
use serde_json::json;
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
#[tokio::test]
async fn native_pair_policies_and_accounting_record_before_success() {
    let (server, sid, addr, _) = server(Some(policies()), None).await;
    let (client, cid) = client(addr.to_string(), None, None).await;
    for action in [
        authentication("ascii"),
        authentication("pap"),
        authorization(),
        accounting("stop"),
    ] {
        assert!(matches!(
            client
                .send_to_client(cid, action, Duration::from_secs(10))
                .await
                .unwrap(),
            ClientSendOutcome::Executed { .. }
        ));
    }
    let rows = logs(
        &client,
        AccessLogOwner::Client(cid.as_u32()),
        "tacacs_authentication_result",
        2,
    )
    .await;
    assert!(rows.iter().all(|r| r.request["authenticated"] == true));
    let rows = server.list_access_logs(None).await;
    let request = rows
        .iter()
        .find(|e| e.event_type == "tacacs_accounting")
        .unwrap();
    let recorded = rows
        .iter()
        .find(|e| e.event_type == "tacacs_accounting_recorded")
        .unwrap();
    assert!(recorded.id > request.id);
    assert_eq!(recorded.response[0]["recorded_in_shared_access_log"], true);
    assert_eq!(recorded.response[0]["durable_storage"], false);
    let result = logs(
        &client,
        AccessLogOwner::Client(cid.as_u32()),
        "tacacs_accounting_result",
        1,
    )
    .await;
    assert_eq!(result[0].request["recorded_by_peer"], true);
    client.remove_client(cid).await;
    server.remove_server(sid).await;
}
#[tokio::test]
async fn default_denies_authentication_authorization_and_accounting_without_model() {
    let (state, id, addr, _) = server(None, None).await;
    let (cstate, cid) = client(addr.to_string(), None, None).await;
    for action in [authentication("pap"), authorization(), accounting("start")] {
        cstate
            .send_to_client(cid, action, Duration::from_secs(10))
            .await
            .unwrap();
    }
    for (event, status) in [
        ("tacacs_authentication_result", "fail"),
        ("tacacs_authorization_result", "fail"),
        ("tacacs_accounting_result", "error"),
    ] {
        assert_eq!(
            logs(&cstate, AccessLogOwner::Client(cid.as_u32()), event, 1).await[0].request["reply"]
                ["status"],
            status
        );
    }
    assert!(!state
        .list_access_logs(None)
        .await
        .iter()
        .any(|r| r.event_type == "tacacs_accounting_recorded"));
    cstate.remove_client(cid).await;
    state.remove_server(id).await;
}
#[tokio::test]
async fn failed_actions_alongside_success_or_duplicate_reply_never_acknowledge() {
    for actions in [
        vec![
            json!({"type":"record_tacacs_accounting","reply":{"status":"success"}}),
            json!({"type":"respond_tacacs_authentication","reply":{"status":"get_pass"}}),
        ],
        vec![json!({"type":"record_tacacs_accounting","reply":{"status":"success"}}); 2],
        vec![json!({"type":"respond_tacacs_authentication","reply":{"status":"pass"}})],
    ] {
        let (state,id,addr,_)=server(Some(vec![json!({"event_pattern":"tacacs_accounting","handler":{"type":"static","actions":actions}})]),None).await;
        let mut peer = TcpStream::connect(addr).await.unwrap();
        let request: Request = serde_json::from_value(json!({"username":"alice"})).unwrap();
        reply(
            &mut peer,
            header(3, 0xc0),
            &request_body(&request, Some(AccountKind::Start)).unwrap(),
        )
        .await;
        let (h, body) = read(&mut peer).await;
        assert_eq!(h.sequence, 2);
        assert_eq!(
            parse_account_reply(&body).unwrap().status,
            AccountStatus::Error
        );
        assert!(!state
            .list_access_logs(None)
            .await
            .iter()
            .any(|r| r.event_type == "tacacs_accounting_recorded"));
        state.remove_server(id).await;
    }
}
#[tokio::test]
async fn malformed_header_type_and_body_have_bounded_fail_closed_replies() {
    let (state, id, addr, _) = server(None, None).await;
    for (kind, version, sequence, flags, body) in [
        (9, 0xc0, 1, 0x80, vec![]),
        (3, 0xc1, 1, 0, vec![]),
        (1, 0xc0, 2, 0, vec![]),
        (2, 0xc0, 1, 0, vec![0; 7]),
    ] {
        let mut peer = TcpStream::connect(addr).await.unwrap();
        let mut h = header(kind, version);
        h.sequence = sequence;
        h.flags = flags;
        reply(&mut peer, h, &body).await;
        let (got, body) = read(&mut peer).await;
        assert_eq!(got.sequence, sequence + 1);
        assert_eq!(got.session_id, h.session_id);
        if kind == 9 {
            assert_eq!(got.flags, flags);
            assert!(body.is_empty());
        } else {
            assert!(!body.is_empty());
        }
    }
    assert!(state.list_access_logs(None).await.is_empty());
    state.remove_server(id).await;
}
#[tokio::test]
async fn ascii_username_prompt_single_connection_decline_and_abort_are_native() {
    let (state, id, addr, _) = server(Some(policies()), None).await;
    let mut peer = TcpStream::connect(addr).await.unwrap();
    let mut h = header(1, 0xc0);
    h.flags = 4;
    // Literal START LOGIN privilege1 ASCII/login, empty username, no context/data.
    reply(&mut peer, h, &[1, 1, 1, 1, 0, 0, 0, 0]).await;
    let (got, body) = read(&mut peer).await;
    assert_eq!(got.flags & 4, 0);
    assert_eq!(parse_auth_reply(&body).unwrap().status, AuthStatus::GetUser);
    h.sequence = 3;
    h.flags = 0;
    reply(&mut peer, h, &continue_body("alice", false).unwrap()).await;
    let (got, body) = read(&mut peer).await;
    assert_eq!(got.sequence, 4);
    assert_eq!(parse_auth_reply(&body).unwrap().status, AuthStatus::GetPass);
    assert!(parse_auth_reply(&body).unwrap().no_echo);
    h.sequence = 5;
    reply(&mut peer, h, &continue_body("correct", false).unwrap()).await;
    let (got, body) = read(&mut peer).await;
    assert_eq!(got.sequence, 6);
    assert_eq!(parse_auth_reply(&body).unwrap().status, AuthStatus::Pass);
    assert_eq!(peer.read(&mut [0]).await.unwrap(), 0);
    let mut peer = TcpStream::connect(addr).await.unwrap();
    let mut h = header(1, 0xc0);
    reply(&mut peer, h, &[1, 1, 1, 1, 0, 0, 0, 0]).await;
    read(&mut peer).await;
    h.sequence = 3;
    reply(&mut peer, h, &continue_body("cancelled", true).unwrap()).await;
    assert_eq!(peer.read(&mut [0]).await.unwrap(), 0);
    assert_eq!(
        logs(
            &state,
            AccessLogOwner::Server(id.as_u32()),
            "tacacs_authentication",
            1
        )
        .await
        .len(),
        1
    );
    state.remove_server(id).await;
}
#[tokio::test]
async fn malformed_continue_correlations_and_unsupported_authentication_deny() {
    let (state, id, addr, _) = server(None, None).await;
    for sequence in [2, 5] {
        let mut peer = TcpStream::connect(addr).await.unwrap();
        let mut h = header(1, 0xc0);
        reply(&mut peer, h, &[1, 1, 1, 1, 0, 0, 0, 0]).await;
        read(&mut peer).await;
        h.sequence = sequence;
        reply(&mut peer, h, &continue_body("alice", false).unwrap()).await;
        let (_, body) = read(&mut peer).await;
        assert_eq!(parse_auth_reply(&body).unwrap().status, AuthStatus::Error);
    }
    for (version, body, status) in [
        (0xc1, vec![1, 1, 3, 1, 1, 0, 0, 0, b'a'], AuthStatus::Fail),
        (0xc0, vec![1, 1, 1, 2, 1, 0, 0, 0, b'a'], AuthStatus::Fail),
        (0xc0, vec![99, 1, 1, 1, 0, 0, 0, 0], AuthStatus::Error),
        (
            0xc0,
            vec![1, 1, 2, 1, 1, 0, 0, 1, b'a', b'b'],
            AuthStatus::Error,
        ),
    ] {
        let mut peer = TcpStream::connect(addr).await.unwrap();
        reply(&mut peer, header(1, version), &body).await;
        let (_, body) = read(&mut peer).await;
        assert_eq!(parse_auth_reply(&body).unwrap().status, status);
    }
    assert!(state.list_access_logs(None).await.is_empty());
    state.remove_server(id).await;
}
#[tokio::test]
async fn exact_ip_secret_override_and_wrong_secret_fail_without_handler() {
    let(state,id,addr,_)=server(Some(policies()),Some(json!({"shared_secret":"different-default","client_secrets":[{"client_ip":"127.0.0.1","shared_secret":"test-secret"}]}))).await;
    let mut peer = TcpStream::connect(addr).await.unwrap();
    let a: Authentication =
        serde_json::from_value(json!({"username":"alice","password":"correct","method":"pap"}))
            .unwrap();
    let (v, request_body) = authentication_body(&a).unwrap();
    reply(&mut peer, header(1, v), &request_body).await;
    let (_, body) = read(&mut peer).await;
    assert_eq!(parse_auth_reply(&body).unwrap().status, AuthStatus::Pass);
    let mut peer = TcpStream::connect(addr).await.unwrap();
    write_packet(&mut peer, header(1, v), &request_body, b"wrong-secret")
        .await
        .unwrap();
    let (_, body) = read(&mut peer).await;
    assert_eq!(parse_auth_reply(&body).unwrap().status, AuthStatus::Error);
    assert_eq!(
        logs(
            &state,
            AccessLogOwner::Server(id.as_u32()),
            "tacacs_authentication",
            1
        )
        .await
        .len(),
        1
    );
    state.remove_server(id).await;
}
#[tokio::test]
async fn parser_deadline_and_oversized_length_close_before_allocation_or_handler() {
    let (state, id, addr, _) = server(
        None,
        Some(json!({"shared_secret":"test-secret","io_timeout_seconds":1})),
    )
    .await;
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(&[0xc0]).await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), peer.read(&mut [0]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    let mut peer = TcpStream::connect(addr).await.unwrap();
    let mut bytes = header(1, 0xc0).bytes().unwrap();
    bytes[8..].copy_from_slice(&((MAX_BODY_BYTES + 1) as u32).to_be_bytes());
    peer.write_all(&bytes).await.unwrap();
    assert_eq!(peer.read(&mut [0]).await.unwrap(), 0);
    assert!(state.list_access_logs(None).await.is_empty());
    state.remove_server(id).await;
}
#[tokio::test]
async fn handler_deadline_and_peer_eof_remove_intercepts_without_accounting_success() {
    for close in [true, false] {
        let(state,id,addr,_)=server(Some(vec![json!({"event_pattern":"tacacs_accounting","handler":{"type":"manual","timeout_secs":300}})]),Some(json!({"shared_secret":"test-secret","handler_timeout_seconds":1}))).await;
        let mut peer = TcpStream::connect(addr).await.unwrap();
        let r: Request = serde_json::from_value(json!({"username":"alice"})).unwrap();
        reply(
            &mut peer,
            header(3, 0xc0),
            &request_body(&r, Some(AccountKind::Start)).unwrap(),
        )
        .await;
        intercept(&state).await;
        if close {
            drop(peer);
        } else {
            let (_, body) = read(&mut peer).await;
            assert_eq!(
                parse_account_reply(&body).unwrap().status,
                AccountStatus::Error
            );
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while !state.list_intercepts().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!state
            .list_access_logs(None)
            .await
            .iter()
            .any(|r| r.event_type == "tacacs_accounting_recorded"));
        state.remove_server(id).await;
    }
}
#[tokio::test]
async fn removal_cancels_owned_connections_parked_handler_and_listener() {
    let(state,id,addr,_)=server(Some(vec![json!({"event_pattern":"tacacs_accounting","handler":{"type":"manual","timeout_secs":300}})]),None).await;
    let mut peer = TcpStream::connect(addr).await.unwrap();
    let r: Request = serde_json::from_value(json!({"username":"alice"})).unwrap();
    reply(
        &mut peer,
        header(3, 0xc0),
        &request_body(&r, Some(AccountKind::Start)).unwrap(),
    )
    .await;
    intercept(&state).await;
    state.remove_server(id).await;
    assert_eq!(peer.read(&mut [0]).await.unwrap(), 0);
    assert!(state.list_intercepts().await.is_empty());
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    drop(listener);
}
