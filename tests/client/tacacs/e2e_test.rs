use crate::helpers::tacacs::{
    accounting, authentication, authorization, client, disconnected, intercept, logs, read, reply,
};
use netget::{
    server::tacacs::codec::*,
    state::{client_handles::ClientSendOutcome, AccessLogOwner},
};
use serde_json::json;
use std::time::Duration;
use tokio::{io::AsyncReadExt, net::TcpListener};
#[tokio::test]
async fn parked_connected_handler_does_not_block_injection_or_disconnect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let(state,id)=client(listener.local_addr().unwrap().to_string(),Some(vec![json!({"event_pattern":"tacacs_connected","handler":{"type":"manual","timeout_secs":300}})]),None).await;
    intercept(&state).await;
    let send = state.send_to_client(id, authentication("pap"), Duration::from_secs(10));
    let receive = async {
        let (mut peer, _) = listener.accept().await.unwrap();
        let (h, body) = read(&mut peer).await;
        let start = parse_start(&body).unwrap();
        assert_eq!(start.password.unwrap(), "correct");
        reply(
            &mut peer,
            h.reply().unwrap(),
            &auth_reply_body(&AuthReply {
                status: AuthStatus::Pass,
                no_echo: false,
                server_message: "accepted".into(),
                data: String::new(),
            })
            .unwrap(),
        )
        .await;
    };
    let (out, _) = tokio::join!(send, receive);
    assert!(matches!(out.unwrap(), ClientSendOutcome::Executed { .. }));
    assert!(!state.list_intercepts().await.is_empty());
    assert!(matches!(
        state
            .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(2))
            .await
            .unwrap(),
        ClientSendOutcome::Disconnected
    ));
    disconnected(&state, id).await;
    assert!(state.list_intercepts().await.is_empty());
    state.remove_client(id).await;
}
#[tokio::test]
async fn one_inflight_and_disconnect_cancel_socket_without_replay() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (state, id) = client(listener.local_addr().unwrap().to_string(), None, None).await;
    let send = state.send_to_client(id, accounting("start"), Duration::from_secs(10));
    let drive = async {
        let (mut peer, _) = listener.accept().await.unwrap();
        read(&mut peer).await;
        let refused = state
            .send_to_client(id, authentication("pap"), Duration::from_secs(2))
            .await
            .unwrap();
        assert!(matches!(refused, ClientSendOutcome::Rejected { .. }));
        state
            .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(peer.read(&mut [0]).await.unwrap(), 0);
    };
    let (out, _) = tokio::join!(send, drive);
    assert!(out.is_err());
    disconnected(&state, id).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
    state.remove_client(id).await;
}
#[tokio::test]
async fn removal_cancels_pending_exchange_and_parked_intercept() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let(state,id)=client(listener.local_addr().unwrap().to_string(),Some(vec![json!({"event_pattern":"tacacs_connected","handler":{"type":"manual","timeout_secs":300}})]),None).await;
    intercept(&state).await;
    let send = state.send_to_client(id, accounting("stop"), Duration::from_secs(5));
    let remove = async {
        let (mut peer, _) = listener.accept().await.unwrap();
        read(&mut peer).await;
        state.remove_client(id).await;
        assert_eq!(peer.read(&mut [0]).await.unwrap(), 0);
    };
    let (out, _) = tokio::join!(send, remove);
    assert!(out.is_err());
    assert!(state.list_intercepts().await.is_empty());
    assert!(!state.has_client_handle(id).await);
}
#[tokio::test]
async fn invalid_typed_actions_are_atomic_before_tcp_connect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (state, id) = client(listener.local_addr().unwrap().to_string(), None, None).await;
    for action in [
        json!({"type":"authenticate_tacacs","username":"alice","password":"correct","method":"chap"}),
        json!({"type":"authenticate_tacacs","username":"a b","password":"x"}),
        json!({"type":"authenticate_tacacs","username":"alice","password":"x","undeclared":true}),
        json!({"type":"account_tacacs","record_type":"both","request":{"username":"alice"}}),
        json!({"type":"authorize_tacacs","request":{"username":"alice","arguments":[{"name":"bad=name","value":"x"}]}}),
    ] {
        assert!(matches!(
            state
                .send_to_client(id, action, Duration::from_secs(2))
                .await
                .unwrap(),
            ClientSendOutcome::Rejected { .. }
        ));
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
    state.remove_client(id).await;
}
#[tokio::test]
async fn reply_type_version_session_sequence_and_unencrypted_bit_are_checked() {
    for defect in 0..5 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (state, id) = client(listener.local_addr().unwrap().to_string(), None, None).await;
        let send = state.send_to_client(id, accounting("start"), Duration::from_secs(10));
        let receive = async {
            let (mut peer, _) = listener.accept().await.unwrap();
            let (h, _) = read(&mut peer).await;
            let mut h = h.reply().unwrap();
            match defect {
                0 => h.kind = 2,
                1 => h.version = 0xc1,
                2 => h.session_id ^= 1,
                3 => h.sequence = 4,
                _ => h.flags = 1,
            };
            reply(
                &mut peer,
                h,
                &account_reply_body(&AccountReply {
                    status: AccountStatus::Success,
                    server_message: String::new(),
                    data: String::new(),
                })
                .unwrap(),
            )
            .await;
        };
        let (out, _) = tokio::join!(send, receive);
        assert!(out.is_err());
        let rows = logs(
            &state,
            AccessLogOwner::Client(id.as_u32()),
            "tacacs_error",
            1,
        )
        .await;
        assert_eq!(
            rows[0].request["error"],
            "TACACS transport, framing, correlation or flow failure; no replay"
        );
        assert!(!state
            .list_access_logs(None)
            .await
            .iter()
            .any(|e| e.event_type == "tacacs_accounting_result"));
        state.remove_client(id).await;
    }
}
#[tokio::test]
async fn authorization_pass_add_replace_unknown_mandatory_and_privilege_are_safe() {
    for (status, args, accepted, effective) in [
        (
            AuthorStatus::PassAdd,
            json!([{"name":"priv-lvl","value":"15"},{"name":"audit","value":"enabled","mandatory":false}]),
            true,
            4,
        ),
        (
            AuthorStatus::PassReplace,
            json!([{"name":"service","value":"shell"}]),
            true,
            1,
        ),
        (
            AuthorStatus::PassAdd,
            json!([{"name":"site_policy","value":"required"}]),
            false,
            0,
        ),
        (
            AuthorStatus::PassReplace,
            json!([{"name":"priv-lvl","value":"16"}]),
            false,
            0,
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (state, id) = client(listener.local_addr().unwrap().to_string(), None, None).await;
        let send = state.send_to_client(id, authorization(), Duration::from_secs(10));
        let receive = async {
            let (mut peer, _) = listener.accept().await.unwrap();
            let (h, body) = read(&mut peer).await;
            let (r, _) = parse_request(&body, false).unwrap();
            assert_eq!(r.arguments[2].value, "version");
            reply(
                &mut peer,
                h.reply().unwrap(),
                &author_reply_body(&AuthorReply {
                    status,
                    arguments: serde_json::from_value(args).unwrap(),
                    server_message: String::new(),
                    data: String::new(),
                })
                .unwrap(),
            )
            .await;
        };
        let (out, _) = tokio::join!(send, receive);
        assert!(matches!(out.unwrap(), ClientSendOutcome::Executed { .. }));
        let rows = logs(
            &state,
            AccessLogOwner::Client(id.as_u32()),
            "tacacs_authorization_result",
            1,
        )
        .await;
        assert_eq!(rows[0].request["authorized"], accepted);
        assert_eq!(
            rows[0].request["effective_arguments"]
                .as_array()
                .unwrap()
                .len(),
            effective
        );
        assert_eq!(rows[0].request["device_policy_applied"], false);
        state.remove_client(id).await;
    }
}
#[tokio::test]
async fn unsupported_auth_prompt_aborts_and_restart_follow_fail_without_retry() {
    for status in [AuthStatus::GetData, AuthStatus::Restart, AuthStatus::Follow] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (state, id) = client(listener.local_addr().unwrap().to_string(), None, None).await;
        let send = state.send_to_client(id, authentication("ascii"), Duration::from_secs(10));
        let receive = async {
            let (mut peer, _) = listener.accept().await.unwrap();
            let (h, _) = read(&mut peer).await;
            reply(
                &mut peer,
                h.reply().unwrap(),
                &auth_reply_body(&AuthReply {
                    status,
                    no_echo: false,
                    server_message: "unsupported".into(),
                    data: String::new(),
                })
                .unwrap(),
            )
            .await;
            if status == AuthStatus::GetData {
                let (h, body) = read(&mut peer).await;
                assert_eq!(h.sequence, 3);
                assert!(parse_continue(&body).unwrap().abort);
            }
        };
        let (out, _) = tokio::join!(send, receive);
        assert_eq!(out.is_ok(), status != AuthStatus::GetData);
        if status != AuthStatus::GetData {
            let rows = logs(
                &state,
                AccessLogOwner::Client(id.as_u32()),
                "tacacs_authentication_result",
                1,
            )
            .await;
            assert_eq!(rows[0].request["authenticated"], false);
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        state.remove_client(id).await;
    }
}
#[tokio::test]
async fn event_handler_common_memory_is_updated_and_excessive_actions_fail_before_network() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let(state,id)=client(listener.local_addr().unwrap().to_string(),Some(vec![json!({"event_pattern":"tacacs_connected","handler":{"type":"static","actions":[{"type":"set_memory","value":"ready"}]}})]),None).await;
    logs(
        &state,
        AccessLogOwner::Client(id.as_u32()),
        "tacacs_connected",
        1,
    )
    .await;
    assert_eq!(state.get_memory_for_client(id).await.unwrap(), "ready");
    state.remove_client(id).await;
    let(state,id)=client(listener.local_addr().unwrap().to_string(),Some(vec![json!({"event_pattern":"tacacs_connected","handler":{"type":"static","actions":vec![authentication("pap");33]}})]),None).await;
    disconnected(&state, id).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
    state.remove_client(id).await;
}

