//! Independent LMTP clients against NetGet's server, each failing rather than skipping when
//! absent: CPython's `smtplib.LMTP` (standard library, unchanged) and swaks
//! (`swaks --protocol LMTP`, the Perl Swiss Army Knife for SMTP). Both see one reply per
//! accepted recipient after DATA, which is the part of RFC 2033 that differs from SMTP.
use super::wire_test::{scripted_handlers, start};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};

fn binary(env: &str, name: &str, hint: &str) -> PathBuf {
    std::env::var_os(env)
        .map(PathBuf::from)
        .or_else(|| crate::helpers::real_server::find_binary(name))
        .unwrap_or_else(|| panic!("{name} is required for LMTP evidence: {hint}, or set {env}"))
}

const SMTPLIB_CLIENT: &str = r#"
import json, smtplib, sys
s = smtplib.LMTP("127.0.0.1", int(sys.argv[1]))
out = {"lhlo": s.ehlo()[0], "features": sorted(s.esmtp_features)}
out["mail"] = s.mail("s@example.test")[0]
out["rcpt"] = [s.rcpt(r)[0] for r in ["alice@example.test", "nobody@example.test", "bob@example.test"]]
first = s.data("Subject: From CPython\r\n\r\nHello from smtplib\r\n")
second = s.getreply()
out["delivery"] = [[c, m.decode()] for c, m in (first, second)]
out["quit"] = s.quit()[0]
print(json.dumps(out))
"#;

#[tokio::test]
async fn cpython_smtplib_lmtp_reads_one_reply_per_recipient() {
    let python = binary("NETGET_PYTHON3_BIN", "python3", "install python3");
    let (state, id, addr) = start(scripted_handlers(), json!({})).await;
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(python)
            .args(["-I", "-c", SMTPLIB_CLIENT, &addr.port().to_string()])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("smtplib deadline")
    .expect("start python3");
    assert!(
        output.status.success(),
        "smtplib failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let r: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(r["lhlo"], 250, "{r}");
    for feature in ["pipelining", "enhancedstatuscodes", "8bitmime", "size"] {
        assert!(
            r["features"].as_array().unwrap().contains(&json!(feature)),
            "{feature} missing: {r}"
        );
    }
    assert_eq!(r["mail"], 250, "{r}");
    assert_eq!(r["rcpt"], json!([250, 550, 250]), "{r}");
    assert_eq!(
        r["delivery"],
        json!([
            [
                250,
                "2.0.0 Delivered subject=From CPython body=Hello from smtplib"
            ],
            [550, "5.0.0 Mailbox full"]
        ]),
        "{r}"
    );
    assert_eq!(r["quit"], 221, "{r}");
    state.remove_server(id).await;
}

#[tokio::test]
async fn swaks_delivers_over_lmtp_and_reports_each_recipient() {
    let swaks = binary(
        "NETGET_SWAKS_BIN",
        "swaks",
        "install swaks (apt or Homebrew)",
    );
    let (state, id, addr) = start(scripted_handlers(), json!({"hostname":"mx.test"})).await;
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(swaks)
            .args([
                "--protocol",
                "LMTP",
                "--server",
                "127.0.0.1",
                "--port",
                &addr.port().to_string(),
                "--helo",
                "swaks.test",
                "--from",
                "s@example.test",
                "--to",
                "alice@example.test,bob@example.test",
                "--header",
                "Subject: From swaks",
                "--body",
                "Hello from swaks",
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("swaks deadline")
    .expect("start swaks");
    let transcript = String::from_utf8_lossy(&output.stdout).to_string()
        + &String::from_utf8_lossy(&output.stderr);
    for expected in [
        "220 mx.test LMTP NetGet ready",
        "LHLO swaks.test",
        "250-PIPELINING",
        "RCPT TO:<alice@example.test>",
        "250 2.1.5 <alice@example.test> recipient OK",
        "354 End data",
        "250 2.0.0 Delivered subject=From swaks body=Hello from swaks",
        "550 5.0.0 Mailbox full",
        "221 2.0.0 mx.test closing connection",
    ] {
        assert!(
            transcript.contains(expected),
            "missing {expected:?} in:\n{transcript}"
        );
    }
    state.remove_server(id).await;
}
