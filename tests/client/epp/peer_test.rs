//! NetGet's EPP client against a registry built on epp-lib v0.2.0 (the Swedish Internet
//! Foundation's Go EPP server library, independent, unchanged), driven by its handler: TLS with
//! the registry's certificate, the greeting and login, a domain check, contact, host and domain
//! creates, info, renew with the expiry info reported, a transfer request, a duplicate create
//! refused, logout. epp-lib routes by namespace URI, so a command NetGet rendered in the wrong
//! namespace would close the session instead of being answered. A wrong password and an
//! untrusted certificate never connect. Fails, never skips.
use crate::helpers::epp::*;
use netget::state::AccessLogOwner;
use serde_json::{json, Value};

const DRIVER: &str = r#"import json,sys
i=json.load(sys.stdin); k=i['event_type_id']; e=i['event']
def out(*a): print(json.dumps({'actions':list(a)})); sys.exit()
if k=='epp_connected': out({'type':'epp_check','object':'domain','names':['taken.example','mine.example']})
c,o,code,d=e.get('command'),e.get('object'),e.get('code'),e.get('data') or {}
if c=='check': out({'type':'epp_create_contact','id':'c-2001','name':'Grace Hopper','street':['1 Compiler Way'],'city':'Arlington','cc':'US','email':'grace@example.com','auth_info':'c-pw-2'})
if c=='create' and o=='contact': out({'type':'epp_create_host','name':'ns1.mine.example','addrs':[{'ip':'v4','addr':'192.0.2.10'}]})
if c=='create' and o=='host': out({'type':'epp_create_domain','name':'mine.example','period':2,'registrant':'c-2001','contacts':[{'type':'admin','id':'c-2001'}],'ns':['ns1.mine.example'],'auth_info':'d-pw-2'})
if c=='create' and o=='domain' and code==1000: out({'type':'epp_info','object':'domain','name':'mine.example'})
if c=='info' and d.get('name')=='mine.example': out({'type':'epp_renew','name':'mine.example','cur_exp_date':d['exDate'][:10],'period':1})
if c=='renew': out({'type':'epp_transfer','op':'request','name':'taken.example','auth_info':'move-me-1'})
if c=='transfer': out({'type':'epp_create_domain','name':'mine.example','registrant':'c-2001','auth_info':'x-pw-3'})
if c=='create' and o=='domain': out({'type':'epp_logout'})
out()
"#;

#[tokio::test(flavor = "multi_thread")]
async fn netget_against_epp_lib() {
    let registry = Registry::start().await;
    let state = state();
    let remote = format!("localhost:{}", registry.port);
    let params =
        json!({"client_id": "registrar1", "password": "secret-pw-1", "ca_cert_path": registry.ca});
    let id = client_in(&state, remote.clone(), script(DRIVER), params.clone())
        .await
        .unwrap();
    let owner = AccessLogOwner::Client(id.as_u32());
    let response = |want: fn(&Value) -> bool| wait_for(&state, owner, "epp_response", want);

    let connected = wait_for(&state, owner, "epp_connected", |_| true).await;
    assert_eq!(connected["sv_id"], "epp-lib test registry", "{connected}");
    assert_eq!(connected["login"]["code"], 1000, "{connected}");
    let check = response(|e| e["command"] == "check").await;
    assert_eq!(
        check["data"]["results"],
        json!([{"name": "taken.example", "available": false, "reason": "In use"}, {"name": "mine.example", "available": true, "reason": null}]),
        "{check}"
    );
    assert!(
        check["cl_trid"]
            .as_str()
            .is_some_and(|t| t.starts_with("NG-C-")),
        "{check}"
    );
    assert!(
        check["sv_trid"]
            .as_str()
            .is_some_and(|t| t.starts_with("REG-")),
        "{check}"
    );
    let contact = response(|e| e["command"] == "create" && e["object"] == "contact").await;
    assert_eq!(
        (contact["code"].clone(), contact["data"]["id"].clone()),
        (json!(1000), json!("c-2001")),
        "{contact}"
    );
    let host = response(|e| e["command"] == "create" && e["object"] == "host").await;
    assert_eq!(host["code"], 1000, "{host}");
    let created =
        response(|e| e["command"] == "create" && e["object"] == "domain" && e["code"] == 1000)
            .await;
    assert_eq!(created["data"]["name"], "mine.example", "{created}");
    let info = response(|e| e["command"] == "info").await;
    assert_eq!(info["data"]["registrant"], "c-2001", "{info}");
    assert_eq!(
        info["data"]["contacts"],
        json!([{"type": "admin", "id": "c-2001"}]),
        "{info}"
    );
    assert_eq!(info["data"]["ns"], json!(["ns1.mine.example"]), "{info}");
    assert_eq!(info["data"]["authInfo"], "d-pw-2", "{info}");
    assert_eq!(info["data"]["status"], json!(["ok"]), "{info}");
    let renew = response(|e| e["command"] == "renew").await;
    assert_eq!(
        renew["code"], 1000,
        "the registry accepted the expiry the client reported: {renew}"
    );
    let transfer = response(|e| e["command"] == "transfer").await;
    assert_eq!(
        (
            transfer["code"].clone(),
            transfer["data"]["trStatus"].clone()
        ),
        (json!(1001), json!("pending")),
        "{transfer}"
    );
    assert_eq!(transfer["data"]["acID"], "OtherReg", "{transfer}");
    let duplicate = response(|e| e["command"] == "create" && e["code"] == 2302).await;
    assert_eq!(duplicate["object"], "domain", "{duplicate}");
    let logout = response(|e| e["command"] == "logout").await;
    assert_eq!(logout["code"], 1500, "{logout}");

    let mut wrong = params.clone();
    wrong["password"] = json!("nope");
    let refused = client_in(&state, remote.clone(), script(DRIVER), wrong)
        .await
        .unwrap_err();
    assert!(format!("{refused:#}").contains("2200"), "{refused:#}");
    let mut untrusted = params;
    untrusted.as_object_mut().unwrap().remove("ca_cert_path");
    assert!(client_in(&state, remote, script(DRIVER), untrusted)
        .await
        .is_err());
}