#[tokio::test]
async fn packet_and_parked_handler_deadlines_release_without_replay() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (state,id)=client(listener.local_addr().unwrap().to_string(),
        Some(vec![json!({"event_pattern":"tacacs_connected","handler":{"type":"manual","timeout_secs":300}})]),
        Some(json!({"shared_secret":"test-secret","io_timeout_seconds":1,"handler_timeout_seconds":1}))).await;
    intercept(&state).await;
    let send = state.send_to_client(id, accounting("start"), Duration::from_secs(5));
    let receive = async {
        let (mut peer, _) = listener.accept().await.unwrap();
        read(&mut peer).await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), peer.read(&mut [0]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    };
    let (out, _) = tokio::join!(send, receive);
    assert!(out.is_err());
    logs(
        &state,
        AccessLogOwner::Client(id.as_u32()),
        "tacacs_error",
        1,
    )
    .await;
    assert!(state.list_intercepts().await.is_empty());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
    state.remove_client(id).await;
}
#[tokio::test]
async fn ascii_prompt_round_cap_sends_abort_and_never_accepts() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (state, id) = client(listener.local_addr().unwrap().to_string(), None, None).await;
    let send = state.send_to_client(id, authentication("ascii"), Duration::from_secs(10));
    let receive = async {
        let (mut peer, _) = listener.accept().await.unwrap();
        let (mut h, _) = read(&mut peer).await;
        for round in 0..MAX_AUTH_ROUNDS {
            reply(
                &mut peer,
                h.reply().unwrap(),
                &auth_reply_body(&AuthReply {
                    status: AuthStatus::GetPass,
                    no_echo: true,
                    server_message: "Password:".into(),
                    data: String::new(),
                })
                .unwrap(),
            )
            .await;
            let (next, body) = read(&mut peer).await;
            let c = parse_continue(&body).unwrap();
            assert_eq!(c.abort, round + 1 == MAX_AUTH_ROUNDS);
            h = next;
        }
    };
    let (out, _) = tokio::join!(send, receive);
    assert!(out.is_err());
    assert!(!state
        .list_access_logs(None)
        .await
        .iter()
        .any(|r| r.event_type == "tacacs_authentication_result"));
    state.remove_client(id).await;
}
