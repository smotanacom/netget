//! Independent CalDAV clients, unchanged, against NetGet's server (backed by the script store):
//! python caldav 3.3.1 (discovery, MKCALENDAR, saves, a time-range search, lookup by UID, an
//! update, a delete, a refused password, a duplicate UID it refuses after looking it up) and vdirsyncer 0.21.0 (discovery, a full sync both
//! ways, a local change and a local delete propagated). Fails, never skips.
use crate::helpers::dav::*;
use netget::state::AccessLogOwner;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn python_caldav_discovers_saves_searches_updates_and_deletes() {
    let state = state();
    let dir = tempfile::tempdir().unwrap();
    let (sid, addr) = server_in(
        &state,
        "caldav",
        store_policy("caldav", &dir.path().join("db.json"), "work"),
        json!({}),
    )
    .await;
    let out = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(peer_bin("python"))
            .arg(peer_script())
            .arg(format!("http://{addr}/"))
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("caldav timed out")
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "python caldav failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let steps: HashMap<String, Value> = stdout
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .map(|v| (v["step"].as_str().unwrap().to_owned(), v))
        .collect();
    assert_eq!(steps["bad_login"]["refused"], true);
    assert!(steps["principal"]["url"]
        .as_str()
        .unwrap()
        .ends_with("/principals/alice/"));
    assert_eq!(steps["calendars"]["names"], json!(["Default"]));
    assert_eq!(
        steps["search"]["uids"],
        json!(["standup"]),
        "only the event inside the range"
    );
    assert_eq!(steps["updated"]["summary"], "Daily standup");
    assert_eq!(steps["remaining"]["uids"], json!(["standup"]));
    assert_eq!(
        steps["duplicate"]["refused"], true,
        "{}",
        steps["duplicate"]
    );
    let owner = AccessLogOwner::Server(sid.as_u32());
    let reqs = logs(&state, owner, "caldav_request", 1).await;
    assert!(reqs
        .iter()
        .any(|r| r.request["operation"] == "make_collection"
            && r.request["collection"] == "personal"));
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn vdirsyncer_syncs_calendars_both_ways() {
    let state = state();
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.json");
    std::fs::write(&db, json!({"collections": {"work": {"displayname": "Work", "objects": {
        "a.ics": "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//EN\r\nBEGIN:VEVENT\r\nUID:a\r\nDTSTAMP:20261001T000000Z\r\nDTSTART:20261005T100000Z\r\nSUMMARY:Remote A\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    }}}}).to_string()).unwrap();
    let (sid, addr) = server_in(
        &state,
        "caldav",
        store_policy("caldav", &db, "work"),
        json!({}),
    )
    .await;
    let sync = tempfile::tempdir().unwrap();
    let conf = vdirsyncer_config(sync.path(), "caldav", &format!("http://{addr}/"), "ics");
    let (ok, log) = vdirsyncer(&conf, &["discover"]).await;
    assert!(ok, "discover failed:\n{log}");
    let (ok, log) = vdirsyncer(&conf, &["sync"]).await;
    assert!(ok, "sync failed:\n{log}");
    let local = sync.path().join("local/work");
    let files: Vec<String> = std::fs::read_dir(&local)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(std::fs::read_to_string(local.join(&files[0]))
        .unwrap()
        .contains("SUMMARY:Remote A"));
    std::fs::write(local.join("b.ics"), "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//EN\r\nBEGIN:VEVENT\r\nUID:b\r\nDTSTAMP:20261001T000000Z\r\nDTSTART:20261006T100000Z\r\nSUMMARY:Local B\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n").unwrap();
    std::fs::remove_file(local.join(&files[0])).unwrap();
    let (ok, log) = vdirsyncer(&conf, &["sync"]).await;
    assert!(ok, "second sync failed:\n{log}");
    let stored: Value = serde_json::from_str(&std::fs::read_to_string(&db).unwrap()).unwrap();
    let objs = stored["collections"]["work"]["objects"]
        .as_object()
        .unwrap();
    assert_eq!(
        objs.len(),
        1,
        "the local delete reached the server and the new event arrived: {objs:?}"
    );
    assert!(objs
        .values()
        .next()
        .unwrap()
        .as_str()
        .unwrap()
        .contains("SUMMARY:Local B"));
    state.remove_server(sid).await;
}
