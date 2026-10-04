//! RESTCONF from NetGet's side: data-resource paths; NetGet's client against NetGet's server
//! (discovery, YANG library, an RFC-shaped list entry, POST 201 with Location, DELETE 204,
//! data-exists 409, a 404 error document); and raw HTTP: host-meta in XRD and JSON, media type
//! negotiation (406, 415), query parameters, OPTIONS, HEAD, the operations list, a refused
//! datastore replacement.
use crate::helpers::restconf::*;
use netget::server::restconf::path;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;

#[test]
fn paths() {
    let p = path::parse("example:car/tire=front%2Fleft,2/size").unwrap();
    assert_eq!(
        path::to_json(&p),
        json!([{"name": "car", "module": "example"}, {"name": "tire", "keys": ["front/left", "2"]}, {"name": "size"}])
    );
    assert_eq!(path::target_module(&p), Some("example"));
    assert!(path::parse("car:").is_ok());
    for bad in [
        "car",
        "example:car//x",
        "example:1car",
        "example:car/x%zz=1",
        "example:car/a:",
    ] {
        assert!(path::parse(bad).is_err(), "{bad}");
    }
    assert_eq!(path::encode_key("front/left 1"), "front%2Fleft%201");
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_client_against_netget_server() {
    let dir = tempfile::tempdir().unwrap();
    let state = state();
    let (sid, addr) = server_in(&state, policy(&dir.path().join("car.json"))).await;
    let cid = client_in(&state, addr.to_string()).await.unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    let c = &logs(&state, owner, "restconf_connected", 1).await[0];
    assert_eq!(
        (
            c["root_status"].as_u64(),
            c["yang_library_version"].as_str()
        ),
        (Some(200), Some("2019-01-04"))
    );
    assert_eq!(
        c["modules"],
        json!([{"name": "car", "revision": "2023-03-27", "namespace": "freeconf.org/car"}])
    );
    for a in [
        json!({"type": "restconf_get", "path": "car:tire=2"}),
        json!({"type": "restconf_post", "path": "car:", "data": {"car:tire": [{"pos": 7, "size": "R17"}]}}),
        json!({"type": "restconf_post", "path": "car:", "data": {"car:tire": [{"pos": 7}]}}),
        json!({"type": "restconf_delete", "path": "car:tire=7"}),
        json!({"type": "restconf_get", "path": "car:tire=7"}),
        json!({"type": "restconf_invoke", "operation": "car:addOil", "input": {"amount": 5}}),
    ] {
        let r = state
            .send_to_client(cid, a.clone(), Duration::from_secs(20))
            .await
            .unwrap();
        assert!(matches!(r, ClientSendOutcome::Sent { .. }), "{a}: {r:?}");
    }
    let r: Vec<Value> = logs(&state, owner, "restconf_response", 6).await;
    assert_eq!(
        r[0]["data"],
        json!({"car:tire": [{"pos": 2, "size": "H15", "worn": false, "wear": 100, "flat": false}]})
    );
    assert_eq!(
        (r[1]["status"].as_u64(), r[1]["location"].as_str()),
        (Some(201), Some("/restconf/data/car:"))
    );
    assert_eq!(
        (
            r[2]["status"].as_u64(),
            r[2]["errors"][0]["error-tag"].as_str()
        ),
        (Some(409), Some("data-exists"))
    );
    assert_eq!(r[3]["status"], 204);
    assert_eq!(
        (
            r[4]["status"].as_u64(),
            r[4]["errors"][0]["error-type"].as_str()
        ),
        (Some(404), Some("protocol"))
    );
    assert_eq!(r[5]["data"], json!({"car:output": {"oilLevel": 15}}));
    state.remove_client(cid).await;
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn raw_http() {
    let dir = tempfile::tempdir().unwrap();
    let state = state();
    let (sid, addr) = server_in(&state, policy(&dir.path().join("car.json"))).await;
    let http = reqwest::Client::new();
    let url = |p: &str| format!("http://{addr}{p}");
    let xrd = http
        .get(url("/.well-known/host-meta"))
        .send()
        .await
        .unwrap();
    assert_eq!(xrd.headers()["content-type"], "application/xrd+xml");
    assert!(xrd
        .text()
        .await
        .unwrap()
        .contains("<Link rel='restconf' href='/restconf'/>"));
    let jrd: Value = http
        .get(url("/.well-known/host-meta"))
        .header("accept", "application/json")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(jrd["links"][0]["href"], "/restconf");
    let status = |r: reqwest::Response| r.status().as_u16();
    assert_eq!(
        status(
            http.get(url("/restconf/data/car:"))
                .header("accept", "application/yang-data+xml")
                .send()
                .await
                .unwrap()
        ),
        406
    );
    assert_eq!(
        status(
            http.put(url("/restconf/data/car:speed"))
                .header("content-type", "text/plain")
                .body("1")
                .send()
                .await
                .unwrap()
        ),
        415
    );
    let bad_query = http
        .get(url("/restconf/data/car:?depth=0"))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_query.status().as_u16(), 400);
    let doc: Value = bad_query.json().await.unwrap();
    assert_eq!(
        doc["ietf-restconf:errors"]["error"][0]["error-tag"],
        "invalid-value"
    );
    assert_eq!(
        status(
            http.get(url("/restconf/data/nomodule"))
                .send()
                .await
                .unwrap()
        ),
        400
    );
    assert_eq!(
        status(
            http.put(url("/restconf/data"))
                .header("content-type", "application/yang-data+json")
                .body("{}")
                .send()
                .await
                .unwrap()
        ),
        405
    );
    let options = http
        .request(reqwest::Method::OPTIONS, url("/restconf/data/car:"))
        .send()
        .await
        .unwrap();
    assert!(options.headers()["allow"]
        .to_str()
        .unwrap()
        .contains("PATCH"));
    let head = http.head(url("/restconf/data/car:")).send().await.unwrap();
    assert_eq!(
        (
            head.status().as_u16(),
            head.headers()["content-type"].to_str().unwrap()
        ),
        (200, "application/yang-data+json")
    );
    assert!(head.bytes().await.unwrap().is_empty());
    let ops: Value = http
        .get(url("/restconf/operations"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        ops,
        json!({"ietf-restconf:operations": {"car:addOil": [null], "car:reset": [null]}})
    );
    let malformed = http
        .patch(url("/restconf/data/car:"))
        .header("content-type", "application/yang-data+json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status().as_u16(), 400);
    assert_eq!(
        status(http.get(url("/restconf/nothing")).send().await.unwrap()),
        404
    );
    state.remove_server(sid).await;
}
