//! Independent ACME clients, unchanged, against NetGet's CA (policy script in
//! `tests/helpers/acme.rs`): lego 4.35.2 (Go; ES256 account) orders two names over http-01,
//! which Rust fetches from lego's own solver, lists and revokes, and is refused an order the
//! policy rejects, all over HTTPS (lego requires it); certbot 5.8.0 (Python acme; RSA
//! account, RS256), over plain HTTP, answers http-01 with its standalone server and dns-01 for a wildcard through a manual hook, revokes, and is refused an
//! account the policy rejects. Fails, never skips.
use crate::helpers::acme::*;
use netget::state::AccessLogOwner;
use serde_json::{json, Value};

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn lego_orders_validates_lists_and_revokes() {
    let lego = peer("NETGET_ACME_LEGO");
    let state = state();
    let solver = free_port();
    let dir = tempfile::tempdir().unwrap();
    let (root, cert, key) = tls_files(dir.path());
    let (sid, addr) = server_in(&state, policy(), json!({"http01_target": format!("127.0.0.1:{solver}"), "validity_days": 30, "tls_cert_file": cert, "tls_key_file": key})).await;
    // lego refuses a plain-HTTP directory, so this CA serves HTTPS with a certificate lego trusts.
    let env = [("LEGO_CA_CERTIFICATES", root.as_str())];
    let path = dir.path().join("lego").display().to_string();
    let base = |more: &[&str]| {
        let mut a = s(&[
            "--server",
            &format!("https://localhost:{}/directory", addr.port()),
            "--email",
            "ops@example.test",
            "--accept-tos",
            "--path",
            &path,
            "--http",
            "--http.port",
            &format!("127.0.0.1:{solver}"),
        ]);
        a.extend(s(more));
        a
    };
    let (ok, out) = run(
        &lego,
        &base(&[
            "--domains",
            "www.example.test",
            "--domains",
            "api.example.test",
            "run",
        ]),
        &env,
    )
    .await;
    assert!(ok, "lego run failed:\n{out}");
    let chain =
        std::fs::read_to_string(dir.path().join("lego/certificates/www.example.test.crt")).unwrap();
    assert_eq!(
        chain.matches("-----BEGIN CERTIFICATE-----").count(),
        2,
        "leaf and CA:\n{chain}"
    );
    let (ok, listed) = run(&lego, &s(&["--path", &path, "list"]), &env).await;
    assert!(
        ok && listed.contains("www.example.test") && listed.contains("api.example.test"),
        "{listed}"
    );

    let owner = AccessLogOwner::Server(sid.as_u32());
    let validations = logs(&state, owner, "acme_validate", 2).await;
    for v in &validations {
        assert_eq!(v.request["challenge_type"], "http-01");
        assert_eq!(
            v.request["verified"], true,
            "Rust fetched the key authorization from lego: {}",
            v.request
        );
    }
    let accounts = logs(&state, owner, "acme_new_account", 1).await;
    assert_eq!(accounts[0].request["key_type"], "P-256");
    assert_eq!(
        accounts[0].request["contact"],
        json!(["mailto:ops@example.test"])
    );
    let finalized = logs(&state, owner, "acme_finalize", 1).await;
    let mut names: Vec<&str> = finalized[0].request["identifiers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["api.example.test", "www.example.test"]);

    let (ok, out) = run(
        &lego,
        &base(&["--domains", "www.example.test", "revoke", "--reason", "4"]),
        &env,
    )
    .await;
    assert!(ok, "lego revoke failed:\n{out}");
    let revoked = logs(&state, owner, "acme_revoke", 1).await;
    assert_eq!(revoked[0].request["reason"], 4);
    assert_eq!(revoked[0].request["serial"].as_str().unwrap().len(), 32);

    let (ok, out) = run(
        &lego,
        &base(&["--domains", "x.forbidden.test", "run"]),
        &env,
    )
    .await;
    assert!(
        !ok && out.contains("rejectedIdentifier") && out.contains("forbidden.test are not issued"),
        "{out}"
    );
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn certbot_http01_dns01_wildcard_revoke_and_refused_account() {
    let certbot = peer("NETGET_ACME_CERTBOT");
    let state = state();
    let standalone = free_port();
    let (sid, addr) = server_in(
        &state,
        policy(),
        json!({"http01_target": format!("127.0.0.1:{standalone}")}),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().display().to_string();
    let common = |email: &str| {
        s(&[
            "--non-interactive",
            "--server",
            &format!("http://{addr}/directory"),
            "--config-dir",
            &format!("{d}/config"),
            "--work-dir",
            &format!("{d}/work"),
            "--logs-dir",
            &format!("{d}/logs"),
            "--agree-tos",
            "-m",
            email,
        ])
    };

    let mut args = s(&[
        "certonly",
        "--standalone",
        "--preferred-challenges",
        "http",
        "--http-01-address",
        "127.0.0.1",
        "--http-01-port",
        &standalone.to_string(),
        "-d",
        "certbot.example.test",
    ]);
    args.extend(common("certbot@example.test"));
    let (ok, out) = run(&certbot, &args, &[]).await;
    assert!(ok, "certbot standalone failed:\n{out}");
    let chain = std::fs::read_to_string(
        dir.path()
            .join("config/live/certbot.example.test/fullchain.pem"),
    )
    .unwrap();
    assert_eq!(chain.matches("-----BEGIN CERTIFICATE-----").count(), 2);

    let mut args = s(&[
        "certonly",
        "--manual",
        "--preferred-challenges",
        "dns",
        "--manual-auth-hook",
        "/usr/bin/true",
        "-d",
        "*.example.test",
        "-d",
        "example.test",
        "--cert-name",
        "wild",
    ]);
    args.extend(common("certbot@example.test"));
    let (ok, out) = run(&certbot, &args, &[]).await;
    assert!(ok, "certbot manual dns-01 failed:\n{out}");

    let mut args = s(&[
        "revoke",
        "--cert-name",
        "certbot.example.test",
        "--reason",
        "superseded",
        "--no-delete-after-revoke",
    ]);
    args.extend(common("certbot@example.test"));
    let (ok, out) = run(&certbot, &args, &[]).await;
    assert!(ok, "certbot revoke failed:\n{out}");

    let owner = AccessLogOwner::Server(sid.as_u32());
    let accounts = logs(&state, owner, "acme_new_account", 1).await;
    assert_eq!(
        accounts[0].request["key_type"], "RSA",
        "certbot signs with RS256"
    );
    let v: Vec<Value> = logs(&state, owner, "acme_validate", 3)
        .await
        .into_iter()
        .map(|r| r.request)
        .collect();
    assert_eq!(
        (v[0]["challenge_type"].as_str(), v[0]["verified"].as_bool()),
        (Some("http-01"), Some(true))
    );
    for dns in &v[1..] {
        assert_eq!(dns["challenge_type"], "dns-01");
        assert!(dns["verified"].is_null(), "Rust cannot check dns-01: {dns}");
    }
    let orders = logs(&state, owner, "acme_new_order", 2).await;
    let mut wild: Vec<&str> = orders[1].request["identifiers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    wild.sort();
    assert_eq!(wild, ["*.example.test", "example.test"]);
    assert_eq!(
        logs(&state, owner, "acme_revoke", 1).await[0].request["reason"],
        4
    );

    let blocked = tempfile::tempdir().unwrap();
    let bd = blocked.path().display().to_string();
    let args = s(&[
        "register",
        "--non-interactive",
        "--server",
        &format!("http://{addr}/directory"),
        "--config-dir",
        &format!("{bd}/config"),
        "--work-dir",
        &format!("{bd}/work"),
        "--logs-dir",
        &format!("{bd}/logs"),
        "--agree-tos",
        "-m",
        "blocked@example.test",
    ]);
    let (ok, out) = run(&certbot, &args, &[]).await;
    assert!(
        !ok && out.contains("this contact may not register"),
        "{out}"
    );
    state.remove_server(sid).await;
}
