//! gql 4.4.0 — an independent GraphQL client, unchanged — against NetGet's server. gql builds
//! its schema from our introspection answer (graphql-core's `build_client_schema`, strict about
//! every field), validates each document against it before sending, and runs variables,
//! aliases, a union with fragments, a mutation and a field error, first under legacy
//! `application/json` and then asking for `application/graphql-response+json`. Fails, never skips.
use crate::helpers::graphql::*;
use netget::state::AccessLogOwner;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::AsyncBufReadExt;

#[tokio::test(flavor = "multi_thread")]
async fn gql_client_introspects_validates_and_runs_operations_against_netget() {
    let state = state();
    let (sid, addr) = server_in(&state, book_policy(), json!({"schema": BOOK_SCHEMA})).await;
    let out = tokio::time::timeout(
        Duration::from_secs(90),
        tokio::process::Command::new(python())
            .arg(peer_script())
            .args(["client", &format!("http://{addr}/graphql")])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("gql client timed out")
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "gql client failed: {stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let steps: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(steps.len(), 12, "{stdout}");
    for round in steps.chunks(6) {
        let [schema, book, aliases, mutation, field_error, local] = round else {
            unreachable!()
        };
        assert_eq!(
            schema["types"],
            json!([
                "Author",
                "Book",
                "Boolean",
                "ID",
                "Int",
                "Mutation",
                "Query",
                "SearchResult",
                "String",
                "Subscription"
            ])
        );
        assert_eq!(
            schema["query"],
            json!(["book", "hello", "search", "secret"])
        );
        assert_eq!(schema["mutation"], json!(["addBook"]));
        assert_eq!(
            book["result"],
            json!({"book": {"id": "1", "title": "Dune", "year": 1965, "author": {"name": "Frank Herbert"}}})
        );
        assert_eq!(
            aliases["result"],
            json!({"a": {"title": "Dune"}, "b": {"title": "Emma"}, "search": [{"__typename": "Book", "title": "Dune"}, {"__typename": "Author", "name": "Frank Herbert"}]})
        );
        assert_eq!(
            mutation["result"],
            json!({"addBook": {"id": "3", "title": "Emma"}})
        );
        assert_eq!(field_error["error"][0]["message"], "book 404 is gone");
        assert_eq!(field_error["error"][0]["path"], json!(["book"]));
        assert_eq!(field_error["data"], json!({"book": null}));
        assert!(
            local["error"].as_str().unwrap().contains("isbn"),
            "gql refuses a field our schema does not have before sending: {local}"
        );
    }
    // Introspection is Rust's: only the 4 operations per round reached the handler.
    let seen = logs(
        &state,
        AccessLogOwner::Server(sid.as_u32()),
        "graphql_operation",
        8,
    )
    .await;
    assert_eq!(seen.len(), 8);
    assert_eq!(seen[2].request["operation_type"], "mutation");
    assert_eq!(
        seen[0].request["shape"],
        json!({"book": {"id": "ID!", "title": "String!", "year": "Int", "author": {"name": "String!"}}})
    );
    state.remove_server(sid).await;
}

/// gql's websockets transport over graphql-transport-ws fetches the schema over the socket, runs a query, a countdown the handler answers
/// in full, a refused subscription, then a subscription fed by `send_to_peer` that gql cancels
/// after two events; the cancellation reaches NetGet as `complete`.
#[tokio::test(flavor = "multi_thread")]
async fn gql_websocket_transport_runs_subscriptions_against_netget() {
    use netget::state::client_handles::ClientSendOutcome;
    let state = state();
    let (sid, addr) = server_in(&state, book_policy(), json!({"schema": BOOK_SCHEMA})).await;
    let mut child = tokio::process::Command::new(python())
        .arg(peer_script())
        .args(["ws", &format!("http://{addr}/graphql")])
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let mut steps = Vec::new();
    loop {
        let step = next_step(&mut lines).await;
        let waiting = step["step"] == "waiting";
        steps.push(step);
        if waiting {
            break;
        }
    }
    assert_eq!(steps[0]["subprotocol"], "graphql-transport-ws");
    assert_eq!(
        steps[0]["subscription"],
        json!(["bookAdded", "countdown", "forbidden"])
    );
    assert_eq!(steps[1]["result"], json!({"hello": "Hello, socket"}));
    assert_eq!(
        steps[2]["events"],
        json!([{"countdown": 3}, {"countdown": 2}, {"countdown": 1}])
    );
    assert_eq!(steps[3]["error"][0]["message"], "not authorized");
    let owner = AccessLogOwner::Server(sid.as_u32());
    let starts = logs(&state, owner, "graphql_subscription_start", 3).await;
    let book = &starts[2];
    assert_eq!(book.request["root_fields"][0]["field"], "bookAdded");
    let conn = book.connection_id.unwrap();
    let sub = book.request["subscription_id"].as_str().unwrap().to_owned();
    for title in ["Emma", "Persuasion"] {
        let pushed = state
            .send_to_peer(
                sid,
                conn,
                json!({"type":"graphql_event","subscription_id":sub,"data":{"bookAdded":{"title":title,"author":{"name":"Jane Austen"},"year":1815}}}),
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert!(
            matches!(pushed, ClientSendOutcome::Sent { .. }),
            "{pushed:?}"
        );
    }
    let pushed = next_step(&mut lines).await;
    assert_eq!(
        pushed["events"],
        json!([{"bookAdded": {"title": "Emma", "author": {"name": "Jane Austen"}}}, {"bookAdded": {"title": "Persuasion", "author": {"name": "Jane Austen"}}}])
    );
    // gql's cancellation arrives as `complete`; after it the subscription takes no events.
    let gone = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let r = state
                .send_to_peer(sid, conn, json!({"type":"graphql_event","subscription_id":sub,"data":{"bookAdded":{"title":"x","author":{"name":"y"}}}}), Duration::from_secs(5))
                .await;
            match r {
                Ok(ClientSendOutcome::Rejected { error }) => break error,
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .expect("cancelled subscription still accepted events");
    assert!(gone.contains("no active subscription"), "{gone}");
    let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
    state.remove_server(sid).await;
}

async fn next_step(
    lines: &mut tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
) -> Value {
    let line = tokio::time::timeout(Duration::from_secs(60), lines.next_line())
        .await
        .expect("gql websockets client stalled")
        .unwrap()
        .expect("gql websockets client exited early");
    serde_json::from_str(&line).unwrap()
}
