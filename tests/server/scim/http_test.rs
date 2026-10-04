//! SCIM rules without peers: the filter grammar and its evaluation, PATCH paths, projection,
//! discovery documents, request refusals, paging, bearer auth, fail-closed answers, and the
//! NetGet client/service pair.
use crate::helpers::scim::*;
use netget::server::scim::{query, schema};
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;

#[test]
fn filters_parse_and_evaluate_per_rfc_7644() {
    let user = schema::by_name("User").unwrap();
    let r = json!({"id": "1", "userName": "BJensen", "name": {"givenName": "Barbara"}, "active": true,
        "emails": [{"value": "bj@example.com", "type": "work"}, {"value": "babs@home.example", "type": "home"}],
        "meta": {"lastModified": "2026-01-02T00:00:00Z"},
        "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User": {"employeeNumber": "701984"}});
    let yes = |f: &str| query::matches(&query::parse_filter(f).unwrap(), user, &r);
    assert!(yes("userName eq \"bjensen\""), "userName is not caseExact");
    assert!(!yes("id eq \"1 \"") && yes("id eq \"1\""));
    assert!(yes("name.givenName sw \"bar\" and active eq true"));
    assert!(
        yes("emails co \"home\""),
        "a multi-valued attribute compares its values"
    );
    assert!(yes("emails[type eq \"work\" and value ew \".com\"]"));
    assert!(!yes("emails[type eq \"work\" and value ew \".example\"]"));
    assert!(yes(
        "not (userName eq \"x\") and (title pr or active eq true)"
    ));
    assert!(yes("meta.lastModified gt \"2025-12-31T00:00:00Z\""));
    assert!(yes(
        "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User:employeeNumber eq \"701984\""
    ));
    assert!(yes(
        "urn:ietf:params:scim:schemas:core:2.0:User:userName pr"
    ));
    assert!(yes("title ne \"boss\""), "ne on an absent attribute");
    for bad in [
        "",
        "userName",
        "userName eq",
        "userName xx \"a\"",
        "(userName pr",
        "emails[type eq \"w\"",
        "a[b[c pr]]",
        "userName eq bjensen",
        "1abc pr",
    ] {
        assert!(query::parse_filter(bad).is_err(), "{bad:?}");
    }
    let deep = format!("{}userName pr{}", "(".repeat(40), ")".repeat(40));
    assert!(query::parse_filter(&deep).is_err(), "nesting is bounded");
    let p = query::PatchPath::parse("emails[type eq \"work\"].value").unwrap();
    assert_eq!(
        (p.path.attr.as_str(), p.sub_after_filter.as_deref()),
        ("emails", Some("value"))
    );
    assert!(query::PatchPath::parse("emails[type eq \"work\"]x").is_err());
    let projected = query::project(
        &json!({"id": "1", "schemas": [schema::USER], "userName": "u", "password": "p", "name": {"givenName": "G", "familyName": "F"}}),
        user,
        &[query::AttrPath::parse("name.givenName").unwrap()],
        &[],
    );
    assert_eq!(
        projected,
        json!({"id": "1", "schemas": [schema::USER], "name": {"givenName": "G"}})
    );
    let excluded = query::project(
        &json!({"id": "1", "userName": "u", "password": "p", "title": "t"}),
        user,
        &[],
        &[
            query::AttrPath::parse("title").unwrap(),
            query::AttrPath::parse("id").unwrap(),
        ],
    );
    assert_eq!(
        excluded,
        json!({"id": "1", "userName": "u"}),
        "password is never returned; id always is"
    );
    let mut list = vec![
        json!({"userName": "b"}),
        json!({}),
        json!({"userName": "A"}),
    ];
    query::sort(
        &mut list,
        user,
        &query::AttrPath::parse("userName").unwrap(),
        false,
    );
    assert_eq!(
        list,
        vec![
            json!({"userName": "A"}),
            json!({"userName": "b"}),
            json!({})
        ]
    );
}

