//! Independent SCIM clients, unchanged, against NetGet's service: scim2-tester 0.5.1 — a
//! compliance checker that builds its models from our discovery endpoints and round-trips
//! generated values through create, read, replace, PATCH add/replace/remove, attribute
//! selection and delete — and scim2-cli 0.4.0 for creates, a filtered sorted query, attribute
//! selection, a uniqueness conflict, replace and delete. Fails, never skips.
use crate::helpers::scim::*;
use serde_json::{json, Value};
use std::time::Duration;

async fn scim2(url: &str, args: &[&str], stdin: Option<&str>) -> (bool, String, String) {
    use tokio::io::AsyncWriteExt;
    let mut cmd = tokio::process::Command::new(peer_bin("scim2"));
    cmd.args(["--url", url, "-h", "Authorization: Bearer t0ken"])
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    if let Some(s) = stdin {
        input.write_all(s.as_bytes()).await.unwrap();
    }
    drop(input);
    let out = tokio::time::timeout(Duration::from_secs(180), child.wait_with_output())
        .await
        .expect("scim2 timed out")
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn scim2_tester_finds_netget_compliant() {
    let state = state();
    let dir = tempfile::tempdir().unwrap();
    let (sid, addr) = server_in(
        &state,
        store_policy(&dir.path().join("db.json")),
        json!({"bearer_token": "t0ken"}),
    )
    .await;
    let url = format!("http://{addr}/scim/v2");
    let (_, out, err) = scim2(&url, &["test"], None).await;
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    for line in out.lines() {
        if let Some(status) = ["SUCCESS", "ERROR", "CRITICAL", "SKIPPED", "WARNING"]
            .into_iter()
            .find(|s| line.starts_with(s))
        {
            *counts.entry(status).or_default() += 1;
        }
    }
    let failures: Vec<&str> = out
        .lines()
        .collect::<Vec<_>>()
        .windows(2)
        .filter(|w| w[0].starts_with("ERROR") || w[0].starts_with("CRITICAL"))
        .map(|w| w[1])
        .collect();
    assert!(
        failures.is_empty() && counts.get("ERROR").is_none() && counts.get("CRITICAL").is_none(),
        "scim2-tester failures {counts:?}:\n{}\n{err}",
        out.lines()
            .filter(|l| !l.starts_with("SUCCESS"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        counts.get("SUCCESS").copied().unwrap_or(0) >= 100,
        "{counts:?}\n{out}"
    );
    for needle in [
        "check_add_attribute",
        "check_remove_attribute",
        "check_replace_attribute",
        "object_replacement",
        "object_deletion",
        "search_with_attributes",
        "query_all_schemas",
    ] {
        assert!(
            out.lines()
                .any(|l| l.starts_with("SUCCESS") && l.contains(needle)),
            "no successful {needle}"
        );
    }
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn scim2_cli_creates_filters_replaces_and_deletes() {
    let state = state();
    let dir = tempfile::tempdir().unwrap();
    let (sid, addr) = server_in(
        &state,
        store_policy(&dir.path().join("db.json")),
        json!({"bearer_token": "t0ken", "base_path": ""}),
    )
    .await;
    let url = format!("http://{addr}");
    let mut ids = Vec::new();
    for (user, given) in [("bjensen", "Barbara"), ("alice", "Alice"), ("bob", "Bob")] {
        let (ok, out, err) = scim2(
            &url,
            &[
                "create",
                "user",
                "--user-name",
                user,
                "--name",
                &format!(r#"{{"givenName":"{given}"}}"#),
                "--emails",
                &format!(r#"[{{"value":"{user}@example.com","type":"work","primary":true}}]"#),
            ],
            None,
        )
        .await;
        assert!(ok, "{out}\n{err}");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            (v["userName"].as_str(), v["meta"]["resourceType"].as_str()),
            (Some(user), Some("User"))
        );
        assert!(v["meta"]["location"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/Users/{}", v["id"].as_str().unwrap())));
        ids.push(v["id"].as_str().unwrap().to_owned());
    }
    let (ok, out, err) = scim2(
        &url,
        &[
            "query",
            "user",
            "--filter",
            "userName sw \"b\" or emails[type eq \"work\" and value co \"alice\"]",
            "--sort-by",
            "userName",
            "--sort-order",
            "descending",
        ],
        None,
    )
    .await;
    assert!(ok, "{out}\n{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    let names: Vec<&str> = v["Resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["userName"].as_str().unwrap())
        .collect();
    assert_eq!(
        (v["totalResults"].as_u64(), names),
        (Some(3), vec!["bob", "bjensen", "alice"])
    );
    let (ok, out, _) = scim2(
        &url,
        &[
            "query",
            "user",
            "--filter",
            "name.givenName eq \"BARBARA\"",
            "--count",
            "1",
        ],
        None,
    )
    .await;
    assert!(ok);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        (
            v["totalResults"].as_u64(),
            v["Resources"][0]["userName"].as_str()
        ),
        (Some(1), Some("bjensen")),
        "givenName is not caseExact"
    );
    let (ok, out, _) = scim2(
        &url,
        &["query", "user", &ids[0], "--attribute", "userName"],
        None,
    )
    .await;
    assert!(ok);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert!(
        v.get("name").is_none() && v["userName"] == "bjensen" && v["id"] == ids[0].as_str(),
        "{v}"
    );
    // scim2-cli prints a SCIM error as JSON and exits 0, so read the status it printed.
    let (_, out, err) = scim2(&url, &["create", "user", "--user-name", "BJENSEN"], None).await;
    let refused: Value = serde_json::from_str(&out).unwrap_or(Value::Null);
    assert_eq!(
        (refused["status"].as_str(), refused["scimType"].as_str()),
        (Some("409"), Some("uniqueness")),
        "a duplicate userName is refused:\n{out}\n{err}"
    );
    let (ok, out, err) = scim2(&url, &["replace", "user"], Some(&json!({"schemas":["urn:ietf:params:scim:schemas:core:2.0:User"],"id":ids[1],"userName":"alice","active":false}).to_string())).await;
    assert!(ok, "{out}\n{err}");
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap()["active"],
        false
    );
    let (ok, _, err) = scim2(&url, &["delete", "user", &ids[2]], None).await;
    assert!(ok, "{err}");
    let (_, out, err) = scim2(&url, &["query", "user", &ids[2]], None).await;
    let gone: Value = serde_json::from_str(&out).unwrap_or(Value::Null);
    assert_eq!(
        gone["status"].as_str(),
        Some("404"),
        "deleted:\n{out}\n{err}"
    );
    state.remove_server(sid).await;
}
