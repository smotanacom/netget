//! NetGet's CardDAV client against Radicale 3.8.1 (independent, unchanged): discovery from
//! /.well-known/carddav, extended MKCOL, PUT, list, an addressbook-query, GET and DELETE.
//! Fails, never skips.
use crate::helpers::dav::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn client_works_against_radicale() {
    let radicale = start_radicale().await.unwrap();
    let state = state();
    let cid = client_in(
        &state,
        "carddav",
        radicale.addr(),
        json!({"scheme": "http", "username": "alice", "password": "secret"}),
    )
    .await
    .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    logs(&state, owner, "carddav_connected", 1).await;
    let send = |a: Value| state.send_to_client(cid, a, Duration::from_secs(20));
    for a in [
        json!({"type":"carddav_make_collection","collection":"contacts","displayname":"Contacts"}),
        json!({"type":"carddav_put","collection":"contacts","name":"ada.vcf","data":"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:ada\r\nFN:Ada Lovelace\r\nEMAIL:ada@example.com\r\nEND:VCARD\r\n","create_only":true}),
        json!({"type":"carddav_put","collection":"contacts","name":"bob.vcf","data":"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:bob\r\nFN:Bob\r\nEMAIL:bob@example.org\r\nEND:VCARD\r\n"}),
        json!({"type":"carddav_list","collection":"contacts"}),
        json!({"type":"carddav_query","collection":"contacts","property":"EMAIL","text":"example.com","match_type":"ends-with"}),
        json!({"type":"carddav_get","collection":"contacts","name":"bob.vcf"}),
        json!({"type":"carddav_delete","collection":"contacts","name":"bob.vcf"}),
        json!({"type":"carddav_list","collection":"contacts"}),
    ] {
        assert!(matches!(
            send(a).await.unwrap(),
            ClientSendOutcome::Sent { .. }
        ));
    }
    let rows = logs(&state, owner, "carddav_response", 8).await;
    let r: Vec<&Value> = rows.iter().map(|r| &r.request).collect();
    assert_eq!(
        (r[0]["status"].as_u64(), r[1]["status"].as_u64()),
        (Some(201), Some(201))
    );
    assert_eq!(r[3]["objects"].as_array().unwrap().len(), 2);
    let q = r[4]["objects"].as_array().unwrap();
    assert!(
        q.len() == 1 && q[0]["data"].as_str().unwrap().contains("UID:ada"),
        "{}",
        r[4]
    );
    assert!(r[5]["data"].as_str().unwrap().contains("FN:Bob"));
    assert_eq!(r[7]["objects"].as_array().unwrap().len(), 1);
    state.remove_client(cid).await;
}
