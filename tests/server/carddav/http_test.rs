//! CardDAV without peers: vCard validation, addressbook-query filters (text-match types,
//! negation, anyof/allof, is-not-defined, limit), multiget, extended MKCOL, and the NetGet pair.
use crate::helpers::dav::*;
use netget::server::dav_common::object;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;

fn card(uid: &str, fn_: &str, extra: &str) -> String {
    format!("BEGIN:VCARD\r\nVERSION:3.0\r\nUID:{uid}\r\nFN:{fn_}\r\n{extra}END:VCARD\r\n")
}

#[test]
fn vcard_validation() {
    assert_eq!(
        object::check_vcard(&card(
            "a",
            "Ada",
            "item1.EMAIL;TYPE=work:ada@example.com\r\n"
        ))
        .unwrap()
        .0,
        "a"
    );
    for bad in [
        card("a", "Ada", "").replace("FN:Ada\r\n", ""),
        card("a", "Ada", "").replace("UID:a\r\n", ""),
        card("a", "Ada", "").replace("3.0", "2.1"),
        "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n".into(),
    ] {
        assert!(object::check_vcard(&bad).is_err(), "{bad}");
    }
}

async fn dav(method: &str, url: &str, headers: &[(&str, &str)], body: &str) -> (u16, String) {
    let mut r = reqwest::Client::new()
        .request(method.parse().unwrap(), url)
        .basic_auth("alice", Some("secret"))
        .header("Depth", "1")
        .body(body.to_owned());
    for (k, v) in headers {
        r = r.header(*k, *v);
    }
    let resp = r.send().await.unwrap();
    (resp.status().as_u16(), resp.text().await.unwrap())
}

