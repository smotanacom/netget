//! NetGet's CalDAV client against Radicale 3.8.1 (independent, unchanged): discovery from
//! /.well-known/caldav, MKCALENDAR, PUT (create-only and conditional), list, a time-range
//! query, GET and DELETE. Fails, never skips.
use crate::helpers::dav::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;

fn ics(uid: &str, start: &str) -> String {
    format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//NetGet//EN\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:20261001T000000Z\r\nDTSTART:{start}\r\nDURATION:PT1H\r\nSUMMARY:{uid}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n")
}

#[tokio::test(flavor = "multi_thread")]
async fn client_works_against_radicale() {
    let radicale = start_radicale().await.unwrap();
    let state = state();
    let cid = client_in(
        &state,
        "caldav",
        radicale.addr(),
        json!({"scheme": "http", "username": "alice", "password": "secret"}),
    )
    .await
    .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    let connected = logs(&state, owner, "caldav_connected", 1).await;
    assert_eq!(connected[0].request["home"], "/alice/");
    let send = |a: Value| state.send_to_client(cid, a, Duration::from_secs(20));
    for a in [
        json!({"type":"caldav_make_collection","collection":"work","displayname":"Work"}),
        json!({"type":"caldav_put","collection":"work","name":"a.ics","data":ics("a","20261005T100000Z"),"create_only":true}),
        json!({"type":"caldav_put","collection":"work","name":"b.ics","data":ics("b","20261020T100000Z")}),
        json!({"type":"caldav_put","collection":"work","name":"a.ics","data":ics("a","20261005T100000Z"),"create_only":true}),
        json!({"type":"caldav_list","collection":"work"}),
        json!({"type":"caldav_query","collection":"work","start":"20261005T000000Z","end":"20261012T000000Z"}),
        json!({"type":"caldav_get","collection":"work","name":"a.ics"}),
        json!({"type":"caldav_delete","collection":"work","name":"b.ics"}),
    ] {
        assert!(matches!(
            send(a).await.unwrap(),
            ClientSendOutcome::Sent { .. }
        ));
    }
    let rows = logs(&state, owner, "caldav_response", 8).await;
    let r: Vec<&Value> = rows.iter().map(|r| &r.request).collect();
    assert_eq!(r[0]["status"], 201);
    assert_eq!(r[1]["status"], 201);
    assert!(matches!(r[2]["status"].as_u64(), Some(201 | 204)));
    assert_eq!(
        r[3]["status"], 412,
        "If-None-Match: * on an existing object"
    );
    assert_eq!(r[4]["objects"].as_array().unwrap().len(), 2);
    let q = r[5]["objects"].as_array().unwrap();
    assert_eq!(q.len(), 1, "{}", r[5]);
    assert!(q[0]["data"].as_str().unwrap().contains("UID:a"));
    let etag = r[6]["etag"].as_str().unwrap().to_owned();
    assert_eq!(r[7]["status"], 200, "Radicale answers DELETE with 200");
    send(json!({"type":"caldav_put","collection":"work","name":"a.ics","data":ics("a","20261006T100000Z"),"if_match":"\"stale\""})).await.unwrap();
    send(json!({"type":"caldav_put","collection":"work","name":"a.ics","data":ics("a","20261006T100000Z"),"if_match":etag})).await.unwrap();
    let rows = logs(&state, owner, "caldav_response", 10).await;
    assert_eq!(
        (
            rows[8].request["status"].as_u64(),
            rows[9].request["status"].as_u64()
        ),
        (Some(412), Some(204))
    );
    state.remove_client(cid).await;
}
