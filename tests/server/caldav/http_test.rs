//! CalDAV without peers: iCalendar validation and time-range overlap, PROPFIND and REPORT on the
//! wire (well-known redirect, principal, home, collection, calendar-query, multiget), PUT
//! preconditions, auth, fail-closed answers, and the NetGet client/server pair.
use crate::helpers::dav::*;
use netget::server::dav_common::object;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;

fn ics(uid: &str, start: &str, extra: &str) -> String {
    format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//EN\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:20261001T000000Z\r\nDTSTART:{start}\r\n{extra}SUMMARY:Event {uid}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n")
}

#[test]
fn icalendar_validation_and_time_ranges() {
    let (kind, uid, cal) =
        object::check_calendar(&ics("a", "20261005T100000Z", "DTEND:20261005T110000Z\r\n"))
            .unwrap();
    assert_eq!((kind.as_str(), uid.as_str()), ("VEVENT", "a"));
    let ev = &cal.children[0];
    let t = |s: &str| object::timestamp(s).ok();
    assert!(object::overlaps(
        ev,
        t("20261005T103000Z"),
        t("20261005T120000Z")
    ));
    assert!(
        !object::overlaps(ev, t("20261005T110000Z"), t("20261005T120000Z")),
        "end is exclusive"
    );
    let all_day = object::check_calendar(&ics("d", "20261007", "")).unwrap().2;
    assert!(
        object::overlaps(&all_day.children[0], t("20261007T230000Z"), None),
        "a DATE start lasts a day"
    );
    let rec = object::check_calendar(&ics("r", "20260101T100000Z", "RRULE:FREQ=WEEKLY\r\n"))
        .unwrap()
        .2;
    assert!(
        object::overlaps(
            &rec.children[0],
            t("20271001T000000Z"),
            t("20271002T000000Z")
        ),
        "recurring objects are not expanded, so they match later ranges"
    );
    assert_eq!(object::duration("PT1H30M").unwrap(), 5400);
    assert_eq!(object::duration("-P1W").unwrap(), -604_800);
    let folded = ics("f", "20261005T100000Z", "DESCRIPTION:a long\r\n  line\r\n");
    assert_eq!(
        object::check_calendar(&folded).unwrap().2.children[0]
            .prop("DESCRIPTION")
            .unwrap()
            .value,
        "a long line"
    );
    for bad in [
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nEND:VCALENDAR\r\n".to_owned(),
        ics("a", "20261005T100000Z", "").replace("UID:a\r\n", ""),
        ics("a", "20261005T100000Z", "").replace("END:VEVENT", "END:VTODO"),
        ics("a", "20261005T100000Z", "").replace("VERSION:2.0", "VERSION:1.0"),
        format!("{}BEGIN:VCALENDAR\r\n", ics("a", "20261005T100000Z", "")),
    ] {
        assert!(object::check_calendar(&bad).is_err(), "{bad}");
    }
}