fn query(filter: &str) -> String {
    format!(
        r#"<CR:addressbook-query xmlns:D="DAV:" xmlns:CR="urn:ietf:params:xml:ns:carddav"><D:prop><D:getetag/></D:prop>{filter}</CR:addressbook-query>"#
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn addressbook_queries_multiget_and_mkcol() {
    let state = state();
    let dir = tempfile::tempdir().unwrap();
    let (sid, addr) = server_in(
        &state,
        "carddav",
        store_policy("carddav", &dir.path().join("db.json"), "contacts"),
        json!({}),
    )
    .await;
    let book = format!("http://{addr}/addressbooks/alice/contacts");
    for (name, c) in [
        (
            "ada.vcf",
            card("ada", "Ada Lovelace", "EMAIL:ada@example.com\r\n"),
        ),
        (
            "bob.vcf",
            card("bob", "Bob Smith", "EMAIL:bob@example.org\r\nTEL:555\r\n"),
        ),
        ("cy.vcf", card("cy", "Cy", "")),
    ] {
        assert_eq!(
            dav(
                "PUT",
                &format!("{book}/{name}"),
                &[("If-None-Match", "*")],
                &c
            )
            .await
            .0,
            201
        );
    }
    let (s, body) = dav(
        "PUT",
        &format!("{book}/bad.vcf"),
        &[],
        "BEGIN:VCARD\r\nVERSION:3.0\r\nEND:VCARD\r\n",
    )
    .await;
    assert!(s == 403 && body.contains("valid-address-data"));
    let hits = |body: &str| {
        ["ada.vcf", "bob.vcf", "cy.vcf"]
            .iter()
            .filter(|n| body.contains(*n))
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
    };
    for (filter, expected) in [
        (
            r#"<CR:filter><CR:prop-filter name="EMAIL"><CR:text-match match-type="ends-with">example.com</CR:text-match></CR:prop-filter></CR:filter>"#,
            vec!["ada.vcf"],
        ),
        (
            r#"<CR:filter><CR:prop-filter name="FN"><CR:text-match match-type="starts-with">BOB</CR:text-match></CR:prop-filter></CR:filter>"#,
            vec!["bob.vcf"],
        ),
        (
            r#"<CR:filter><CR:prop-filter name="EMAIL"><CR:is-not-defined/></CR:prop-filter></CR:filter>"#,
            vec!["cy.vcf"],
        ),
        (
            r#"<CR:filter test="allof"><CR:prop-filter name="EMAIL"/><CR:prop-filter name="TEL"/></CR:filter>"#,
            vec!["bob.vcf"],
        ),
        (
            r#"<CR:filter test="anyof"><CR:prop-filter name="FN"><CR:text-match match-type="equals">cy</CR:text-match></CR:prop-filter><CR:prop-filter name="TEL"/></CR:filter>"#,
            vec!["bob.vcf", "cy.vcf"],
        ),
        (
            r#"<CR:filter><CR:prop-filter name="FN"><CR:text-match negate-condition="yes">a</CR:text-match></CR:prop-filter></CR:filter>"#,
            vec!["bob.vcf", "cy.vcf"],
        ),
    ] {
        let (s, body) = dav("REPORT", &format!("{book}/"), &[], &query(filter)).await;
        assert_eq!(
            (s, hits(&body)),
            (
                207,
                expected.iter().map(|s| s.to_string()).collect::<Vec<_>>()
            ),
            "{filter}"
        );
    }
    let limited = query(r#"<CR:filter/><CR:limit><CR:nresults>1</CR:nresults></CR:limit>"#);
    assert_eq!(
        hits(&dav("REPORT", &format!("{book}/"), &[], &limited).await.1).len(),
        1
    );
    let (_, body) = dav("REPORT", &format!("{book}/"), &[], r#"<CR:addressbook-multiget xmlns:D="DAV:" xmlns:CR="urn:ietf:params:xml:ns:carddav"><D:prop><CR:address-data/></D:prop><D:href>/addressbooks/alice/contacts/cy.vcf</D:href></CR:addressbook-multiget>"#).await;
    assert!(body.contains("FN:Cy") && !body.contains("ada"), "{body}");
    assert_eq!(dav("MKCOL", &format!("http://{addr}/addressbooks/alice/work/"), &[], r#"<D:mkcol xmlns:D="DAV:" xmlns:CR="urn:ietf:params:xml:ns:carddav"><D:set><D:prop><D:resourcetype><D:collection/><CR:addressbook/></D:resourcetype></D:prop></D:set></D:mkcol>"#).await.0, 201);
    assert_eq!(dav("MKCOL", &format!("http://{addr}/addressbooks/alice/plain/"), &[], r#"<D:mkcol xmlns:D="DAV:"><D:set><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop></D:set></D:mkcol>"#).await.0, 403);
    assert_eq!(
        dav(
            "MKCALENDAR",
            &format!("http://{addr}/addressbooks/alice/cal/"),
            &[],
            ""
        )
        .await
        .0,
        405
    );
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_client_and_server_agree() {
    let state = state();
    let dir = tempfile::tempdir().unwrap();
    let (sid, addr) = server_in(
        &state,
        "carddav",
        store_policy("carddav", &dir.path().join("db.json"), "contacts"),
        json!({}),
    )
    .await;
    let cid = client_in(
        &state,
        "carddav",
        addr.to_string(),
        json!({"scheme": "http", "username": "alice", "password": "secret"}),
    )
    .await
    .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    let connected = logs(&state, owner, "carddav_connected", 1).await;
    assert_eq!(connected[0].request["collections"][0]["name"], "contacts");
    let send = |a: Value| state.send_to_client(cid, a, Duration::from_secs(15));
    for a in [
        json!({"type":"carddav_put","collection":"contacts","name":"ada.vcf","data":card("ada","Ada Lovelace","EMAIL:ada@example.com\r\n"),"create_only":true}),
        json!({"type":"carddav_put","collection":"contacts","name":"bob.vcf","data":card("bob","Bob","")}),
        json!({"type":"carddav_query","collection":"contacts","property":"EMAIL","text":"example.com"}),
        json!({"type":"carddav_get","collection":"contacts","name":"bob.vcf"}),
        json!({"type":"carddav_delete","collection":"contacts","name":"bob.vcf"}),
        json!({"type":"carddav_list","collection":"contacts"}),
    ] {
        assert!(matches!(
            send(a).await.unwrap(),
            ClientSendOutcome::Sent { .. }
        ));
    }
    let rows = logs(&state, owner, "carddav_response", 6).await;
    let r: Vec<&Value> = rows.iter().map(|r| &r.request).collect();
    assert_eq!(r[0]["status"], 201);
    let q = r[2]["objects"].as_array().unwrap();
    assert!(q.len() == 1 && q[0]["data"].as_str().unwrap().contains("UID:ada"));
    assert!(r[3]["data"].as_str().unwrap().contains("FN:Bob"));
    assert_eq!(r[4]["status"], 204);
    assert_eq!(r[5]["objects"].as_array().unwrap().len(), 1);
    state.remove_client(cid).await;
    state.remove_server(sid).await;
}
