//! sievelib 1.5.0 (Python, independent, unchanged) against NetGet's ManageSieve server: login,
//! uploads, a refused script with its error, CHECKSCRIPT, SETACTIVE, LISTSCRIPTS, GETSCRIPT,
//! the response codes the handler chooses, HAVESPACE, deactivation, deletion; a wrong password.
//! Fails, never skips.
use crate::helpers::managesieve::*;
use netget::state::AccessLogOwner;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn sievelib_against_netget() {
    let dir = tempfile::tempdir().unwrap();
    let state = state();
    let (sid, addr) = server_in(&state, policy(&dir.path().join("scripts.json"))).await;
    let r = sievelib(addr.port(), "alice", "secret").await;
    let ok = |step: &str| {
        r.get(step)
            .unwrap_or_else(|| panic!("{step} missing: {r:?}"))["ok"]
            .as_bool()
            .unwrap()
    };
    assert!(ok("connect"), "{r:?}");
    assert_eq!(r["connect"]["implementation"], "NetGet");
    assert!(r["connect"]["sieve"]
        .as_array()
        .unwrap()
        .contains(&json!("vacation")));
    for step in [
        "put_spam",
        "put_vacation",
        "check_good",
        "setactive",
        "rename",
        "havespace_small",
        "deactivate",
        "delete",
        "logout",
    ] {
        assert!(ok(step), "{step}: {:?}", r[step]);
    }
    for step in [
        "put_bad",
        "check_bad",
        "get_missing",
        "delete_active",
        "rename_taken",
        "havespace_big",
    ] {
        assert!(!ok(step), "{step} should have failed: {:?}", r[step]);
    }
    assert!(
        r["put_bad"]["error"]
            .as_str()
            .unwrap()
            .contains("unknown command 'bogus'"),
        "{:?}",
        r["put_bad"]
    );
    assert_eq!(r["list"]["active"], "spam");
    assert_eq!(r["list"]["scripts"], json!(["vacation"]));
    // sievelib joins the lines it reads with LF and drops the last line break; the bytes the
    // server received are checked from its side below.
    let spam = "require \"fileinto\";\r\nif header :contains \"subject\" \"[SPAM]\" {\r\n  fileinto \"Junk\";\r\n}\r\n";
    assert_eq!(r["get"]["script"], spam.replace("\r\n", "\n").trim_end());
    assert_eq!(
        (
            r["list_after"]["active"].clone(),
            r["list_after"]["scripts"].clone()
        ),
        (json!(null), json!(["away"]))
    );

    let bad = sievelib(addr.port(), "alice", "wrong").await;
    assert!(!bad["connect"]["ok"].as_bool().unwrap(), "{bad:?}");

    let owner = AccessLogOwner::Server(sid.as_u32());
    let commands: Vec<String> = state
        .list_access_logs_for(Some(owner), None)
        .await
        .iter()
        .filter(|e| e.event_type == "managesieve_command")
        .map(|e| e.request["command"].as_str().unwrap_or_default().to_owned())
        .collect();
    let put = state
        .list_access_logs_for(Some(owner), None)
        .await
        .into_iter()
        .find(|e| {
            e.event_type == "managesieve_command"
                && e.request["name"] == "spam"
                && e.request["command"] == "PUTSCRIPT"
        })
        .unwrap();
    assert_eq!(put.request["script"], spam);
    for c in [
        "PUTSCRIPT",
        "CHECKSCRIPT",
        "SETACTIVE",
        "LISTSCRIPTS",
        "GETSCRIPT",
        "DELETESCRIPT",
        "RENAMESCRIPT",
        "HAVESPACE",
    ] {
        assert!(
            commands.iter().any(|x| x == c),
            "{c} never reached the handler: {commands:?}"
        );
    }
    state.remove_server(sid).await;
}
