//! A2A 1.0 shapes without peers, the agent's JSON-RPC refusals, and the NetGet pair.
use crate::helpers::a2a::*;
use netget::server::a2a::model;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;

#[test]
fn versions_messages_tasks_and_cards_follow_the_10_shapes() {
    assert!(model::version_ok(Some("1.0")) && model::version_ok(Some("1.0.3")));
    for bad in [None, Some(""), Some("0.3"), Some("1.1"), Some("1.x")] {
        assert!(!model::version_ok(bad), "{bad:?}");
    }
    let ok = json!({"message":{"messageId":"m","role":"ROLE_USER","parts":[{"text":"a"},{"data":{"k":1}},{"url":"http://x/f","filename":"f.txt"},{"raw":"AAAA"}]}});
    let m = model::incoming_message(&ok).unwrap();
    assert_eq!(m.text, "a");
    assert_eq!(
        m.parts[3]["inline_bytes"], 3,
        "inline bytes reach a handler by size only"
    );
    for bad in [
        json!({}),
        json!({"message":{"role":"ROLE_USER","parts":[{"text":"a"}]}}),
        json!({"message":{"messageId":"m","role":"ROLE_AGENT","parts":[{"text":"a"}]}}),
        json!({"message":{"messageId":"m","role":"ROLE_USER","parts":[]}}),
        json!({"message":{"messageId":"m","role":"ROLE_USER","parts":[{"nothing":1}]}}),
    ] {
        assert!(model::incoming_message(&bad).is_err(), "{bad}");
    }
    let t = model::task(
        &json!({"state":"completed","text":"done","artifacts":[{"name":"a","text":"x"}]}),
        "t1",
        "c1",
    )
    .unwrap();
    assert_eq!(
        (
            t["id"].as_str(),
            t["status"]["state"].as_str(),
            t["status"]["message"]["role"].as_str()
        ),
        (Some("t1"), Some("TASK_STATE_COMPLETED"), Some("ROLE_AGENT"))
    );
    assert!(model::task(&json!({"state":"done"}), "t", "c").is_err());
    let events = model::stream_events(&json!(7), &t);
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| {
            e["result"]
                .as_object()
                .unwrap()
                .keys()
                .next()
                .unwrap()
                .as_str()
        })
        .collect();
    assert_eq!(
        kinds,
        ["task", "statusUpdate", "artifactUpdate", "statusUpdate"]
    );
    assert_eq!(
        events.last().unwrap()["result"]["statusUpdate"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    let card = model::card("n", "d", "1", "http://h:1/", true, &[]);
    assert_eq!(model::card_rpc_url(&card).unwrap(), "http://h:1/");
    assert!(model::card_rpc_url(&json!({"supportedInterfaces":[{"url":"http://h/","protocolBinding":"GRPC","protocolVersion":"1.0"}]})).is_err());
    assert!(model::check_result("GetTask", &json!({"id":"x"})).is_err());
}

async fn rpc(addr: std::net::SocketAddr, body: &str, version: Option<&str>) -> Value {
    let mut req = reqwest::Client::new()
        .post(format!("http://{addr}/"))
        .header("Content-Type", "application/json")
        .body(body.to_owned());
    if let Some(v) = version {
        req = req.header("A2A-Version", v);
    }
    let text = req.send().await.unwrap().text().await.unwrap();
    serde_json::from_str(
        text.trim()
            .strip_prefix("data:")
            .unwrap_or(text.trim())
            .trim(),
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_agent_refuses_with_json_rpc_codes_and_never_invents_a_reply() {
    let state = state();
    let (sid, addr) = server_in(&state, echo_agent_policy(), json!({"streaming": false})).await;
    let card: Value = reqwest::get(format!("http://{addr}/.well-known/agent-card.json"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(card["supportedInterfaces"][0]["protocolVersion"], "1.0");
    assert_eq!(card["capabilities"]["streaming"], false);
    let send = r#"{"jsonrpc":"2.0","id":1,"method":"SendMessage","params":{"message":{"messageId":"m","role":"ROLE_USER","parts":[{"text":"hi"}]}}}"#;
    assert_eq!(
        rpc(addr, send, None).await["error"]["code"],
        model::VERSION_NOT_SUPPORTED
    );
    assert_eq!(
        rpc(addr, send, Some("0.3")).await["error"]["code"],
        model::VERSION_NOT_SUPPORTED
    );
    assert_eq!(
        rpc(addr, "{nope", Some("1.0")).await["error"]["code"],
        -32700
    );
    assert_eq!(
        rpc(
            addr,
            r#"{"jsonrpc":"1.0","id":1,"method":"SendMessage"}"#,
            Some("1.0")
        )
        .await["error"]["code"],
        -32600
    );
    assert_eq!(
        rpc(
            addr,
            r#"{"jsonrpc":"2.0","id":1,"method":"Nope","params":{}}"#,
            Some("1.0")
        )
        .await["error"]["code"],
        -32601
    );
    assert_eq!(
        rpc(
            addr,
            r#"{"jsonrpc":"2.0","id":1,"method":"CreateTaskPushNotificationConfig","params":{}}"#,
            Some("1.0")
        )
        .await["error"]["code"],
        model::PUSH_NOT_SUPPORTED
    );
    assert_eq!(
        rpc(
            addr,
            r#"{"jsonrpc":"2.0","id":1,"method":"SendStreamingMessage","params":{}}"#,
            Some("1.0")
        )
        .await["error"]["code"],
        model::UNSUPPORTED_OPERATION,
        "streaming not advertised"
    );
    assert_eq!(rpc(addr, r#"{"jsonrpc":"2.0","id":1,"method":"SendMessage","params":{"message":{"messageId":"m","role":"ROLE_AGENT","parts":[{"text":"x"}]}}}"#, Some("1.0")).await["error"]["code"], -32602);
    let ok = rpc(addr, send, Some("1.0")).await;
    assert_eq!(ok["result"]["message"]["parts"][0]["text"], "echo: hi");
    state.remove_server(sid).await;

    let state = crate::helpers::a2a::state();
    let (sid, addr) = server_in(&state, vec![], json!({})).await;
    assert_eq!(
        rpc(addr, send, Some("1.0")).await["error"]["code"],
        -32603,
        "no handler and no model: an internal error, never a reply"
    );
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_client_and_agent_agree_and_a_card_pointing_elsewhere_is_refused() {
    let state = state();
    let (sid, addr) = server_in(&state, echo_agent_policy(), json!({})).await;
    let cid = client_in(&state, addr.to_string(), json!({}))
        .await
        .unwrap();
    for a in [
        json!({"type":"a2a_send_message","text":"hello","context_id":"ctx-1"}),
        json!({"type":"a2a_send_message","text":"a task please","stream":true}),
        json!({"type":"a2a_get_task","task_id":"t-9"}),
        json!({"type":"a2a_cancel_task","task_id":"t-9"}),
        json!({"type":"a2a_list_tasks"}),
        json!({"type":"a2a_get_task","task_id":"missing-1"}),
    ] {
        assert!(matches!(
            state
                .send_to_client(cid, a, Duration::from_secs(15))
                .await
                .unwrap(),
            ClientSendOutcome::Sent { .. }
        ));
    }
    let rows = logs(
        &state,
        AccessLogOwner::Client(cid.as_u32()),
        "a2a_response",
        6,
    )
    .await;
    assert_eq!(rows[0].request["result"]["message"]["contextId"], "ctx-1");
    assert_eq!(
        rows[1].request["stream_events"].as_array().unwrap().len(),
        4
    );
    assert_eq!(
        (
            rows[2].request["task_id"].as_str(),
            rows[2].request["state"].as_str()
        ),
        (Some("t-9"), Some("completed"))
    );
    assert_eq!(rows[3].request["state"], "canceled");
    assert_eq!(rows[4].request["result"]["tasks"][0]["id"], "t-1");
    assert_eq!(rows[5].request["error"]["code"], -32001);
    let seen = logs(
        &state,
        AccessLogOwner::Server(sid.as_u32()),
        "a2a_message",
        2,
    )
    .await;
    assert_eq!(seen[0].request["context_id"], "ctx-1");
    assert!(matches!(
        state
            .send_to_client(cid, json!({"type":"a2a_get_task"}), Duration::from_secs(5))
            .await
            .unwrap(),
        ClientSendOutcome::Rejected { .. }
    ));
    state.remove_client(cid).await;
    state.remove_server(sid).await;

    // A card whose JSON-RPC url names another host is refused by default.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut s, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf).await;
            let body = r#"{"name":"elsewhere","supportedInterfaces":[{"url":"http://203.0.113.9:80/","protocolBinding":"JSONRPC","protocolVersion":"1.0"}],"capabilities":{}}"#;
            let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
        }
    });
    let state = crate::helpers::a2a::state();
    let refused = client_in(&state, addr.to_string(), json!({})).await;
    let err = refused.expect_err("a card pointing at 203.0.113.9 must be refused");
    assert!(err.to_string().contains("203.0.113.9"), "{err}");
}
