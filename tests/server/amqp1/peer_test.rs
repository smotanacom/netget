//! Independent AMQP 1.0 clients, unchanged, against NetGet's container with the policy script:
//! rhea 3.0.5 (JavaScript) and go-amqp 1.7.0 (Go) authenticate with SASL PLAIN, consume and
//! publish, get accepted and rejected outcomes, receive the confirmation the handler routes to
//! orders.confirmed, are refused a forbidden address and a wrong password; rhea also gets the
//! message the handler produces for its news receiver and its own data message back on chat.
//! Fails, never skips.
use crate::helpers::amqp1::*;
use netget::state::AccessLogOwner;
use serde_json::json;
use std::time::Duration;

async fn run(mut c: tokio::process::Command) -> Vec<serde_json::Value> {
    let out = tokio::time::timeout(Duration::from_secs(60), c.kill_on_drop(true).output())
        .await
        .expect("peer timed out")
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "peer failed:\n{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    lines(&text)
}

#[tokio::test(flavor = "multi_thread")]
async fn rhea_against_netget_container() {
    let state = state();
    let (sid, addr) = server_in(&state, policy(), json!({})).await;
    let mut c = tokio::process::Command::new("node");
    c.env("NODE_PATH", peer("NETGET_AMQP1_NODE_MODULES"))
        .arg(script("client.cjs"))
        .args(["127.0.0.1", &addr.port().to_string()]);
    let l = run(c).await;
    assert_eq!(step(&l, "open")["container"], "netget");
    assert_eq!(step(&l, "order")["outcome"], "accepted");
    let no = step(&l, "no_order");
    assert_eq!(
        (
            no["outcome"].as_str(),
            no["error"]["condition"].as_str(),
            no["error"]["description"].as_str()
        ),
        (
            Some("rejected"),
            Some("amqp:precondition-failed"),
            Some("no order id")
        )
    );
    assert_eq!(
        (
            step(&l, "forbidden")["condition"].as_str(),
            step(&l, "forbidden")["description"].as_str()
        ),
        (Some("amqp:unauthorized-access"), Some("forbidden address"))
    );
    let got = step(&l, "received")["messages"].as_array().unwrap().clone();
    assert!(got.contains(&json!({"address": "orders.confirmed", "body": {"order": 1, "status": "confirmed"}, "subject": "confirmation"})), "{got:?}");
    assert!(
        got.contains(&json!({"address": "news", "body": "fresh news", "subject": "news"})),
        "{got:?}"
    );
    assert!(
        got.contains(&json!({"address": "chat", "body": "hello from rhea", "subject": "hello"})),
        "{got:?}"
    );
    assert_ne!(step(&l, "bad_password")["result"], "opened");

    let owner = AccessLogOwner::Server(sid.as_u32());
    let connects = logs(&state, owner, "amqp1_connect", 2).await;
    assert_eq!(
        (
            connects[0].request["mechanism"].as_str(),
            connects[0].request["user"].as_str()
        ),
        (Some("PLAIN"), Some("alice"))
    );
    let messages = logs(&state, owner, "amqp1_message", 3).await;
    let chat = messages
        .iter()
        .find(|m| m.request["address"] == "chat")
        .unwrap();
    assert_eq!(
        (
            chat.request["message"]["body_type"].as_str(),
            chat.request["message"]["body"].as_str()
        ),
        (Some("data"), Some("hello from rhea"))
    );
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn go_amqp_against_netget_container() {
    let state = state();
    let (sid, addr) = server_in(&state, policy(), json!({})).await;
    let mut c = tokio::process::Command::new(peer("NETGET_AMQP1_GOAMQP"));
    c.arg(addr.to_string());
    let l = run(c).await;
    assert_eq!(
        step(&l, "data_order")["result"],
        "<nil>",
        "a JSON data body with an order id is accepted: {l:?}"
    );
    assert_eq!(step(&l, "order")["outcome"], "accepted", "{l:?}");
    assert_eq!(
        step(&l, "no_order")["condition"],
        "amqp:precondition-failed",
        "{l:?}"
    );
    let confirmation = step(&l, "confirmation");
    assert_eq!(confirmation["subject"], "confirmation", "{l:?}");
    assert_eq!(confirmation["value"]["status"], "confirmed");
    assert_eq!(
        step(&l, "forbidden")["condition"],
        "amqp:unauthorized-access",
        "{l:?}"
    );
    assert_ne!(step(&l, "bad_password")["error"], "<nil>");
    state.remove_server(sid).await;
}
