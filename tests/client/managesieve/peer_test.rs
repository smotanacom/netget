//! NetGet's ManageSieve client against Dovecot 2.4.5 with Pigeonhole 2.4.5 (C, independent,
//! unchanged), which compiles every script it is given: uploads, a script Pigeonhole rejects
//! with its compiler error, CHECKSCRIPT, activation (Dovecot's own active link), listing,
//! retrieval, renaming, deletion of the active script refused, HAVESPACE, and a refused login.
//! Fails, never skips.
use crate::helpers::managesieve::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn client_against_dovecot() {
    let (dovecot, run) = start_dovecot().await.unwrap();
    let state = state();
    let cid = client_in(
        &state,
        dovecot.addr(),
        json!({"user": "alice", "password": "secret"}),
    )
    .await
    .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    let connected = &logs(&state, owner, "managesieve_connected", 1).await[0];
    assert_eq!(
        connected["implementation"], "Dovecot Pigeonhole",
        "{connected}"
    );
    assert!(connected["sieve_extensions"]
        .as_array()
        .unwrap()
        .contains(&json!("vacation")));
    let spam = "require \"fileinto\";\r\nif header :contains \"subject\" \"[SPAM]\" { fileinto \"Junk\"; }\r\n";
    for a in [
        json!({"type": "managesieve_put", "name": "spam", "script": spam}),
        json!({"type": "managesieve_put", "name": "bad", "script": "bogus;\r\n"}),
        json!({"type": "managesieve_check", "script": "require \"vacation\";\r\nvacation \"Away\";\r\n"}),
        json!({"type": "managesieve_set_active", "name": "spam"}),
        json!({"type": "managesieve_list"}),
        json!({"type": "managesieve_get", "name": "spam"}),
        json!({"type": "managesieve_delete", "name": "spam"}),
        json!({"type": "managesieve_rename", "name": "spam", "new_name": "junk"}),
        json!({"type": "managesieve_get", "name": "missing"}),
        json!({"type": "managesieve_have_space", "name": "junk", "size": 1000}),
    ] {
        let r = state
            .send_to_client(cid, a.clone(), Duration::from_secs(20))
            .await
            .unwrap();
        assert!(matches!(r, ClientSendOutcome::Sent { .. }), "{a}: {r:?}");
    }
    let r: Vec<Value> = logs(&state, owner, "managesieve_response", 10).await;
    let status: Vec<&str> = r.iter().map(|x| x["status"].as_str().unwrap()).collect();
    assert_eq!(
        status,
        ["OK", "NO", "OK", "OK", "OK", "OK", "NO", "OK", "NO", "OK"],
        "{r:?}\n{}",
        dovecot.log()
    );
    assert!(
        r[1]["message"]
            .as_str()
            .unwrap()
            .contains("unknown command 'bogus'"),
        "{}",
        r[1]
    );
    assert_eq!(
        r[4]["scripts"],
        json!([{"name": "spam", "active": true}]),
        "{}",
        r[4]
    );
    assert_eq!(r[5]["script"], spam);
    assert_eq!(r[6]["code"], "ACTIVE", "{}", r[6]);
    assert_eq!(r[8]["code"], "NONEXISTENT", "{}", r[8]);
    // Dovecot itself now holds the renamed script and points its active link at it.
    let home = std::fs::read_dir(run.path().join("home"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        std::fs::read_to_string(home.join("sieve").join("junk.sieve")).unwrap(),
        spam
    );
    assert!(std::fs::read_link(home.join(".dovecot.sieve"))
        .unwrap()
        .to_string_lossy()
        .ends_with("junk.sieve"));
    assert!(matches!(
        state
            .send_to_client(cid, json!({"type": "disconnect"}), Duration::from_secs(20))
            .await
            .unwrap(),
        ClientSendOutcome::Disconnected
    ));

    let refused = client_in(
        &state,
        dovecot.addr(),
        json!({"user": "alice", "password": "wrong"}),
    )
    .await;
    assert!(refused
        .unwrap_err()
        .to_string()
        .contains("refused the login"));
    state.remove_client(cid).await;
}
