//! gql 4.4.0 — an independent GraphQL client, unchanged — against NetGet's server. gql builds
//! its schema from our introspection answer (graphql-core's `build_client_schema`, strict about
//! every field), validates each document against it before sending, and runs variables,
//! aliases, a union with fragments, a mutation and a field error, first under legacy
//! `application/json` and then asking for `application/graphql-response+json`. Fails, never skips.
use crate::helpers::graphql::*;
use netget::state::AccessLogOwner;
use serde_json::{json, Value};
use std::time::Duration;

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
                "String"
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
