//! NetGet's EPP server from raw RFC 5734 frames over plain TCP (`tls: false`): the greeting and
//! hello, session rules (2002 before login and on a second login), 2100 for another version,
//! 2307 for an unserved object, 2000 for an unknown command, 2001 for malformed XML and for a
//! DOCTYPE, 2005 for a bad transfer op, poll, a handler answer the server refuses (a
//! session-ending code) failing closed with 2400, an oversized frame closing with 2500, three
//! failed logins closing with 2501, and a handler-less server answering 2400.
use crate::helpers::epp::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn frames_sessions_and_refusals() {
    let state = state();
    let mut params = registrar();
    params["tls"] = json!(false);
    let (sid, addr) = server_in(&state, params).await;

    let mut s = Raw::connect(addr).await;
    let greeting = s.recv().await.unwrap();
    assert!(
        greeting.contains("<greeting>") && greeting.contains("<svID>NetGet EPP</svID>"),
        "{greeting}"
    );
    s.send(r#"<epp xmlns="urn:ietf:params:xml:ns:epp-1.0"><hello/></epp>"#)
        .await;
    assert!(s.recv().await.unwrap().contains("<greeting>"));
    let r = s.command(r#"<check><domain:check xmlns:domain="urn:ietf:params:xml:ns:domain-1.0"><domain:name>a.example</domain:name></domain:check></check>"#).await;
    assert_eq!(code(&r), 2002, "{r}");
    assert!(r.contains("<clTRID>raw-1</clTRID>"), "{r}");
    let r = s
        .command(&LOGIN.replace("<version>1.0", "<version>2.0"))
        .await;
    assert_eq!(code(&r), 2100, "{r}");
    let r = s
        .command(&LOGIN.replace(
            "domain-1.0</objURI>",
            "domain-1.0</objURI><objURI>urn:example:widget-1.0</objURI>",
        ))
        .await;
    assert_eq!(code(&r), 2307, "{r}");
    assert_eq!(code(&s.command(LOGIN).await), 1000);
    assert_eq!(code(&s.command(LOGIN).await), 2002);
    let r = s.command(r#"<check><w:check xmlns:w="urn:example:widget-1.0"><w:name>x</w:name></w:check></check>"#).await;
    assert_eq!(code(&r), 2307, "{r}");
    assert_eq!(code(&s.command("<frobnicate/>").await), 2000);
    let r = s.command(r#"<transfer op="steal"><domain:transfer xmlns:domain="urn:ietf:params:xml:ns:domain-1.0"><domain:name>taken.example</domain:name></domain:transfer></transfer>"#).await;
    assert_eq!(code(&r), 2005, "{r}");
    assert_eq!(code(&s.command(r#"<poll op="req"/>"#).await), 1300);
    // The handler answers delete with 1500, which only the server may say.
    let r = s.command(r#"<delete><domain:delete xmlns:domain="urn:ietf:params:xml:ns:domain-1.0"><domain:name>free.example</domain:name></domain:delete></delete>"#).await;
    assert_eq!(code(&r), 2400, "{r}");
    s.send("<epp><not-closed>").await;
    assert_eq!(code(&s.recv().await.unwrap()), 2001);
    s.send(r#"<?xml version="1.0"?><!DOCTYPE epp [<!ENTITY x "y">]><epp xmlns="urn:ietf:params:xml:ns:epp-1.0"><hello/></epp>"#).await;
    assert_eq!(code(&s.recv().await.unwrap()), 2001);
    let r = s.command("<logout/>").await;
    assert_eq!(code(&r), 1500, "{r}");
    assert!(
        s.recv().await.is_none(),
        "the session stays open after logout"
    );

    // A frame announcing more than 256 KiB is refused before it is read.
    let mut s = Raw::connect(addr).await;
    s.recv().await.unwrap();
    use tokio::io::AsyncWriteExt;
    s.0.write_all(&(300_000u32).to_be_bytes()).await.unwrap();
    assert_eq!(code(&s.recv().await.unwrap()), 2500);
    assert!(s.recv().await.is_none());

    let mut s = Raw::connect(addr).await;
    s.recv().await.unwrap();
    let bad = LOGIN.replace("secret-pw-1", "nope");
    assert_eq!(code(&s.command(&bad).await), 2200);
    assert_eq!(code(&s.command(&bad).await), 2200);
    assert_eq!(code(&s.command(&bad).await), 2501);
    assert!(s.recv().await.is_none());
    state.remove_server(sid).await;

    let (sid, addr) = server_with(&state, None, json!({"tls": false})).await;
    let mut s = Raw::connect(addr).await;
    s.recv().await.unwrap();
    assert_eq!(
        code(&s.command(LOGIN).await),
        1000,
        "without clients every login is accepted"
    );
    let r = s.command(r#"<check><domain:check xmlns:domain="urn:ietf:params:xml:ns:domain-1.0"><domain:name>a.example</domain:name></domain:check></check>"#).await;
    assert_eq!(code(&r), 2400, "{r}");
    state.remove_server(sid).await;
}
