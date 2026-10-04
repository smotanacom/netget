//! GraphQL-over-HTTP rules without peers: media-type negotiation, request errors and their
//! status codes, execution over handler data, introspection without a handler, fail-closed
//! answers, and the NetGet client/server pair.
use crate::helpers::graphql::*;
use netget::server::graphql::{engine, negotiate, Media};
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Map, Value};
use std::time::Duration;

#[test]
fn accept_negotiation_follows_the_spec() {
    assert_eq!(
        negotiate(None),
        Some(Media::Json),
        "no Accept is legacy JSON"
    );
    assert_eq!(
        negotiate(Some("application/graphql-response+json")),
        Some(Media::GraphqlResponse)
    );
    assert_eq!(
        negotiate(Some("application/json, application/graphql-response+json")),
        Some(Media::GraphqlResponse)
    );
    assert_eq!(
        negotiate(Some(
            "application/graphql-response+json;q=0.5, application/json"
        )),
        Some(Media::Json)
    );
    assert_eq!(negotiate(Some("*/*")), Some(Media::GraphqlResponse));
    assert_eq!(
        negotiate(Some("text/html, application/json;q=0")),
        None,
        "q=0 excludes"
    );
}

#[test]
fn requests_are_parsed_validated_and_executed_over_handler_data() {
    let schema = engine::load_schema(BOOK_SCHEMA).unwrap();
    for bad in [
        "",
        "type Query",
        "type Book { id: ID }",
        "type Query { a: Nope }",
    ] {
        assert!(engine::load_schema(bad).is_err(), "{bad}");
    }
    let vars = |v: Value| v.as_object().cloned().unwrap_or_default();
    let err = |q: &str, op: Option<&str>, v: Value| {
        engine::prepare(&schema, q, op, &vars(v))
            .err()
            .unwrap()
            .body()
    };
    let parse = err("{ book(id: ", None, json!({}));
    assert!(
        parse["errors"][0]["locations"][0]["line"].is_number(),
        "{parse}"
    );
    assert!(err("{ nope }", None, json!({}))["errors"][0]["message"]
        .as_str()
        .unwrap()
        .contains("nope"));
    assert!(
        err("query A { hello } query B { hello }", None, json!({}))["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("operation name")
    );
    assert!(
        err("query A { hello }", Some("Z"), json!({}))["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("Z")
    );
    assert!(
        !err("query($id: ID!) { book(id: $id) { id } }", None, json!({}))["errors"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let deep = format!(
        "{{ hello(name: {}\"x\"{}) }}",
        "[".repeat(200),
        "]".repeat(200)
    );
    assert!(engine::prepare(&schema, &deep, None, &Map::new()).is_err());

    let p = engine::prepare(
        &schema,
        "query Q($id: ID!) { x: book(id: $id) { title year author { name } } search(term: \"d\") { ... on Book { title } ... on Author { name } } }",
        None,
        &vars(json!({"id": 7})),
    )
    .unwrap();
    assert_eq!(
        p.variables_json(),
        json!({"id": 7}),
        "an Int is a valid ID input"
    );
    assert_eq!(
        p.root_fields(),
        vec![
            json!({"response_key": "x", "field": "book", "arguments": {"id": 7}}),
            json!({"response_key": "search", "field": "search", "arguments": {"term": "d"}})
        ]
    );
    assert_eq!(
        p.shape(&schema),
        json!({"x": {"title": "String!", "year": "Int", "author": {"name": "String!"}}, "search": [{"__typename": "one of Author|Book", "title": "String!", "name": "String!"}]})
    );
    // Extra keys are ignored, a missing nullable field is null, a wrong type is a field error.
    let r = serde_json::to_value(
        p.execute(
            &schema,
            &json!({"x": {"title": "Dune", "isbn": "x", "author": {"name": "F"}}, "search": [{"__typename": "Author", "name": "F"}]}),
            true,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        r,
        json!({"data": {"x": {"title": "Dune", "year": null, "author": {"name": "F"}}, "search": [{"name": "F"}]}})
    );
    let r = serde_json::to_value(
        p.execute(
            &schema,
            &json!({"x": {"title": [1], "author": {"name": "F"}}, "search": []}),
            true,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        r["data"],
        json!({"x": null, "search": []}),
        "a non-null failure nulls the nearest nullable parent"
    );
    assert_eq!(r["errors"][0]["path"], json!(["x", "title"]));
    let r = serde_json::to_value(
        p.execute(
            &schema,
            &json!({"x": {"title": "Dune", "author": "nobody"}, "search": [{"title": "no typename"}]}),
            true,
        )
        .unwrap(),
    )
    .unwrap();
    assert!(
        r["data"].is_null(),
        "search is non-null all the way up: {r}"
    );
    let paths: Vec<&Value> = r["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| &e["path"])
        .collect();
    assert!(paths.contains(&&json!(["search", 0])), "{r}");
    assert!(engine::handler_errors(Some(&json!([{"message": ""}]))).is_err());
    assert!(engine::handler_errors(Some(&json!([{"message": "m", "path": [{}]}]))).is_err());
    assert_eq!(
        engine::parse_operation("query A { a } mutation B { b }", Some("B"))
            .unwrap()
            .1
            .as_deref(),
        Some("B")
    );
    assert!(engine::parse_operation("query A { a } query B { b }", None).is_err());
    assert!(engine::check_response(&json!({"data": null})).is_err());
    assert!(engine::check_response(&json!({"errors": []})).is_err());
    assert!(engine::check_response(&json!({"errors": [{"message": "m"}]})).is_ok());
}

async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String, String, Option<String>) {
    let mut req =
        reqwest::Client::new().request(method.parse().unwrap(), format!("http://{addr}{target}"));
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = req.body(body.to_owned()).send().await.unwrap();
    let status = resp.status().as_u16();
    let ct = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let allow = resp
        .headers()
        .get("allow")
        .map(|v| v.to_str().unwrap().to_owned());
    (status, ct, resp.text().await.unwrap(), allow)
}

const JSON: (&str, &str) = ("content-type", "application/json");
const LEGACY_ACCEPT: (&str, &str) = ("accept", "application/json");
const GRAPHQL_ACCEPT: (&str, &str) = ("accept", "application/graphql-response+json");

#[tokio::test(flavor = "multi_thread")]
async fn status_codes_media_types_and_fail_closed_answers() {
    let state = state();
    let (sid, addr) = server_in(&state, book_policy(), json!({"schema": BOOK_SCHEMA})).await;
    let invalid = r#"{"query":"{ nope }"}"#;
    let (s, ct, body, _) = http(addr, "POST", "/graphql", &[JSON, LEGACY_ACCEPT], invalid).await;
    assert_eq!(
        (s, ct.as_str()),
        (200, "application/json; charset=utf-8"),
        "legacy: 200"
    );
    assert!(body.contains("nope") && !body.contains("\"data\""));
    let (s, ct, _, _) = http(addr, "POST", "/graphql", &[JSON, GRAPHQL_ACCEPT], invalid).await;
    assert_eq!(
        (s, ct.as_str()),
        (400, "application/graphql-response+json; charset=utf-8")
    );
    for (body, why) in [
        ("{not json", "not JSON"),
        (r#"{"query": 1}"#, "query not a string"),
        (
            r#"{"query":"{hello}","variables":[1]}"#,
            "variables not an object",
        ),
    ] {
        assert_eq!(
            http(addr, "POST", "/graphql", &[JSON], body).await.0,
            400,
            "{why}"
        );
    }
    assert_eq!(
        http(
            addr,
            "POST",
            "/graphql",
            &[("content-type", "text/plain")],
            invalid
        )
        .await
        .0,
        415
    );
    assert_eq!(
        http(
            addr,
            "POST",
            "/graphql",
            &[JSON, ("accept", "text/html")],
            invalid
        )
        .await
        .0,
        406
    );
    let (s, _, _, allow) = http(addr, "PUT", "/graphql", &[JSON], invalid).await;
    assert_eq!((s, allow.as_deref()), (405, Some("GET, POST")));
    assert_eq!(http(addr, "GET", "/other", &[], "").await.0, 404);
    let (s, _, body, _) = http(
        addr,
        "GET",
        "/graphql?query=query%20H(%24n%3A%20String)%20%7B%20hello(name%3A%20%24n)%20%7D&variables=%7B%22n%22%3A%22GET%22%7D",
        &[GRAPHQL_ACCEPT],
        "",
    )
    .await;
    assert_eq!(
        (s, body.as_str()),
        (200, r#"{"data":{"hello":"Hello, GET"}}"#)
    );
    let (s, _, _, allow) = http(
        addr,
        "GET",
        "/graphql?query=mutation%20%7B%20addBook(title%3A%20%22x%22)%20%7B%20id%20%7D%20%7D",
        &[],
        "",
    )
    .await;
    assert_eq!(
        (s, allow.as_deref()),
        (405, Some("POST")),
        "mutations are never run over GET"
    );
    let (s, _, body, _) = http(
        addr,
        "POST",
        "/graphql",
        &[JSON],
        r#"{"query":"{ secret hello }"}"#,
    )
    .await;
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(s, 200);
    assert_eq!(
        v,
        json!({"errors": [{"message": "not authorized", "extensions": {"code": "FORBIDDEN"}}], "data": null})
    );
    let (_, _, body, _) = http(
        addr,
        "POST",
        "/graphql",
        &[JSON],
        r#"{"query":"{ a: book(id: \"404\") { title } b: book(id: \"1\") { title } }"}"#,
    )
    .await;
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["data"], json!({"a": null, "b": {"title": "Dune"}}));
    assert_eq!(v["errors"][0]["path"], json!(["a"]));
    state.remove_server(sid).await;

    // No handler and no model: introspection still works, anything else is a 500 category
    // error with no data. With introspection off, __schema is a field error.
    let state = crate::helpers::graphql::state();
    let (sid, addr) = server_in(&state, vec![], json!({})).await;
    let (s, _, body, _) = http(
        addr,
        "POST",
        "/graphql",
        &[JSON],
        r#"{"query":"{ __schema { queryType { name } } }"}"#,
    )
    .await;
    assert_eq!(
        (s, body.as_str()),
        (
            200,
            r#"{"data":{"__schema":{"queryType":{"name":"Query"}}}}"#
        )
    );
    let (s, _, body, _) = http(
        addr,
        "POST",
        "/graphql",
        &[JSON, GRAPHQL_ACCEPT],
        r#"{"query":"{ hello }"}"#,
    )
    .await;
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(s, 500);
    assert!(
        v.get("data").is_none() && v["errors"][0]["message"].is_string(),
        "{v}"
    );
    state.remove_server(sid).await;
    let (sid, addr) = server_in(
        &state,
        vec![],
        json!({"introspection": false, "endpoint": "/api"}),
    )
    .await;
    let (_, _, body, _) = http(
        addr,
        "POST",
        "/api",
        &[JSON],
        r#"{"query":"{ __schema { queryType { name } } }"}"#,
    )
    .await;
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(
        v["errors"][0]["message"].is_string() && v["data"].is_null(),
        "{v}"
    );
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_client_and_server_agree() {
    let state = state();
    let (sid, addr) = server_in(&state, book_policy(), json!({"schema": BOOK_SCHEMA})).await;
    let cid = client_in(&state, addr.to_string(), json!({}))
        .await
        .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    let connected = logs(&state, owner, "graphql_connected", 1).await;
    assert_eq!(
        connected[0].request["root_fields"]["query"],
        json!([
            "hello(name: String): String!",
            "book(id: ID!): Book",
            "search(term: String!): [SearchResult!]!",
            "secret: String"
        ])
    );
    for a in [
        json!({"type":"graphql_query","query":"query($id: ID!) { book(id: $id) { title } }","variables":{"id":"2"}}),
        json!({"type":"graphql_query","query":"{ hello }","use_get":true}),
        json!({"type":"graphql_query","query":"mutation M { addBook(title: \"Persuasion\") { id title year } }"}),
        json!({"type":"graphql_query","query":"{ secret }"}),
        json!({"type":"graphql_query","query":"{ nope }"}),
    ] {
        assert!(matches!(
            state
                .send_to_client(cid, a, Duration::from_secs(15))
                .await
                .unwrap(),
            ClientSendOutcome::Sent { .. }
        ));
    }
    let rows = logs(&state, owner, "graphql_response", 5).await;
    let r: Vec<&Value> = rows.iter().map(|r| &r.request).collect();
    assert_eq!(r[0]["data"], json!({"book": {"title": "Emma"}}));
    assert_eq!(r[0]["media_type"], "application/graphql-response+json");
    assert_eq!(r[1]["data"], json!({"hello": "Hello, world"}));
    assert_eq!(
        (r[2]["operation_name"].as_str(), &r[2]["data"]),
        (
            Some("M"),
            &json!({"addBook": {"id": "3", "title": "Persuasion", "year": null}})
        )
    );
    assert_eq!(r[3]["errors"][0]["extensions"]["code"], "FORBIDDEN");
    assert_eq!(
        (r[4]["status"].as_u64(), r[4].get("data")),
        (Some(400), None)
    );
    let seen = logs(
        &state,
        AccessLogOwner::Server(sid.as_u32()),
        "graphql_operation",
        4,
    )
    .await;
    assert_eq!(seen[1].request["method"], "GET");
    state.remove_client(cid).await;

    // A client pointed at something that is not GraphQL still connects (introspection is
    // advisory) and reports why, and every answer says it was not a GraphQL response.
    let cid = client_in(&state, addr.to_string(), json!({"endpoint": "/elsewhere"}))
        .await
        .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    let connected = logs(&state, owner, "graphql_connected", 1).await;
    assert!(connected[0].request["introspection_error"]
        .as_str()
        .unwrap()
        .contains("404"));
    state.remove_client(cid).await;
    state.remove_server(sid).await;
}
