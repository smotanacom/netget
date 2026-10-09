use crate::helpers::p2p as h;
use serde_json::json;
#[tokio::test]
async fn independent_ncdc_hub_login_chat() {
    let s = h::state();
    let(id,addr)=h::server_in(&s,"adc",vec![json!({"event_pattern":"adc_request","handler":{"type":"static","actions":[{"type":"adc_reply","accepted":true}]}})],json!({})).await;
    assert_eq!(h::peer("adc", "client", addr).await["ok"], true);
    let rows = h::logs(
        &s,
        netget::state::AccessLogOwner::Server(id.as_u32()),
        "adc_request",
        2,
    )
    .await;
    assert!(rows.iter().any(|r| r.request["operation"] == "identify"));
    assert!(rows.iter().any(|r| r.request["operation"] == "chat"));
    s.remove_server(id).await;
}
#[test]
fn invalid_identities_and_escape_injection() {
    use netget::server::adc::codec::*;
    assert!(cid(&"A".repeat(38)).is_err());
    assert!(cid(&"Z".repeat(39)).is_err());
    assert!(words(b"BMSG AAAA bad\\q\n").is_err());
    assert!(words(b"BMSG AAAA text\r\n").is_err());
    assert_eq!(
        words(&encode("BMSG", &["AAAA".into(), "hello world\\x\n".into()]).unwrap()).unwrap()[2],
        "hello world\\x\n"
    );
}

#[tokio::test]
async fn malformed_negotiation_and_owner_shutdown() {
    h::malformed_and_owner_shutdown("adc", b"HSUP ADINVALID\n").await;
}
