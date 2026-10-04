//! jmapc 0.3.0 (Python, independent, unchanged) against NetGet's JMAP server over HTTPS with
//! the self-signed certificate the server publishes: session discovery, Core/echo, a mailbox
//! query feeding a get by result reference, an email query and get, a create referenced by its
//! creation id in the same request, an update refused for an unknown id, changes since a state
//! and a method error for an old one; a wrong password. Fails, never skips.
use crate::helpers::jmap::*;
use netget::state::AccessLogOwner;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn jmapc_against_netget() {
    let dir = tempfile::tempdir().unwrap();
    let state = state();
    let (sid, addr) = server_in(&state, alice()).await;
    let ca = certificate(&state, sid, dir.path()).await;
    let host = format!("localhost:{}", addr.port());

    let r = jmapc(&host, "alice@example.com", "secret", &ca).await;
    assert_eq!(r["username"], "alice@example.com", "{r}");
    assert_eq!(r["account"], "a1", "{r}");
    assert_eq!(r["api_url"], format!("https://{host}/jmap/"), "{r}");
    assert_eq!(r["echo"], json!({"hello": "world"}), "{r}");
    assert_eq!(r["mailboxes"], json!(["Inbox"]), "{r}");
    assert_eq!(r["subjects"], json!(["Invoice", "Welcome"]), "{r}");
    assert_eq!(r["created"], json!({"draft": "m-new"}), "{r}");
    assert_eq!(r["updated"], json!(["e1"]), "{r}");
    assert_eq!(r["not_updated"], json!({"e9": "notFound"}), "{r}");
    assert_eq!(r["draft_subject"], json!(["Draft"]), "{r}");
    assert_eq!(
        r["changes"],
        json!({"old": "s1", "new": "s2", "created": ["m-new"], "updated": ["e1"]}),
        "{r}"
    );
    assert_eq!(r["old_changes_error"], "cannotCalculateChanges", "{r}");

    // What the handler saw: the reference and the creation id were resolved by Rust.
    let owner = AccessLogOwner::Server(sid.as_u32());
    let get = wait_for(&state, owner, "jmap_method_call", |e| {
        e["method"] == "Mailbox/get"
    })
    .await;
    assert_eq!(get["arguments"]["ids"], json!(["inbox"]), "{get}");
    assert!(get["arguments"].get("#ids").is_none(), "{get}");
    let draft = wait_for(&state, owner, "jmap_method_call", |e| {
        e["method"] == "Email/get" && e["arguments"]["ids"] == json!(["m-new"])
    })
    .await;
    assert_eq!(draft["username"], "alice@example.com");
    // Core/echo never reaches the handler.
    assert!(state
        .list_access_logs_for(Some(owner), None)
        .await
        .iter()
        .all(|e| e.request["method"] != "Core/echo"));

    let r = jmapc(&host, "alice@example.com", "wrong", &ca).await;
    assert!(
        r["session_error"]
            .as_str()
            .is_some_and(|e| e.contains("401")),
        "{r}"
    );
    state.remove_server(sid).await;
}
