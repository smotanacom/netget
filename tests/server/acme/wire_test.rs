//! NetGet's CA from raw JWS requests: every refusal Rust owns (content type, nonce, URL, alg,
//! signature, kid, contact, identifier, order state, CSR, ownership, size), http-01 checked
//! before the handler is asked, a CA with no handler answer creating nothing, and the NetGet
//! pair over HTTPS.
use crate::helpers::acme::*;
use netget::server::acme::jws::{self, AccountKey};
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;

struct Raw {
    http: reqwest::Client,
    base: String,
    key: AccountKey,
    kid: Option<String>,
}

struct Answer {
    status: u16,
    location: Option<String>,
    nonce: Option<String>,
    body: Value,
}

impl Raw {
    fn new(addr: std::net::SocketAddr) -> Self {
        Self {
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
            base: format!("http://{addr}"),
            key: AccountKey::generate().unwrap(),
            kid: None,
        }
    }
    async fn nonce(&self) -> String {
        let r = self
            .http
            .head(format!("{}/new-nonce", self.base))
            .send()
            .await
            .unwrap();
        r.headers()["replay-nonce"].to_str().unwrap().to_owned()
    }
    async fn send(&self, path: &str, body: Value) -> Answer {
        let r = self
            .http
            .post(format!("{}{path}", self.base))
            .header("content-type", "application/jose+json")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        let header = |h: &str| r.headers().get(h).map(|v| v.to_str().unwrap().to_owned());
        let (status, location, nonce) = (
            r.status().as_u16(),
            header("location"),
            header("replay-nonce"),
        );
        let text = r.text().await.unwrap();
        Answer {
            status,
            location,
            nonce,
            body: serde_json::from_str(&text).unwrap_or(Value::String(text)),
        }
    }
    /// A correctly signed request; `payload: None` is POST-as-GET.
    async fn post(&self, path: &str, payload: Option<Value>) -> Answer {
        let nonce = self.nonce().await;
        let body = self
            .key
            .sign(
                &format!("{}{path}", self.base),
                &nonce,
                self.kid.as_deref(),
                payload.as_ref(),
            )
            .unwrap();
        self.send(path, body).await
    }
    async fn register(&mut self) -> Answer {
        let a = self
            .post(
                "/new-account",
                Some(json!({"termsOfServiceAgreed": true, "contact": ["mailto:raw@example.test"]})),
            )
            .await;
        self.kid = a.location.clone();
        a
    }
    fn path(&self, url: &str) -> String {
        url.strip_prefix(&self.base).unwrap().to_owned()
    }
}

fn error_type(a: &Answer) -> &str {
    a.body["type"]
        .as_str()
        .unwrap_or_default()
        .trim_start_matches("urn:ietf:params:acme:error:")
}