async fn dav(
    method: &str,
    url: &str,
    auth: bool,
    depth: Option<&str>,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, reqwest::header::HeaderMap, String) {
    let mut r = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .request(method.parse().unwrap(), url)
        .body(body.to_owned());
    if auth {
        r = r.basic_auth("alice", Some("secret"));
    }
    if let Some(d) = depth {
        r = r.header("Depth", d);
    }
    for (k, v) in headers {
        r = r.header(*k, *v);
    }
    let resp = r.send().await.unwrap();
    (
        resp.status().as_u16(),
        resp.headers().clone(),
        resp.text().await.unwrap(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn propfind_report_preconditions_and_fail_closed() {
    let state = state();
    let dir = tempfile::tempdir().unwrap();
    let (sid, addr) = server_in(
        &state,
        "caldav",
        store_policy("caldav", &dir.path().join("db.json"), "work"),
        json!({}),
    )
    .await;
    let base = format!("http://{addr}");
    let (s, h, _) = dav(
        "PROPFIND",
        &format!("{base}/.well-known/caldav"),
        true,
        Some("0"),
        &[],
        "",
    )
    .await;
    assert_eq!((s, h["location"].to_str().unwrap()), (301, "/"));
    let (s, h, _) = dav("PROPFIND", &format!("{base}/"), false, Some("0"), &[], "").await;
    assert_eq!(s, 401);
    assert!(h["www-authenticate"].to_str().unwrap().starts_with("Basic"));
    let (s, _, body) = dav("PROPFIND", &format!("{base}/principals/alice/"), true, Some("0"), &[], r#"<propfind xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><prop><C:calendar-home-set/><displayname/><C:no-such-prop/></prop></propfind>"#).await;
    assert_eq!(s, 207);
    assert!(
        body.contains("<D:href>/calendars/alice/</D:href>")
            && body.contains("404 Not Found")
            && body.contains("no-such-prop"),
        "{body}"
    );
    assert_eq!(
        dav(
            "PROPFIND",
            &format!("{base}/calendars/bob/"),
            true,
            Some("0"),
            &[],
            ""
        )
        .await
        .0,
        403,
        "another user's home"
    );
    let (_, _, body) = dav(
        "PROPFIND",
        &format!("{base}/calendars/alice/"),
        true,
        Some("1"),
        &[],
        "",
    )
    .await;
    assert!(
        body.contains("/calendars/alice/work/") && body.contains("<C:calendar/>"),
        "{body}"
    );
    let work = format!("{base}/calendars/alice/work");
    let (s, h, _) = dav(
        "PUT",
        &format!("{work}/a.ics"),
        true,
        None,
        &[("If-None-Match", "*"), ("Content-Type", "text/calendar")],
        &ics("a", "20261005T100000Z", "DTEND:20261005T110000Z\r\n"),
    )
    .await;
    assert_eq!(s, 201);
    let etag = h["etag"].to_str().unwrap().to_owned();
    assert_eq!(
        dav(
            "PUT",
            &format!("{work}/a.ics"),
            true,
            None,
            &[("If-None-Match", "*")],
            &ics("a", "20261005T100000Z", "")
        )
        .await
        .0,
        412
    );
    assert_eq!(
        dav(
            "PUT",
            &format!("{work}/a.ics"),
            true,
            None,
            &[("If-Match", "\"stale\"")],
            &ics("a", "20261005T100000Z", "")
        )
        .await
        .0,
        412
    );
    let (s, _, body) = dav(
        "PUT",
        &format!("{work}/dup.ics"),
        true,
        None,
        &[],
        &ics("a", "20261008T100000Z", ""),
    )
    .await;
    assert!(s == 409 && body.contains("no-uid-conflict"), "{s} {body}");
    let (s, _, body) = dav(
        "PUT",
        &format!("{work}/bad.ics"),
        true,
        None,
        &[],
        "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n",
    )
    .await;
    assert!(s == 403 && body.contains("valid-calendar-data"), "{body}");
    dav(
        "PUT",
        &format!("{work}/b.ics"),
        true,
        None,
        &[],
        &ics("b", "20261020T100000Z", ""),
    )
    .await;
    let (s, h, got) = dav("GET", &format!("{work}/a.ics"), true, None, &[], "").await;
    assert_eq!((s, h["etag"].to_str().unwrap()), (200, etag.as_str()));
    assert!(
        got.contains("UID:a")
            && h["content-type"]
                .to_str()
                .unwrap()
                .contains("component=vevent")
    );
    let query = r#"<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:prop><D:getetag/><C:calendar-data/></D:prop><C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VEVENT"><C:time-range start="20261005T000000Z" end="20261012T000000Z"/></C:comp-filter></C:comp-filter></C:filter></C:calendar-query>"#;
    let (s, _, body) = dav("REPORT", &format!("{work}/"), true, Some("1"), &[], query).await;
    assert!(
        s == 207 && body.contains("a.ics") && !body.contains("b.ics") && body.contains("UID:a"),
        "{body}"
    );
    let text = r#"<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:prop><D:getetag/></D:prop><C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VEVENT"><C:prop-filter name="SUMMARY"><C:text-match>event B</C:text-match></C:prop-filter></C:comp-filter></C:comp-filter></C:filter></C:calendar-query>"#;
    let (_, _, body) = dav("REPORT", &format!("{work}/"), true, Some("1"), &[], text).await;
    assert!(body.contains("b.ics") && !body.contains("a.ics"), "{body}");
    let multiget = format!(
        r#"<C:calendar-multiget xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:prop><D:getetag/></D:prop><D:href>/calendars/alice/work/b.ics</D:href><D:href>/calendars/alice/work/zzz.ics</D:href></C:calendar-multiget>"#
    );
    let (_, _, body) = dav(
        "REPORT",
        &format!("{work}/"),
        true,
        Some("1"),
        &[],
        &multiget,
    )
    .await;
    assert!(
        body.contains("b.ics") && body.contains("404 Not Found"),
        "{body}"
    );
    let (s, _, body) = dav(
        "REPORT",
        &format!("{work}/"),
        true,
        Some("1"),
        &[],
        r#"<D:sync-collection xmlns:D="DAV:"/>"#,
    )
    .await;
    assert!(s == 403 && body.contains("supported-report"));
    assert_eq!(
        dav(
            "DELETE",
            &format!("{work}/a.ics"),
            true,
            None,
            &[("If-Match", "\"stale\"")],
            ""
        )
        .await
        .0,
        412
    );
    assert_eq!(
        dav(
            "DELETE",
            &format!("{work}/a.ics"),
            true,
            None,
            &[("If-Match", &etag)],
            ""
        )
        .await
        .0,
        204
    );
    assert_eq!(
        dav("GET", &format!("{work}/a.ics"), true, None, &[], "")
            .await
            .0,
        404
    );
    assert_eq!(
        dav(
            "MKCALENDAR",
            &format!("{base}/calendars/alice/home/"),
            true,
            None,
            &[],
            ""
        )
        .await
        .0,
        201
    );
    assert_eq!(
        dav(
            "MKCOL",
            &format!("{base}/calendars/alice/other/"),
            true,
            None,
            &[],
            ""
        )
        .await
        .0,
        405
    );
    let (s, h, _) = dav("OPTIONS", &format!("{base}/"), false, None, &[], "").await;
    assert!(s == 200 && h["dav"].to_str().unwrap().contains("calendar-access"));
    state.remove_server(sid).await;

    // No handler and no model: logins are refused; with auth off, data requests are a 500.
    let state = crate::helpers::dav::state();
    let (sid, addr) = server_in(&state, "caldav", vec![], json!({})).await;
    assert_eq!(
        dav(
            "PROPFIND",
            &format!("http://{addr}/calendars/alice/"),
            true,
            Some("1"),
            &[],
            ""
        )
        .await
        .0,
        401
    );
    state.remove_server(sid).await;
    let (sid, addr) = server_in(&state, "caldav", vec![], json!({"auth": "none"})).await;
    assert_eq!(
        dav(
            "PROPFIND",
            &format!("http://{addr}/calendars/user/"),
            false,
            Some("1"),
            &[],
            ""
        )
        .await
        .0,
        500
    );
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_client_and_server_agree() {
    let state = state();
    let dir = tempfile::tempdir().unwrap();
    let (sid, addr) = server_in(
        &state,
        "caldav",
        store_policy("caldav", &dir.path().join("db.json"), "work"),
        json!({}),
    )
    .await;
    let cid = client_in(
        &state,
        "caldav",
        addr.to_string(),
        json!({"scheme": "http", "username": "alice", "password": "secret"}),
    )
    .await
    .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    let connected = logs(&state, owner, "caldav_connected", 1).await;
    assert_eq!(
        (
            connected[0].request["home"].as_str(),
            connected[0].request["collections"][0]["name"].as_str()
        ),
        (Some("/calendars/alice/"), Some("work"))
    );
    let send = |a: Value| state.send_to_client(cid, a, Duration::from_secs(15));
    for a in [
        json!({"type":"caldav_put","collection":"work","name":"a.ics","data":ics("a","20261005T100000Z","DTEND:20261005T110000Z\r\n"),"create_only":true}),
        json!({"type":"caldav_put","collection":"work","name":"b.ics","data":ics("b","20261020T100000Z","")}),
        json!({"type":"caldav_list","collection":"work"}),
        json!({"type":"caldav_query","collection":"work","start":"20261005T000000Z","end":"20261012T000000Z"}),
        json!({"type":"caldav_get","collection":"work","name":"a.ics"}),
        json!({"type":"caldav_put","collection":"work","name":"a.ics","data":ics("a","20261005T100000Z",""),"if_match":"\"stale\""}),
        json!({"type":"caldav_delete","collection":"work","name":"b.ics"}),
        json!({"type":"caldav_make_collection","collection":"team","displayname":"Team"}),
    ] {
        assert!(matches!(
            send(a).await.unwrap(),
            ClientSendOutcome::Sent { .. }
        ));
    }
    let rows = logs(&state, owner, "caldav_response", 8).await;
    let r: Vec<&Value> = rows.iter().map(|r| &r.request).collect();
    assert_eq!(
        (r[0]["status"].as_u64(), r[1]["status"].as_u64()),
        (Some(201), Some(201))
    );
    assert_eq!(r[2]["objects"].as_array().unwrap().len(), 2);
    let q = r[3]["objects"].as_array().unwrap();
    assert!(q.len() == 1 && q[0]["data"].as_str().unwrap().contains("UID:a"));
    assert!(r[4]["data"].as_str().unwrap().contains("UID:a") && r[4]["etag"].is_string());
    assert_eq!(r[5]["status"], 412);
    assert_eq!(r[6]["status"], 204);
    assert_eq!(r[7]["status"], 201);
    assert!(matches!(send(json!({"type":"caldav_put","collection":"work","name":"x.ics","data":"BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n"})).await.unwrap(), ClientSendOutcome::Rejected { .. }), "invalid iCalendar never leaves the client");
    state.remove_client(cid).await;
    state.remove_server(sid).await;
}
