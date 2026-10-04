//! NetGet's JMAP server from raw HTTP, over plain HTTP (`tls: false`): the session shape,
//! authentication (Basic, Bearer, none), RFC 8620 request-level problems (notJSON, notRequest,
//! unknownCapability, limit), method errors Rust decides (unknownMethod, accountNotFound,
//! requestTooLarge, invalidResultReference, invalidArguments), createdIds round trips, blob and
//! push endpoints refused, and a handler-less server failing closed with serverFail.
use crate::helpers::jmap::*;
use serde_json::{json, Value};

async fn post(addr: std::net::SocketAddr, auth: Option<&str>, body: &str) -> (u16, Value) {
    let mut req = reqwest::Client::new()
        .post(format!("http://{addr}/jmap/"))
        .header("content-type", "application/json")
        .body(body.to_owned());
    if let Some(a) = auth {
        req = req.header("authorization", a);
    }
    let r = req.send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
}

fn calls(calls: Value) -> String {
    json!({"using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"], "methodCalls": calls}).to_string()
}

const BASIC: &str = "Basic YWxpY2VAZXhhbXBsZS5jb206c2VjcmV0"; // alice@example.com:secret

#[tokio::test(flavor = "multi_thread")]
async fn requests_and_errors() {
    let state = state();
    let mut params = alice();
    params["tls"] = json!(false);
    params["api_tokens"] = json!({"tok-1": "alice@example.com"});
    let (sid, addr) = server_in(&state, params).await;

    let session: Value = reqwest::Client::new()
        .get(format!("http://{addr}/.well-known/jmap"))
        .header("authorization", "Bearer tok-1")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(session["apiUrl"], format!("http://{addr}/jmap/"));
    assert_eq!(
        session["primaryAccounts"]["urn:ietf:params:jmap:mail"],
        "a1"
    );
    assert_eq!(
        session["capabilities"]["urn:ietf:params:jmap:core"]["maxCallsInRequest"],
        16
    );
    assert_eq!(session["accounts"]["a1"]["name"], "alice@example.com");

    let unauth = reqwest::get(format!("http://{addr}/.well-known/jmap"))
        .await
        .unwrap();
    assert_eq!(unauth.status().as_u16(), 401);
    assert!(unauth.headers().get("www-authenticate").is_some());
    let (status, _) = post(addr, Some("Bearer nope"), &calls(json!([]))).await;
    assert_eq!(status, 401);

    let problem = |b: &Value| b["type"].as_str().unwrap_or_default().to_owned();
    let (s, b) = post(addr, Some(BASIC), "{not json").await;
    assert_eq!(
        (s, problem(&b)),
        (400, "urn:ietf:params:jmap:error:notJSON".into())
    );
    let (s, b) = post(addr, Some(BASIC), r#"{"using": []}"#).await;
    assert_eq!(
        (s, problem(&b)),
        (400, "urn:ietf:params:jmap:error:notRequest".into())
    );
    let (s, b) = post(
        addr,
        Some(BASIC),
        r#"{"using": ["urn:example:nope"], "methodCalls": []}"#,
    )
    .await;
    assert_eq!(
        (s, problem(&b)),
        (400, "urn:ietf:params:jmap:error:unknownCapability".into())
    );
    let many: Vec<Value> = (0..17)
        .map(|i| json!(["Core/echo", {}, i.to_string()]))
        .collect();
    let (s, b) = post(addr, Some(BASIC), &calls(json!(many))).await;
    assert_eq!(
        (s, problem(&b), b["limit"].clone()),
        (
            400,
            "urn:ietf:params:jmap:error:limit".into(),
            json!("maxCallsInRequest")
        )
    );
    let deep = format!("{}{}", "[".repeat(40), "]".repeat(40));
    let (s, b) = post(
        addr,
        Some(BASIC),
        &format!(r#"{{"using":[],"methodCalls":{deep}}}"#),
    )
    .await;
    assert_eq!(
        (s, problem(&b)),
        (400, "urn:ietf:params:jmap:error:notRequest".into())
    );

    let ids: Vec<String> = (0..257).map(|i| format!("e{i}")).collect();
    let (s, b) = post(
        addr,
        Some(BASIC),
        &json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": [
                ["Core/echo", {"x": 1}, "echo"],
                ["Foo/get", {"accountId": "a1"}, "unknown"],
                ["Identity/get", {"accountId": "a1"}, "not-using"],
                ["Mailbox/get", {"accountId": "zz"}, "account"],
                ["Mailbox/get", {}, "no-account"],
                ["Email/get", {"accountId": "a1", "ids": ids}, "too-large"],
                ["Email/get", {"accountId": "a1", "#ids": {"resultOf": "nowhere", "name": "Email/query", "path": "/ids"}}, "bad-ref"],
                ["Email/get", {"accountId": "a1", "ids": [], "#ids": {"resultOf": "echo", "name": "Core/echo", "path": "/x"}}, "both"],
                ["Email/set", {"accountId": "a1", "create": {"k1": {"subject": "x"}}}, "set"],
                ["Email/get", {"accountId": "a1", "ids": ["#k1", "#k0"]}, "by-creation-id"],
            ],
            "createdIds": {"k0": "e2"},
        })
        .to_string(),
    )
    .await;
    assert_eq!(s, 200, "{b}");
    let responses = b["methodResponses"].as_array().unwrap();
    let by_id = |id: &str| responses.iter().find(|r| r[2] == id).unwrap().clone();
    assert_eq!(by_id("echo"), json!(["Core/echo", {"x": 1}, "echo"]));
    for (id, kind) in [
        ("unknown", "unknownMethod"),
        ("not-using", "unknownMethod"),
        ("account", "accountNotFound"),
        ("no-account", "invalidArguments"),
        ("too-large", "requestTooLarge"),
        ("bad-ref", "invalidResultReference"),
        ("both", "invalidArguments"),
    ] {
        let r = by_id(id);
        assert_eq!(
            (r[0].clone(), r[1]["type"].clone()),
            (json!("error"), json!(kind)),
            "{id}: {r}"
        );
    }
    let got = by_id("by-creation-id");
    assert_eq!(got[0], "Email/get", "{got}");
    let names: Vec<&str> = got[1]["list"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["m-new", "e2"], "{got}");
    assert_eq!(got[1]["accountId"], "a1");
    assert_eq!(b["createdIds"], json!({"k0": "e2", "k1": "m-new"}));
    assert_eq!(b["sessionState"], session["state"]);

    for path in [
        "/jmap/upload/a1/",
        "/jmap/download/a1/b/n?accept=x",
        "/jmap/eventsource/?types=*",
    ] {
        let r = reqwest::Client::new()
            .get(format!("http://{addr}{path}"))
            .header("authorization", BASIC)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 501, "{path}");
    }
    let r = reqwest::Client::new()
        .get(format!("http://{addr}/jmap/"))
        .header("authorization", BASIC)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 405);
    state.remove_server(sid).await;

    // No handler and no model: every method fails closed with serverFail.
    let (sid, addr) = server_with(&state, None, json!({"tls": false})).await;
    let (s, b) = post(
        addr,
        None,
        &calls(json!([["Mailbox/get", {"accountId": "a1"}, "0"]])),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(b["methodResponses"][0][1]["type"], "serverFail", "{b}");
    state.remove_server(sid).await;
}
