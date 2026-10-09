//! pyepp 0.2.0 (InternetNZ's Python registrar client, independent, unchanged) against NetGet's
//! EPP server over TLS with the certificate the server publishes: the greeting, a command
//! refused before login, a refused then accepted login, domain check, contact, host and domain
//! creates, domain info, an info for a missing domain, renew, a transfer request accepted and
//! one refused, logout. Fails, never skips.
use crate::helpers::epp::*;
use netget::state::AccessLogOwner;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn pyepp_against_netget() {
    let dir = tempfile::tempdir().unwrap();
    let state = state();
    let (sid, addr) = server_in(&state, registrar()).await;
    let ca = certificate(&state, sid, dir.path()).await;
    let r = pyepp(addr.port(), &ca).await;

    assert_eq!(r["sv_id"], "NetGet EPP", "{r}");
    assert_eq!(
        r["obj_uris"],
        json!([
            "urn:ietf:params:xml:ns:domain-1.0",
            "urn:ietf:params:xml:ns:host-1.0",
            "urn:ietf:params:xml:ns:contact-1.0"
        ]),
        "{r}"
    );
    assert_eq!(r["before_login"], 2002, "{r}");
    assert!(
        r["bad_login"].as_str().is_some_and(|e| e.contains("2200")),
        "{r}"
    );
    assert_eq!(r["login"], 1000, "{r}");
    assert_eq!(
        r["check"],
        json!({"taken.example": {"avail": false, "reason": "In use"}, "free.example": {"avail": true, "reason": null}}),
        "{r}"
    );
    assert_eq!(r["check_cltrid"], "TR-check-1", "{r}");
    assert!(
        r["check_svtrid"]
            .as_str()
            .is_some_and(|s| s.starts_with("NG-")),
        "{r}"
    );
    assert_eq!(
        (r["contact_create"].clone(), r["host_create"].clone()),
        (json!(1000), json!(1000)),
        "{r}"
    );
    assert_eq!(
        r["domain_create"],
        json!({"code": 1000, "ex_date": "2028-10-04T12:00:00.0Z"}),
        "{r}"
    );
    assert_eq!(
        r["info"],
        json!({"code": 1000, "name": "taken.example", "registrant": "other-1", "admin": "other-1", "status": ["clientTransferProhibited"], "host": ["ns1.other.example"], "sponsor": "OtherReg", "expiry": "2030-01-02T03:04:05.0Z", "roid": "TAKEN1-REP"}),
        "{r}"
    );
    assert_eq!(
        r["missing"],
        json!({"code": 2303, "reason": "No such domain"}),
        "{r}"
    );
    assert_eq!(
        r["renew"],
        json!({"code": 1000, "ex_date": "2029-10-04T12:00:00.0Z"}),
        "{r}"
    );
    assert_eq!(
        r["transfer"],
        json!({"code": 1001, "status": "pending"}),
        "{r}"
    );
    assert_eq!(r["bad_transfer"], 2202, "{r}");
    assert_eq!(r["logout"], 1500, "{r}");

    // What pyepp sent, as the handler saw it.
    let owner = AccessLogOwner::Server(sid.as_u32());
    let create = wait_for(&state, owner, "epp_command", |e| {
        e["command"] == "create" && e["object"] == "domain"
    })
    .await;
    assert_eq!(create["client_id"], "registrar1");
    assert_eq!(create["fields"]["period"], json!({"value": 2, "unit": "y"}));
    assert_eq!(create["fields"]["ns"], json!(["ns1.free.example"]));
    assert_eq!(
        create["fields"]["contacts"],
        json!([{"type": "admin", "id": "c-1001"}])
    );
    let contact = wait_for(&state, owner, "epp_command", |e| e["object"] == "contact").await;
    assert_eq!(
        (
            contact["fields"]["city"].clone(),
            contact["fields"]["cc"].clone(),
            contact["fields"]["email"].clone()
        ),
        (json!("London"), json!("GB"), json!("ada@example.com"))
    );
    let host = wait_for(&state, owner, "epp_command", |e| e["object"] == "host").await;
    assert_eq!(
        host["fields"]["addrs"],
        json!([{"ip": "v4", "addr": "192.0.2.53"}])
    );
    let renew = wait_for(&state, owner, "epp_command", |e| e["command"] == "renew").await;
    assert_eq!(renew["fields"]["cur_exp_date"], "2028-10-04");
    // The password never reaches the handler.
    assert!(state
        .list_access_logs_for(Some(owner), None)
        .await
        .iter()
        .all(|e| !e.request.to_string().contains("secret-pw-1")));
    state.remove_server(sid).await;
}
