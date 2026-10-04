//! RFC 9082 parsing and RFC 9083 envelopes without a network, and the NetGet pair.
use crate::helpers::rdap::*;
use netget::server::rdap::query::{self, Answer, Lookup, Query, Search};
use netget::state::AccessLogOwner;
use serde_json::json;

#[test]
fn queries_are_normalized_or_refused_before_a_handler_sees_them() {
    assert_eq!(
        query::parse("/domain/Example.COM.", None).unwrap(),
        Query::Lookup {
            kind: Lookup::Domain,
            value: "example.com".into()
        }
    );
    assert_eq!(
        query::parse("/ip/192.0.2.0/24", None).unwrap(),
        Query::Lookup {
            kind: Lookup::Ip,
            value: "192.0.2.0/24".into()
        }
    );
    assert_eq!(
        query::parse("/ip/2001:0db8::1", None).unwrap(),
        Query::Lookup {
            kind: Lookup::Ip,
            value: "2001:db8::1".into()
        }
    );
    assert_eq!(
        query::parse("/autnum/64496", None).unwrap(),
        Query::Lookup {
            kind: Lookup::Autnum,
            value: "64496".into()
        }
    );
    assert_eq!(
        query::parse("/entity/EX%2DREG", None).unwrap(),
        Query::Lookup {
            kind: Lookup::Entity,
            value: "EX-REG".into()
        }
    );
    assert_eq!(
        query::parse("/domains", Some("name=exa%2A.com")).unwrap(),
        Query::Search {
            kind: Search::Domains,
            parameter: "name".into(),
            value: "exa*.com".into()
        }
    );
    assert_eq!(query::parse("/help", None).unwrap(), Query::Help);
    for (path, q) in [
        ("/domain/exa_mple.com", None),
        ("/domain/a..b", None),
        ("/domain/x.*.com", None),
        ("/ip/192.0.2.0/33", None),
        ("/ip/999.0.2.0", None),
        ("/autnum/AS1", None),
        ("/autnum/4294967296", None),
        ("/entity/a%0Ab", None),
        ("/entity/%ZZ", None),
        ("/domains", Some("name=a&nsIp=1.2.3.4")),
        ("/domains", Some("fn=x")),
        ("/nameservers", None),
        ("/domain/a/b", None),
        ("/whois/example.com", None),
    ] {
        assert!(query::parse(path, q).is_err(), "{path} {q:?} was accepted");
    }
    let long = format!("/entity/{}", "x".repeat(256));
    assert!(query::parse(&long, None).is_err());
}

#[test]
fn answers_must_match_the_query_and_carry_the_conformance_rdap_requires() {
    let domain = Query::Lookup {
        kind: Lookup::Domain,
        value: "example.com".into(),
    };
    let Answer::Body { status, body } = query::answer(&domain, &json!({"object":{"objectClassName":"domain","ldhName":"example.com","rdapConformance":["cidr0"]}})).unwrap() else { panic!() };
    assert_eq!(status, 200);
    assert_eq!(body["rdapConformance"], json!(["rdap_level_0", "cidr0"]));
    for bad in [
        json!({"object":{"objectClassName":"nameserver"}}),
        json!({"object":{"ldhName":"example.com"}}),
        json!({"object":"text"}),
        json!({"results":[]}),
        json!({"not_found":true,"object":{}}),
        json!({"error":{"code":418}}),
        json!({"redirect":"ftp://x"}),
        json!({"redirect":"http://x\r\nSet-Cookie: a"}),
        json!({"surprise":true}),
        json!({}),
    ] {
        assert!(query::answer(&domain, &bad).is_err(), "{bad} was accepted");
    }
    let search = Query::Search {
        kind: Search::Domains,
        parameter: "name".into(),
        value: "e*".into(),
    };
    let Answer::Body { body, .. } =
        query::answer(&search, &json!({"results":[{"objectClassName":"domain"}]})).unwrap()
    else {
        panic!()
    };
    assert!(body["domainSearchResults"].is_array());
    assert!(query::answer(&search, &json!({"results":[{"objectClassName":"entity"}]})).is_err());
    assert!(
        query::answer(&search, &json!({"redirect":"http://x/"})).is_err(),
        "searches are not redirected"
    );
    let Answer::Body { status, body } = query::answer(
        &domain,
        &json!({"error":{"code":403,"title":"Forbidden","description":"private"}}),
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(
        (
            status,
            body["errorCode"].as_u64(),
            body["description"][0].as_str()
        ),
        (403, Some(403), Some("private"))
    );
    assert!(query::answer(&Query::Help, &json!({"object":{"title":"no notices"}})).is_err());
    let deep = (0..40).fold(json!(1), |v, _| json!([v]));
    assert!(query::answer(
        &domain,
        &json!({"object":{"objectClassName":"domain","x":deep}})
    )
    .is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_client_walks_netget_server_through_lookup_search_error_and_referral() {
    let state = state();
    let (sid, addr) = server_in(&state, registry_policy(), json!({"base_path": "/rdap"})).await;
    let quiet = vec![
        json!({"event_pattern":"rdap_ready","handler":{"type":"static","actions":[]}}),
        json!({"event_pattern":"rdap_response","handler":{"type":"static","actions":[]}}),
    ];
    let cid = client_in(
        &state,
        addr.to_string(),
        quiet,
        json!({"base_path": "/rdap"}),
    )
    .await;
    for q in [
        json!({"type":"rdap_query","query_type":"domain","value":"EXAMPLE.com"}),
        json!({"type":"rdap_query","query_type":"domains","value":"exa*"}),
        json!({"type":"rdap_query","query_type":"domain","value":"nope.example"}),
        json!({"type":"rdap_query","query_type":"domain","value":"moved.example"}),
        json!({"type":"rdap_query","query_type":"autnum","value":"64511"}),
    ] {
        state
            .send_to_client(cid, q, std::time::Duration::from_secs(10))
            .await
            .unwrap();
    }
    let rows = logs(
        &state,
        AccessLogOwner::Client(cid.as_u32()),
        "rdap_response",
        5,
    )
    .await;
    assert_eq!(rows[0].request["status"], 200);
    assert_eq!(rows[0].request["object"]["ldhName"], "example.com");
    assert_eq!(
        rows[1].request["object"]["domainSearchResults"][0]["handle"],
        "EX-1"
    );
    assert_eq!(
        (
            rows[2].request["status"].as_u64(),
            rows[2].request["error"]["errorCode"].as_u64()
        ),
        (Some(404), Some(404))
    );
    assert_eq!(rows[3].request["status"], 302);
    assert_eq!(
        rows[3].request["redirect"],
        "http://127.0.0.1:9/rdap/domain/moved.example"
    );
    assert_eq!(rows[4].request["error"]["description"][0], "private range");
    let refused = state
        .send_to_client(
            cid,
            json!({"type":"rdap_query","query_type":"autnum","value":"AS1"}),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(matches!(
        refused,
        netget::state::client_handles::ClientSendOutcome::Rejected { .. }
    ));
    state.remove_client(cid).await;
    state.remove_server(sid).await;
}