#[tokio::test(flavor = "multi_thread")]
async fn rust_owns_every_protocol_refusal() {
    let state = state();
    let closed = free_port();
    let (sid, addr) = server_in(&state, policy(), json!({"http01_target": format!("127.0.0.1:{closed}"), "challenge_types": ["http-01", "dns-01"]})).await;
    let mut raw = Raw::new(addr);

    let dir: Value = raw
        .http
        .get(format!("{}/directory", raw.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(dir["newAccount"], format!("{}/new-account", raw.base));
    assert_eq!(dir["meta"]["externalAccountRequired"], false);
    let head = raw
        .http
        .head(format!("{}/new-nonce", raw.base))
        .send()
        .await
        .unwrap();
    assert_eq!(head.status(), 200);
    assert_eq!(head.headers()["cache-control"], "no-store");
    assert_eq!(
        raw.http
            .get(format!("{}/new-nonce", raw.base))
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    let roots = raw
        .http
        .get(format!("{}/roots/0", raw.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(roots.starts_with("-----BEGIN CERTIFICATE-----"));

    // Transport-level refusals.
    let plain = raw
        .http
        .post(format!("{}/new-account", raw.base))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(plain.status(), 415);
    let big = raw
        .http
        .post(format!("{}/new-account", raw.base))
        .header("content-type", "application/jose+json")
        .body(vec![b' '; 70 * 1024])
        .send()
        .await
        .unwrap();
    assert_eq!(big.status(), 413);

    // Nonces are single-use; the refusal carries a fresh one.
    let nonce = raw.nonce().await;
    let body = raw
        .key
        .sign(
            &format!("{}/new-account", raw.base),
            &nonce,
            None,
            Some(&json!({"termsOfServiceAgreed": true})),
        )
        .unwrap();
    assert_eq!(raw.send("/new-account", body.clone()).await.status, 201);
    let again = raw.send("/new-account", body).await;
    assert_eq!((again.status, error_type(&again)), (400, "badNonce"));
    assert!(again.nonce.is_some());

    // The signed URL must be the request URL.
    let nonce = raw.nonce().await;
    let body = raw
        .key
        .sign(
            &format!("{}/new-order", raw.base),
            &nonce,
            None,
            Some(&json!({})),
        )
        .unwrap();
    let a = raw.send("/new-account", body).await;
    assert_eq!((a.status, error_type(&a)), (403, "unauthorized"));

    // An unsupported algorithm names the supported ones; a forged signature is refused.
    let protected = jws::b64(json!({"alg": "HS256", "nonce": raw.nonce().await, "url": format!("{}/new-account", raw.base), "jwk": raw.key.jwk()}).to_string().as_bytes());
    let a = raw.send("/new-account", json!({"protected": protected, "payload": jws::b64(b"{}"), "signature": jws::b64(&[0; 32])})).await;
    assert_eq!((a.status, error_type(&a)), (400, "badSignatureAlgorithm"));
    assert!(a.body["algorithms"]
        .as_array()
        .unwrap()
        .contains(&json!("ES256")));
    let mut forged = raw
        .key
        .sign(
            &format!("{}/new-account", raw.base),
            &raw.nonce().await,
            None,
            Some(&json!({"termsOfServiceAgreed": true})),
        )
        .unwrap();
    forged["payload"] = json!(jws::b64(br#"{"termsOfServiceAgreed":false}"#));
    let a = raw.send("/new-account", forged).await;
    assert_eq!((a.status, error_type(&a)), (400, "malformed"), "{}", a.body);

    // Accounts: same key finds the same account; unknown kid; contacts.
    let reg = raw.register().await;
    assert_eq!(
        reg.status, 200,
        "the key registered above already has an account"
    );
    let mut stranger = Raw::new(addr);
    let a = stranger
        .post("/new-account", Some(json!({"onlyReturnExisting": true})))
        .await;
    assert_eq!((a.status, error_type(&a)), (400, "accountDoesNotExist"));
    let a = stranger
        .post(
            "/new-account",
            Some(json!({"contact": ["tel:+15555550100"]})),
        )
        .await;
    assert_eq!((a.status, error_type(&a)), (400, "unsupportedContact"));
    stranger.kid = Some(format!("{}/acct/nope", raw.base));
    let a = stranger
        .post(
            "/new-order",
            Some(json!({"identifiers": [{"type": "dns", "value": "a.example.test"}]})),
        )
        .await;
    assert_eq!((a.status, error_type(&a)), (400, "accountDoesNotExist"));

    // Identifiers and order fields.
    for (ids, expected) in [
        (
            json!([{"type": "ip", "value": "192.0.2.1"}]),
            "unsupportedIdentifier",
        ),
        (
            json!([{"type": "dns", "value": "192.0.2.1"}]),
            "rejectedIdentifier",
        ),
        (
            json!([{"type": "dns", "value": "bad_name.example.test"}]),
            "rejectedIdentifier",
        ),
        (
            json!([{"type": "dns", "value": "x.forbidden.test"}]),
            "rejectedIdentifier",
        ),
        (json!([]), "malformed"),
    ] {
        let a = raw
            .post("/new-order", Some(json!({"identifiers": ids})))
            .await;
        assert_eq!(error_type(&a), expected, "{ids}: {}", a.body);
    }
    let a = raw.post("/new-order", Some(json!({"identifiers": [{"type": "dns", "value": "a.example.test"}], "notAfter": "2030-01-01T00:00:00Z"}))).await;
    assert_eq!(error_type(&a), "malformed");

    // An order: not ready to finalize; another account may not read it.
    let o = raw.post("/new-order", Some(json!({"identifiers": [{"type": "dns", "value": "A.Example.Test"}, {"type": "dns", "value": "*.example.test"}]}))).await;
    assert_eq!(o.status, 201, "{}", o.body);
    assert_eq!(
        o.body["identifiers"],
        json!([{"type": "dns", "value": "a.example.test"}, {"type": "dns", "value": "*.example.test"}])
    );
    let order_path = raw.path(o.location.as_deref().unwrap());
    let finalize_path = raw.path(o.body["finalize"].as_str().unwrap());
    let a = raw
        .post(&finalize_path, Some(json!({"csr": jws::b64(b"x")})))
        .await;
    assert_eq!((a.status, error_type(&a)), (403, "orderNotReady"));
    let mut other = Raw::new(addr);
    other.register().await;
    let a = other.post(&order_path, None).await;
    assert_eq!((a.status, error_type(&a)), (403, "unauthorized"));

    // http-01 that Rust cannot fetch fails without asking the handler; dns-01 is the handler's.
    let authz: Vec<String> = o.body["authorizations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| raw.path(u.as_str().unwrap()))
        .collect();
    let plain_authz = raw.post(&authz[0], None).await.body;
    let wild_authz = raw.post(&authz[1], None).await.body;
    assert_eq!(wild_authz["wildcard"], true);
    assert_eq!(
        wild_authz["challenges"].as_array().unwrap().len(),
        1,
        "a wildcard is offered dns-01 only"
    );
    let http = plain_authz["challenges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["type"] == "http-01")
        .unwrap();
    let a = raw
        .post(&raw.path(http["url"].as_str().unwrap()), Some(json!({})))
        .await;
    assert_eq!(
        (
            a.status,
            a.body["status"].as_str(),
            a.body["error"]["type"].as_str()
        ),
        (
            200,
            Some("invalid"),
            Some("urn:ietf:params:acme:error:incorrectResponse")
        ),
        "{}",
        a.body
    );
    let a = raw.post(&order_path, None).await;
    assert_eq!(a.body["status"], "invalid");
    let owner = AccessLogOwner::Server(sid.as_u32());
    assert!(state
        .list_access_logs_for(Some(owner), None)
        .await
        .iter()
        .all(|e| e.event_type != "acme_validate"));

    // A fresh order, both names by dns-01 (the handler accepts), then a CSR for other names.
    let o = raw
        .post(
            "/new-order",
            Some(json!({"identifiers": [{"type": "dns", "value": "b.example.test"}]})),
        )
        .await;
    let order_path = raw.path(o.location.as_deref().unwrap());
    let authz = raw
        .post(
            &raw.path(o.body["authorizations"][0].as_str().unwrap()),
            None,
        )
        .await
        .body;
    let dns = authz["challenges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["type"] == "dns-01")
        .unwrap();
    let a = raw
        .post(&raw.path(dns["url"].as_str().unwrap()), Some(json!({})))
        .await;
    assert_eq!(a.body["status"], "valid", "{}", a.body);
    assert_eq!(raw.post(&order_path, None).await.body["status"], "ready");
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(vec!["c.example.test".to_owned()]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    let csr = params.serialize_request(&key).unwrap();
    let a = raw
        .post(
            &raw.path(o.body["finalize"].as_str().unwrap()),
            Some(json!({"csr": jws::b64(csr.der())})),
        )
        .await;
    assert_eq!((a.status, error_type(&a)), (400, "badCSR"), "{}", a.body);
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn no_handler_answer_creates_nothing() {
    let state = state();
    let (sid, addr) = server_in(&state, vec![], json!({})).await;
    let mut raw = Raw::new(addr);
    let a = raw.register().await;
    assert_eq!(
        (a.status, error_type(&a)),
        (500, "serverInternal"),
        "{}",
        a.body
    );
    assert!(a.location.is_none());
    let a = raw
        .post("/new-account", Some(json!({"onlyReturnExisting": true})))
        .await;
    assert_eq!(error_type(&a), "accountDoesNotExist");
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_client_against_netget_ca_over_https() {
    let state = state();
    let dir = tempfile::tempdir().unwrap();
    let (root, cert, key) = tls_files(dir.path());
    let responder = free_port();
    let (sid, addr) = server_in(&state, policy(), json!({"http01_target": format!("127.0.0.1:{responder}"), "challenge_types": ["http-01"], "tls_cert_file": cert, "tls_key_file": key})).await;
    let cid = client_in(
        &state,
        format!("localhost:{}", addr.port()),
        json!({"ca_file": root, "http01_listen": format!("127.0.0.1:{responder}")}),
    )
    .await
    .unwrap();
    let send = |a: Value| state.send_to_client(cid, a, Duration::from_secs(30));
    for a in [
        json!({"type": "acme_register", "contact": ["mailto:pair@example.test"], "agree_tos": true}),
        json!({"type": "acme_order", "identifiers": ["pair.example.test"]}),
        json!({"type": "acme_validate", "identifier": "pair.example.test", "challenge_type": "http-01"}),
        json!({"type": "acme_finalize"}),
        json!({"type": "acme_revoke", "reason": 1}),
        json!({"type": "acme_revoke", "reason": 5}),
    ] {
        assert!(matches!(
            send(a).await.unwrap(),
            ClientSendOutcome::Sent { .. }
        ));
    }
    let r: Vec<Value> = logs(
        &state,
        AccessLogOwner::Client(cid.as_u32()),
        "acme_response",
        6,
    )
    .await
    .into_iter()
    .map(|r| r.request)
    .collect();
    assert_eq!(r[0]["status"], 201);
    assert_eq!(r[2]["challenge_status"], "valid", "{}", r[2]);
    assert_eq!(r[3]["order_status"], "valid", "{}", r[3]);
    assert_eq!(
        r[3]["certificate"]
            .as_str()
            .unwrap()
            .matches("BEGIN CERTIFICATE")
            .count(),
        2
    );
    assert_eq!(
        r[4]["problem"]["detail"],
        "keyCompromise revocations go through the operator"
    );
    assert_eq!(r[5]["status"], 200);
    let v = logs(
        &state,
        AccessLogOwner::Server(sid.as_u32()),
        "acme_validate",
        1,
    )
    .await;
    assert_eq!(v[0].request["verified"], true);
    state.remove_client(cid).await;
    state.remove_server(sid).await;
}
