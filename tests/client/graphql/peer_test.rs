//! NetGet's GraphQL client against strawberry-graphql 0.330.2 — an independent server,
//! unchanged, under uvicorn: introspection on connect, POST and GET queries with variables, a
//! union with fragments, a mutation, a resolver error and a validation error. Fails, never skips.
use crate::helpers::graphql::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::AsyncBufReadExt;

#[tokio::test(flavor = "multi_thread")]
async fn client_runs_queries_and_mutations_against_strawberry() {
    let mut child = tokio::process::Command::new(python())
        .arg(peer_script())
        .arg("server")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let first: Value = serde_json::from_str(
        &tokio::time::timeout(Duration::from_secs(60), lines.next_line())
            .await
            .expect("strawberry did not start")
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let port = first["port"].as_u64().unwrap();
    let state = state();
    let cid = client_in(&state, format!("127.0.0.1:{port}"), json!({}))
        .await
        .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    let connected = logs(&state, owner, "graphql_connected", 1).await;
    let roots = &connected[0].request["root_fields"];
    assert!(
        roots["query"]
            .as_array()
            .unwrap()
            .contains(&json!("book(id: ID!): Book")),
        "{roots}"
    );
    assert_eq!(
        roots["mutation"],
        json!(["addBook(title: String!, year: Int): Book!"])
    );
    for a in [
        json!({"type":"graphql_query","query":"query B($id: ID!) { book(id: $id) { title author { name } } }","variables":{"id":"1"}}),
        json!({"type":"graphql_query","query":"query H($n: String) { hello(name: $n) }","variables":{"n":"NetGet"},"use_get":true}),
        json!({"type":"graphql_query","query":"{ search(term: \"du\") { __typename ... on Book { title } ... on Author { name } } }"}),
        json!({"type":"graphql_query","query":"mutation { addBook(title: \"Emma\", year: 1815) { id title year } }"}),
        json!({"type":"graphql_query","query":"{ book(id: \"404\") { title } }"}),
        json!({"type":"graphql_query","query":"{ book(id: \"1\") { isbn } }"}),
    ] {
        assert!(matches!(
            state
                .send_to_client(cid, a, Duration::from_secs(20))
                .await
                .unwrap(),
            ClientSendOutcome::Sent { .. }
        ));
    }
    let rows = logs(&state, owner, "graphql_response", 6).await;
    let r: Vec<&Value> = rows.iter().map(|r| &r.request).collect();
    assert_eq!(
        r[0]["data"],
        json!({"book": {"title": "Dune", "author": {"name": "Frank Herbert"}}})
    );
    assert_eq!(r[0]["operation_name"], "B");
    assert_eq!(r[1]["data"], json!({"hello": "Hello, NetGet"}));
    assert_eq!(
        r[2]["data"]["search"],
        json!([{"__typename": "Book", "title": "Dune"}, {"__typename": "Author", "name": "Frank Herbert"}])
    );
    assert_eq!(
        (
            r[3]["operation_type"].as_str(),
            &r[3]["data"]["addBook"]["title"]
        ),
        (Some("mutation"), &json!("Emma"))
    );
    assert_eq!(r[4]["data"], json!({"book": null}));
    assert_eq!(r[4]["errors"][0]["path"], json!(["book"]));
    assert!(
        r[5].get("data").is_none() || r[5]["data"].is_null(),
        "{}",
        r[5]
    );
    assert!(r[5]["errors"][0]["message"]
        .as_str()
        .unwrap()
        .contains("isbn"));
    // A mutation over GET and a subscription are refused before anything is sent.
    for a in [
        json!({"type":"graphql_query","query":"mutation { addBook(title: \"x\") { id } }","use_get":true}),
        json!({"type":"graphql_query","query":"subscription { ticks }"}),
        json!({"type":"graphql_query","query":"{ book(id: "}),
    ] {
        assert!(matches!(
            state
                .send_to_client(cid, a, Duration::from_secs(5))
                .await
                .unwrap(),
            ClientSendOutcome::Rejected { .. }
        ));
    }
    state.remove_client(cid).await;
    drop(child.stdin.take());
    let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
}

/// NetGet's client over strawberry's graphql-transport-ws: a countdown that completes, an
/// endless subscription the client cancels, and a subscription strawberry refuses.
#[tokio::test(flavor = "multi_thread")]
async fn client_subscribes_against_strawberry() {
    let mut child = tokio::process::Command::new(python())
        .arg(peer_script())
        .arg("server")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let first: Value = serde_json::from_str(
        &tokio::time::timeout(Duration::from_secs(60), lines.next_line())
            .await
            .expect("strawberry did not start")
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let port = first["port"].as_u64().unwrap();
    let state = state();
    let cid = client_in(
        &state,
        format!("127.0.0.1:{port}"),
        json!({"introspect": false}),
    )
    .await
    .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    let send = |a: Value| {
        let state = &state;
        async move {
            state
                .send_to_client(cid, a, Duration::from_secs(20))
                .await
                .unwrap()
        }
    };
    assert!(matches!(
        send(json!({"type":"graphql_subscribe","query":"subscription C($n: Int!) { countdown(from: $n) }","variables":{"n":3}})).await,
        ClientSendOutcome::Sent { .. }
    ));
    let events = logs(&state, owner, "graphql_subscription_event", 3).await;
    let values: Vec<&Value> = events
        .iter()
        .map(|e| &e.request["data"]["countdown"])
        .collect();
    assert_eq!(values, [&json!(3), &json!(2), &json!(1)]);
    assert_eq!(events[0].request["operation_name"], "C");
    let done = logs(&state, owner, "graphql_subscription_complete", 1).await;
    assert_eq!(
        done[0].request["subscription_id"],
        events[0].request["subscription_id"]
    );

    assert!(matches!(
        send(json!({"type":"graphql_subscribe","query":"subscription { ticks }"})).await,
        ClientSendOutcome::Sent { .. }
    ));
    let ticks = logs(&state, owner, "graphql_subscription_event", 5).await;
    let tick_id = ticks[3].request["subscription_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(
        tick_id,
        events[0].request["subscription_id"].as_str().unwrap()
    );
    assert!(matches!(
        send(json!({"type":"graphql_unsubscribe","subscription_id":tick_id})).await,
        ClientSendOutcome::Sent { .. }
    ));
    assert!(matches!(
        send(json!({"type":"graphql_unsubscribe","subscription_id":tick_id})).await,
        ClientSendOutcome::Rejected { .. }
    ));
    assert!(matches!(
        send(json!({"type":"graphql_subscribe","query":"subscription { nope }"})).await,
        ClientSendOutcome::Sent { .. }
    ));
    let refused = logs(&state, owner, "graphql_subscription_error", 1).await;
    assert!(refused[0].request["errors"][0]["message"]
        .as_str()
        .unwrap()
        .contains("nope"));
    // The cancelled subscription never completed from the server's side.
    let completes = logs(&state, owner, "graphql_subscription_complete", 1).await;
    assert_eq!(completes.len(), 1);
    assert!(matches!(
        send(json!({"type":"graphql_subscribe","query":"{ hello }"})).await,
        ClientSendOutcome::Rejected { .. }
    ));
    state.remove_client(cid).await;
    drop(child.stdin.take());
    let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
}
