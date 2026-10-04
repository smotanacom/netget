use hyper::HeaderMap;
use netget::{
    client::oci_registry::{api, OciRegistryClientProtocol},
    llm::actions::{client_trait::Client, protocol_trait::Protocol},
};
use serde_json::{json, Value};
fn h(mt: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("content-type", mt.parse().unwrap());
    h
}
fn req(operation: &str) -> api::Request {
    let action = match operation {
        "probe" | "catalog" => json!({"type":"oci_request","operation":operation}),
        "tags" => json!({"type":"oci_request","operation":operation,"repository":"library/demo"}),
        _ => {
            json!({"type":"oci_request","operation":operation,"repository":"library/demo","reference":"latest"})
        }
    };
    let api::Action::Request(r) = api::action(&action).unwrap() else {
        panic!()
    };
    r
}
fn nested(n: usize) -> Value {
    let mut v = Value::Null;
    for _ in 0..n {
        v = Value::Array(vec![v])
    }
    v
}
#[test]
fn actions_examples_and_all_event_privacy_definitions_are_valid() {
    let p = OciRegistryClientProtocol::new();
    for a in p.get_async_actions(&super::common::state()) {
        assert!(p.execute_action(a.example).is_ok(), "{}", a.name)
    }
    for e in p.get_event_types() {
        assert!(e.actions.iter().any(|a| a.name == "oci_authenticate"));
        assert!(e.actions.iter().any(|a| a.name == "oci_set_token"));
    }
}
#[test]
fn action_and_json_budgets_have_exact_boundaries_and_owned_deep_refusal() {
    assert!(api::within_budget(&nested(api::MAX_DEPTH)));
    assert!(!api::within_budget(&nested(api::MAX_DEPTH + 1)));
    assert!(api::within_budget(&Value::Array(vec![
        Value::Null;
        api::MAX_NODES - 1
    ])));
    assert!(!api::within_budget(&Value::Array(vec![
        Value::Null;
        api::MAX_NODES
    ])));
    let n = api::MAX_RETAINED - std::mem::size_of::<Value>();
    assert!(api::within_budget(&Value::String("x".repeat(n))));
    assert!(!api::within_budget(&Value::String("x".repeat(n + 1))));
    assert!(OciRegistryClientProtocol::new()
        .execute_action(nested(10000))
        .is_err());
    for n in [32, 33] {
        let s = format!("{}null{}", "[".repeat(n), "]".repeat(n));
        assert_eq!(api::json_body(s.as_bytes()).is_ok(), n == 32);
    }
    assert!(api::json_body(b"{\"x\":1,\"x\":2}").is_err());
    assert!(api::json_body(b"{} {}").is_err());
}
#[test]
fn repository_tags_digests_and_action_fields_are_strict() {
    for s in ["a__b", "a--b", "library/demo"] {
        api::repository(s).unwrap();
    }
    for s in ["a..b", "a___b", "a._b", "A/b"] {
        assert!(api::repository(s).is_err());
    }
    api::tag("_tag").unwrap();
    api::tag(&"x".repeat(128)).unwrap();
    assert!(api::tag(&"x".repeat(129)).is_err());
    for s in ["x/y", "-tag", ""] {
        assert!(api::tag(s).is_err());
    }
    for v in [
        json!({"type":"oci_request","operation":"push"}),
        json!({"type":"oci_request","operation":"probe","repository":"x"}),
        json!({"type":"oci_authenticate","password":"private"}),
        json!({"type":"oci_request","operation":"tags","repository":"x","n":0}),
        json!({"type":"oci_request","operation":"tags","repository":"x","n":1001}),
    ] {
        assert!(api::action(&v).is_err());
    }
    api::token(&"x".repeat(api::MAX_TOKEN)).unwrap();
    assert!(api::token(&"x".repeat(api::MAX_TOKEN + 1)).is_err());
    assert!(api::token("bad\r\nheader").is_err());
}
#[test]
fn native_raw_manifest_digest_media_and_descriptors_are_verified() {
    let r = req("manifest");
    let d = netget::server::oci_registry::actions::sha256_digest(b"{}");
    let doc = json!({"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"digest":d,"size":2,"mediaType":"application/vnd.oci.image.config.v1+json"},"layers":[]});
    let bytes = serde_json::to_vec(&doc).unwrap();
    let mut headers = h("application/vnd.oci.image.manifest.v1+json");
    let api::Outcome::Result(v) = api::response(&r, 200, &headers, &bytes).unwrap() else {
        panic!()
    };
    assert_eq!(v["data"]["digest_verified"], true);
    headers.insert("docker-content-digest", d.parse().unwrap());
    assert!(api::response(&r, 200, &headers, &bytes).is_err());
    assert!(api::response(&r, 200, &h("application/json"), &bytes).is_err());
    let mut bad = doc.clone();
    bad["config"]["size"] = json!(-1);
    assert!(api::response(
        &r,
        200,
        &h("application/vnd.oci.image.manifest.v1+json"),
        &serde_json::to_vec(&bad).unwrap()
    )
    .is_err());
    bad = doc;
    bad["config"]["data"] = json!("e30=");
    assert!(api::response(
        &r,
        200,
        &h("application/vnd.oci.image.manifest.v1+json"),
        &serde_json::to_vec(&bad).unwrap()
    )
    .is_err());
}
#[test]
fn head_is_metadata_only_and_blob_is_hashed_before_exposing_text() {
    let body = b"native text";
    let d = netget::server::oci_registry::actions::sha256_digest(body);
    let api::Action::Request(mut r)=api::action(&json!({"type":"oci_request","operation":"blob","repository":"x","reference":d,"expected_size":body.len()})).unwrap()else{panic!()};
    let api::Outcome::Result(v) =
        api::response(&r, 200, &h("application/octet-stream"), body).unwrap()
    else {
        panic!()
    };
    assert_eq!(v["data"]["text"], "native text");
    assert!(api::response(&r, 200, &h("application/octet-stream"), b"bad").is_err());
    r.expected_size = Some(99);
    assert!(api::response(&r, 200, &h("application/octet-stream"), body).is_err());
    r.operation = "blob_head";
    r.method = "HEAD";
    let mut headers = h("application/octet-stream");
    headers.insert("content-length", "9876543210".parse().unwrap());
    let api::Outcome::Result(v) = api::response(&r, 200, &headers, b"").unwrap() else {
        panic!()
    };
    assert_eq!(v["data"]["digest_verified"], false);
    assert_eq!(v["data"]["size"], 9876543210u64);
}
#[test]
fn pagination_stays_relative_on_same_route_with_explicit_cursor() {
    let r = req("tags");
    let body = br#"{"name":"library/demo","tags":["a","b"]}"#;
    let mut headers = h("application/json");
    headers.insert(
        "link",
        "</v2/library/demo/tags/list?n=100&last=b>; rel=\"next\""
            .parse()
            .unwrap(),
    );
    let api::Outcome::Result(v) = api::response(&r, 200, &headers, body).unwrap() else {
        panic!()
    };
    assert_eq!(v["data"]["next_last"], "b");
    for link in [
        "<https://evil.example/v2/library/demo/tags/list?n=100&last=b>; rel=\"next\"",
        "</v2/other/tags/list?n=100&last=b>; rel=\"next\"",
        "</v2/library/demo/tags/list?n=999&last=b>; rel=\"next\"",
        "</v2/library/demo/tags/list?n=100&last=a>; rel=\"next\"",
        "<http://netget.invalid/v2/library/demo/tags/list?n=100&last=b>; rel=\"next\"",
        "<//netget.invalid/v2/library/demo/tags/list?n=100&last=b>; rel=\"next\"",
    ] {
        headers.insert("link", link.parse().unwrap());
        assert!(api::response(&r, 200, &headers, body).is_err());
    }
}
#[test]
fn pagination_full_pages_without_link_require_explicit_continuation_and_order() {
    let api::Action::Request(r) = api::action(
        &json!({"type":"oci_request","operation":"tags","repository":"library/demo","n":2}),
    )
    .unwrap() else {
        panic!()
    };
    let headers = h("text/plain; charset=utf-8");
    let api::Outcome::Result(v) = api::response(
        &r,
        200,
        &headers,
        br#"{"name":"library/demo","tags":["a","b"]}"#,
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(v["data"]["next_last"], "b");
    assert_eq!(v["data"]["pagination_complete"], false);
    assert_eq!(v["data"]["continuation_source"], "count_fallback");
    let mut link = headers.clone();
    link.insert(
        "link",
        "</v2/library/demo/tags/list?last=b>; rel=next"
            .parse()
            .unwrap(),
    );
    let api::Outcome::Result(v) = api::response(
        &r,
        200,
        &link,
        br#"{"name":"library/demo","tags":["a","b"]}"#,
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(v["data"]["continuation_source"], "link");
    for body in [
        br#"{"name":"library/demo","tags":["b","a"]}"#.as_slice(),
        br#"{"name":"library/demo","tags":["a","a"]}"#,
    ] {
        assert!(api::response(&r, 200, &headers, body).is_err());
    }
    let api::Action::Request(r) = api::action(&json!({"type":"oci_request","operation":"tags","repository":"library/demo","n":2,"last":"b"})).unwrap() else {panic!()};
    assert!(api::response(
        &r,
        200,
        &headers,
        br#"{"name":"library/demo","tags":["b"]}"#
    )
    .is_err());
    let api::Outcome::Result(v) = api::response(
        &r,
        200,
        &headers,
        br#"{"name":"library/demo","tags":["c"]}"#,
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(v["data"]["pagination_complete"], true);
    assert!(v["data"]["next_last"].is_null());
}
#[test]
fn raw_body_json_container_text_key_header_and_manifest_caps_are_exact() {
    for nodes in [api::MAX_NODES, api::MAX_NODES + 1] {
        let mut arrays = vec![Value::Array(vec![Value::Null; 1000]); 65];
        arrays.push(Value::Array(vec![Value::Null; nodes - 65067]));
        assert_eq!(
            api::json_body(&serde_json::to_vec(&Value::Array(arrays)).unwrap()).is_ok(),
            nodes == api::MAX_NODES
        );
    }
    for n in [api::MAX_TEXT, api::MAX_TEXT + 1] {
        let bytes = serde_json::to_vec(&"x".repeat(n)).unwrap();
        assert_eq!(api::json_body(&bytes).is_ok(), n == api::MAX_TEXT);
    }
    for n in [1000, 1001] {
        assert_eq!(
            api::json_body(&serde_json::to_vec(&vec![Value::Null; n]).unwrap()).is_ok(),
            n == 1000
        );
    }
    for n in [256, 257] {
        let v: serde_json::Map<String, Value> =
            (0..n).map(|i| (i.to_string(), Value::Null)).collect();
        assert_eq!(
            api::json_body(&serde_json::to_vec(&v).unwrap()).is_ok(),
            n == 256
        );
        let v = Value::Object(serde_json::Map::from_iter([("x".repeat(n), Value::Null)]));
        assert_eq!(
            api::json_body(&serde_json::to_vec(&v).unwrap()).is_ok(),
            n == 256
        );
    }
    for n in [api::MAX_BODY, api::MAX_BODY + 1] {
        let mut body = vec![b' '; n];
        body[0] = b'0';
        assert_eq!(api::json_body(&body).is_ok(), n == api::MAX_BODY);
    }
    let mut headers = HeaderMap::new();
    for i in 0..64 {
        headers.insert(
            format!("x-{i}")
                .parse::<hyper::header::HeaderName>()
                .unwrap(),
            "v".parse().unwrap(),
        );
    }
    api::headers(&headers).unwrap();
    headers.insert("x-65", "v".parse().unwrap());
    assert!(api::headers(&headers).is_err());
    for n in [8192, 8193] {
        let mut headers = HeaderMap::new();
        headers.insert("x", "v".repeat(n).parse().unwrap());
        assert_eq!(api::headers(&headers).is_ok(), n == 8192);
    }
    for n in [32768, 32769] {
        let mut headers = HeaderMap::new();
        for (i, len) in [8192, 8192, 8192, n - 3 * 8192 - 8].into_iter().enumerate() {
            headers.insert(
                format!("x{i}")
                    .parse::<hyper::header::HeaderName>()
                    .unwrap(),
                "v".repeat(len).parse().unwrap(),
            );
        }
        assert_eq!(api::headers(&headers).is_ok(), n == 32768);
    }
    let r = req("manifest");
    let d = netget::server::oci_registry::actions::sha256_digest(b"{}");
    let doc = json!({"schemaVersion":2,"config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":d,"size":2},"layers":[]});
    for n in [api::MAX_MANIFEST, api::MAX_MANIFEST + 1] {
        let mut body = serde_json::to_vec(&doc).unwrap();
        body.resize(n, b' ');
        assert_eq!(
            api::response(
                &r,
                200,
                &h("application/vnd.oci.image.manifest.v1+json"),
                &body
            )
            .is_ok(),
            n == api::MAX_MANIFEST
        );
    }
    for n in [
        api::MAX_TEXT,
        api::MAX_TEXT + 1,
        api::MAX_BODY,
        api::MAX_BODY + 1,
    ] {
        let body = vec![b'x'; n];
        let d = netget::server::oci_registry::actions::sha256_digest(&body);
        let api::Action::Request(r) = api::action(
            &json!({"type":"oci_request","operation":"blob","repository":"x","reference":d}),
        )
        .unwrap() else {
            panic!()
        };
        let parsed = api::response(&r, 200, &h("application/octet-stream"), &body);
        if n > api::MAX_BODY {
            assert!(parsed.is_err());
        } else {
            let api::Outcome::Result(v) = parsed.unwrap() else {
                panic!()
            };
            assert_eq!(v["data"]["digest_verified"], true);
            assert_eq!(v["data"]["content_omitted"], n > api::MAX_TEXT);
        }
    }
}
#[test]
fn bearer_challenge_and_origin_trust_require_selected_pull_permissions() {
    let r = req("tags");
    let mut headers = h("application/json");
    headers.insert("www-authenticate","Bearer realm=\"http://127.0.0.1:9000/token\",service=\"native\",scope=\"repository:library/demo:pull\"".parse().unwrap());
    let c = api::challenge(&headers, &r).unwrap();
    let registry = api::origin("http://127.0.0.1:9001").unwrap();
    assert!(api::trusted(&c, &registry, None).is_err());
    let trusted = api::origin("http://127.0.0.1:9000").unwrap();
    api::trusted(&c, &registry, Some(&trusted)).unwrap();
    assert!(c
        .token_url()
        .query_pairs()
        .any(|(k, v)| k == "scope" && v == "repository:library/demo:pull"));
    for challenge in [
        "Bearer realm=\"http://127.0.0.1:9000/token\",scope=\"repository:library/demo:pull,push\"",
        "Bearer realm=\"http://127.0.0.1:9000/token\",realm=\"http://evil.example\"",
        "Basic realm=\"registry\"",
        "Bearer realm=\"http://user:pass@127.0.0.1/token\"",
    ] {
        headers.insert("www-authenticate", challenge.parse().unwrap());
        assert!(api::challenge(&headers, &r).is_err());
    }
}
#[test]
fn token_aliases_expiry_and_reflections_preserve_fixed_schema() {
    let headers = h("application/json");
    let t = api::issued_token(
        200,
        &headers,
        br#"{"token":"native-token","access_token":"native-token","expires_in":120}"#,
    )
    .unwrap();
    assert_eq!(t.metadata["token_received"], true);
    assert_eq!(t.metadata["registry_authorization_verified"], false);
    for body in [
        br#"{"token":"a","access_token":"b"}"#.as_slice(),
        br#"{"token":"a","expires_in":0}"#,
        br#"{"token":"a","expires_in":86401}"#,
        br#"{"token":"a","issued_at":"2000-01-01T00:00:00Z"}"#,
    ] {
        assert!(api::issued_token(200, &headers, body).is_err());
    }
    let mut v = json!({"operation":"manifest","data":{"manifest":{"schemaVersion":2,"config":{"mediaType":"native","digest":"sha256:x","size":2,"annotations":{"config":"copied config"}}},"digest":"sha256:x"}});
    api::redact_payload(&mut v, &["config".into()]);
    assert!(v["data"]["manifest"].get("config").is_some());
    assert_eq!(
        v["data"]["manifest"]["config"]["annotations"]["<redacted>"],
        "<redacted>"
    );
    let secret = "private\n\"credential";
    for text in [
        secret.into(),
        serde_json::to_string(secret).unwrap(),
        format!("{secret:?}"),
    ] {
        let mut v = Value::String(text);
        api::redact(&mut v, &[secret.into()]);
        assert_eq!(v, "<redacted>");
    }
    let mut deep = nested(10000);
    api::redact(&mut deep, &[secret.into()]);
    // Safe disposal of a constructed deep value after the redactor's real guard.
    assert!(netget::utils::json_budget::within_budget(
        &deep,
        api::MAX_RETAINED,
        api::MAX_NODES,
        api::MAX_DEPTH + 9
    ));
}