async fn go(
    base: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (u16, Value) {
    let mut r = reqwest::Client::new().request(method.parse().unwrap(), format!("{base}{path}"));
    if let Some(t) = token {
        r = r.header("Authorization", format!("Bearer {t}"));
    }
    if let Some(b) = body {
        r = r
            .header("Content-Type", "application/scim+json")
            .body(b.to_string());
    }
    let resp = r.send().await.unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread")]
async fn discovery_refusals_paging_auth_and_fail_closed() {
    let state = state();
    let dir = tempfile::tempdir().unwrap();
    let (sid, addr) = server_in(
        &state,
        store_policy(&dir.path().join("db.json")),
        json!({"bearer_token": "tok"}),
    )
    .await;
    let base = format!("http://{addr}/scim/v2");
    let t = Some("tok");
    assert_eq!(go(&base, "GET", "/Users", None, None).await.0, 401);
    assert_eq!(go(&base, "GET", "/Users", Some("nope"), None).await.0, 401);
    let (s, spc) = go(&base, "GET", "/ServiceProviderConfig", t, None).await;
    assert_eq!(
        (
            s,
            spc["patch"]["supported"].as_bool(),
            spc["bulk"]["supported"].as_bool()
        ),
        (200, Some(true), Some(false))
    );
    assert_eq!(spc["authenticationSchemes"][0]["type"], "oauthbearertoken");
    let (_, schemas) = go(&base, "GET", "/Schemas", t, None).await;
    assert_eq!(schemas["totalResults"], 6);
    let (s, user_schema) = go(
        &base,
        "GET",
        "/Schemas/urn:ietf:params:scim:schemas:core:2.0:User",
        t,
        None,
    )
    .await;
    assert_eq!((s, user_schema["name"].as_str()), (200, Some("User")));
    let (_, rts) = go(&base, "GET", "/ResourceTypes", t, None).await;
    assert_eq!(
        rts["Resources"][0]["schemaExtensions"][0]["schema"],
        schema::ENTERPRISE
    );
    assert_eq!(
        go(&base, "POST", "/Schemas", t, Some(json!({}))).await.0,
        405
    );
    let (s, e) = go(&base, "GET", "/Users?filter=userName%20eq", t, None).await;
    assert_eq!(
        (s, e["scimType"].as_str(), e["status"].as_str()),
        (400, Some("invalidFilter"), Some("400"))
    );
    let (s, e) = go(&base, "POST", "/Users", t, Some(json!({"userName": "x"}))).await;
    assert_eq!(
        (s, e["scimType"].as_str()),
        (400, Some("invalidSyntax")),
        "schemas is required"
    );
    for i in 0..5 {
        let (s, _) = go(
            &base,
            "POST",
            "/Users",
            t,
            Some(
                json!({"schemas": [schema::USER], "userName": format!("user{i}"), "id": "ignored"}),
            ),
        )
        .await;
        assert_eq!(s, 201);
    }
    let (_, page) = go(
        &base,
        "GET",
        "/Users?sortBy=userName&startIndex=2&count=2&attributes=userName",
        t,
        None,
    )
    .await;
    assert_eq!(
        (
            page["totalResults"].as_u64(),
            page["startIndex"].as_u64(),
            page["itemsPerPage"].as_u64()
        ),
        (Some(5), Some(2), Some(2))
    );
    assert_eq!(page["Resources"][0]["userName"], "user1");
    assert!(page["Resources"][0].get("meta").is_none());
    let (_, all) = go(&base, "POST", "/.search", t, Some(json!({"schemas": [schema::SEARCH_REQUEST], "filter": "userName sw \"user\"", "count": 0}))).await;
    assert_eq!(
        (
            all["totalResults"].as_u64(),
            all["Resources"].as_array().map(Vec::len)
        ),
        (Some(5), Some(0))
    );
    let id = page["Resources"][0]["id"].as_str().unwrap().to_owned();
    let (s, e) = go(
        &base,
        "PATCH",
        &format!("/Users/{id}"),
        t,
        Some(json!({"schemas": [schema::PATCH_OP], "Operations": [{"op": "remove"}]})),
    )
    .await;
    assert_eq!((s, e["scimType"].as_str()), (400, Some("noTarget")));
    let (s, e) = go(&base, "PATCH", &format!("/Users/{id}"), t, Some(json!({"schemas": [schema::PATCH_OP], "Operations": [{"op": "replace", "path": "emails[type eq", "value": 1}]}))).await;
    assert_eq!((s, e["scimType"].as_str()), (400, Some("invalidPath")));
    let (s, u) = go(&base, "PATCH", &format!("/Users/{id}"), t, Some(json!({"schemas": [schema::PATCH_OP], "Operations": [{"op": "add", "path": "emails", "value": [{"value": "x@y", "type": "work"}]}, {"op": "replace", "path": "emails[type eq \"work\"].value", "value": "z@y"}]}))).await;
    assert_eq!((s, u["emails"][0]["value"].as_str()), (200, Some("z@y")));
    assert_eq!(go(&base, "GET", "/Bulk", t, None).await.0, 501);
    assert_eq!(go(&base, "GET", "/Users/nope", t, None).await.0, 404);
    let seen = logs(
        &state,
        AccessLogOwner::Server(sid.as_u32()),
        "scim_request",
        3,
    )
    .await;
    let patch = seen
        .iter()
        .find(|r| r.request["operation"] == "patch")
        .unwrap();
    assert_eq!(
        patch.request["operations"][1]["parsed_path"]["sub_attribute_after_filter"],
        "value"
    );
    state.remove_server(sid).await;

    let state = crate::helpers::scim::state();
    let (sid, addr) = server_in(&state, vec![], json!({})).await;
    let (s, e) = go(
        &format!("http://{addr}/scim/v2"),
        "GET",
        "/Users",
        None,
        None,
    )
    .await;
    assert_eq!(s, 500, "no handler, no model: {e}");
    let (s, _) = go(
        &format!("http://{addr}/scim/v2"),
        "GET",
        "/ServiceProviderConfig",
        None,
        None,
    )
    .await;
    assert_eq!(s, 200, "discovery needs no handler");
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_client_and_service_agree() {
    let state = state();
    let dir = tempfile::tempdir().unwrap();
    let (sid, addr) = server_in(&state, store_policy(&dir.path().join("db.json")), json!({})).await;
    let cid = client_in(&state, addr.to_string(), json!({"scheme": "http"}))
        .await
        .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    let connected = logs(&state, owner, "scim_connected", 1).await;
    assert_eq!(connected[0].request["features"]["patch"], true);
    let send = |a: Value| state.send_to_client(cid, a, Duration::from_secs(15));
    assert!(matches!(send(json!({"type":"scim_create","resource_type":"Users","resource":{"userName":"pair","emails":[{"value":"p@x","type":"work"}]}})).await.unwrap(), ClientSendOutcome::Sent { .. }));
    let created = logs(&state, owner, "scim_response", 1).await;
    let id = created[0].request["resource"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    for a in [
        json!({"type":"scim_list","resource_type":"Users","filter":"emails[type eq \"work\"]","attributes":"userName"}),
        json!({"type":"scim_patch","resource_type":"Users","id":id,"operations":[{"op":"replace","path":"active","value":false}]}),
        json!({"type":"scim_get","resource_type":"Users","id":id}),
        json!({"type":"scim_replace","resource_type":"Users","id":id,"resource":{"userName":"pair2"}}),
        json!({"type":"scim_create","resource_type":"Users","resource":{"userName":"PAIR2"}}),
        json!({"type":"scim_delete","resource_type":"Users","id":id}),
        json!({"type":"scim_get","resource_type":"Users","id":id}),
    ] {
        assert!(matches!(
            send(a).await.unwrap(),
            ClientSendOutcome::Sent { .. }
        ));
    }
    let rows = logs(&state, owner, "scim_response", 8).await;
    let r: Vec<&Value> = rows.iter().map(|r| &r.request).collect();
    assert_eq!(
        (r[0]["status"].as_u64(), r[0]["operation"].as_str()),
        (Some(201), Some("create"))
    );
    assert_eq!(
        (
            r[1]["total_results"].as_u64(),
            r[1]["resources"][0].get("emails").is_none()
        ),
        (Some(1), true)
    );
    assert_eq!(r[2]["resource"]["active"], false);
    assert_eq!(r[3]["resource"]["active"], false);
    assert_eq!(r[4]["resource"]["userName"], "pair2");
    assert_eq!(
        (r[5]["status"].as_u64(), r[5]["error"]["scim_type"].as_str()),
        (Some(409), Some("uniqueness"))
    );
    assert_eq!(r[6]["status"], 204);
    assert_eq!(r[7]["error"]["status"], 404);
    for bad in [
        json!({"type":"scim_list","resource_type":"Users","filter":"userName eq"}),
        json!({"type":"scim_patch","resource_type":"Users","id":"x","operations":[{"op":"remove"}]}),
        json!({"type":"scim_get","resource_type":"../x","id":"1"}),
    ] {
        assert!(matches!(
            send(bad).await.unwrap(),
            ClientSendOutcome::Rejected { .. }
        ));
    }
    state.remove_client(cid).await;
    state.remove_server(sid).await;
}
