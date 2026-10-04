//! The ManageSieve grammar and server from the wire: quoted strings, both literal forms and
//! escapes parse incrementally; NetGet's client against NetGet's server covers every command;
//! raw refusals: commands before login, a bad name, an oversized literal, three failed logins,
//! a continuation-style AUTHENTICATE, and NOOP's tag.
use crate::helpers::managesieve::*;
use netget::server::managesieve::proto::{self, Arg};
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[test]
fn grammar() {
    let line = b"PUTSCRIPT \"a \\\"b\\\"\" {5+}\r\nkeep;\r\n";
    for cut in 0..line.len() {
        assert!(proto::parse(&line[..cut]).unwrap().is_none(), "cut {cut}");
    }
    let (args, used) = proto::parse(line).unwrap().unwrap();
    assert_eq!(used, line.len());
    assert_eq!(
        args,
        vec![
            Arg::Atom("PUTSCRIPT".into()),
            Arg::String(b"a \"b\"".to_vec()),
            Arg::String(b"keep;".to_vec())
        ]
    );
    let (args, _) = proto::parse(b"HAVESPACE {3}\r\nabc 42\r\n")
        .unwrap()
        .unwrap();
    assert_eq!(args[1..], [Arg::String(b"abc".to_vec()), Arg::Number(42)]);
    assert!(proto::parse(format!("X {{{}+}}\r\n", proto::MAX_LITERAL + 1).as_bytes()).is_err());
    assert!(proto::parse(b"X \"a\nb\"\r\n").is_err());
    assert_eq!(proto::string(b"plain", false), b"\"plain\"");
    assert_eq!(
        proto::string(b"two\r\nlines", true),
        b"{10+}\r\ntwo\r\nlines"
    );
    assert!(
        proto::valid_name("vacation".as_bytes())
            && !proto::valid_name(b"")
            && !proto::valid_name(b"a\tb")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_client_against_netget_server() {
    let dir = tempfile::tempdir().unwrap();
    let state = state();
    let (sid, addr) = server_in(&state, policy(&dir.path().join("scripts.json"))).await;
    let cid = client_in(
        &state,
        addr.to_string(),
        json!({"user": "alice", "password": "secret"}),
    )
    .await
    .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    let connected = &logs(&state, owner, "managesieve_connected", 1).await[0];
    assert_eq!(connected["implementation"], "NetGet");
    let script = "require \"fileinto\";\nfileinto \"Junk\";\n";
    for a in [
        json!({"type": "managesieve_put", "name": "junk filter", "script": script}),
        json!({"type": "managesieve_put", "name": "bad", "script": "bogus;\n"}),
        json!({"type": "managesieve_set_active", "name": "junk filter"}),
        json!({"type": "managesieve_list"}),
        json!({"type": "managesieve_get", "name": "junk filter"}),
        json!({"type": "managesieve_delete", "name": "junk filter"}),
        json!({"type": "managesieve_have_space", "name": "x", "size": 200000}),
    ] {
        let r = state
            .send_to_client(cid, a.clone(), Duration::from_secs(20))
            .await
            .unwrap();
        assert!(matches!(r, ClientSendOutcome::Sent { .. }), "{a}: {r:?}");
    }
    let r: Vec<Value> = logs(&state, owner, "managesieve_response", 7).await;
    assert_eq!(
        (r[0]["command"].as_str(), r[0]["status"].as_str()),
        (Some("PUTSCRIPT"), Some("OK"))
    );
    assert_eq!(r[1]["status"], "NO");
    assert!(r[1]["message"].as_str().unwrap().contains("bogus"));
    assert_eq!(
        r[3]["scripts"],
        json!([{"name": "junk filter", "active": true}])
    );
    assert_eq!(r[4]["script"], script);
    assert_eq!(
        (r[5]["status"].as_str(), r[5]["code"].as_str()),
        (Some("NO"), Some("ACTIVE"))
    );
    assert_eq!(r[6]["code"], "QUOTA/MAXSIZE");
    state.remove_client(cid).await;
    state.remove_server(sid).await;
}

async fn read_until(s: &mut TcpStream, needle: &str) -> String {
    let mut text = String::new();
    tokio::time::timeout(Duration::from_secs(20), async {
        while !text.contains(needle) {
            let mut b = [0u8; 4096];
            let n = s.read(&mut b).await.unwrap();
            assert!(n > 0, "closed before {needle:?}: {text}");
            text.push_str(&String::from_utf8_lossy(&b[..n]));
        }
    })
    .await
    .unwrap_or_else(|_| panic!("never saw {needle:?}: {text}"));
    text
}

#[tokio::test(flavor = "multi_thread")]
async fn raw_refusals() {
    let dir = tempfile::tempdir().unwrap();
    let state = state();
    let (sid, addr) = server_in(&state, policy(&dir.path().join("scripts.json"))).await;
    let plain = |p: &str| {
        base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            format!("\0alice\0{p}"),
        )
    };

    let mut s = TcpStream::connect(addr).await.unwrap();
    let greeting = read_until(&mut s, "OK \"NetGet ManageSieve ready.\"\r\n").await;
    assert!(greeting.contains("\"SASL\" \"PLAIN\"") && !greeting.contains("STARTTLS"));
    s.write_all(b"LISTSCRIPTS\r\n").await.unwrap();
    read_until(&mut s, "NO \"Authenticate first\"").await;
    s.write_all(b"NOOP \"t1\"\r\n").await.unwrap();
    read_until(&mut s, "OK (TAG \"t1\") \"Done\"").await;
    // SASL with a continuation instead of an initial response.
    s.write_all(b"AUTHENTICATE \"PLAIN\"\r\n").await.unwrap();
    read_until(&mut s, "\"\"\r\n").await;
    s.write_all(format!("\"{}\"\r\n", plain("secret")).as_bytes())
        .await
        .unwrap();
    read_until(&mut s, "OK").await;
    s.write_all(b"GETSCRIPT \"bad\\\\name\x01\"\r\n")
        .await
        .unwrap();
    read_until(&mut s, "NO \"Invalid script name\"").await;
    s.write_all(format!("PUTSCRIPT \"big\" {{{}+}}\r\n", proto::MAX_LITERAL + 1).as_bytes())
        .await
        .unwrap();
    read_until(&mut s, "BYE \"Protocol error\"").await;

    let mut s = TcpStream::connect(addr).await.unwrap();
    read_until(&mut s, "ready.").await;
    for i in 1..=3 {
        s.write_all(format!("AUTHENTICATE \"PLAIN\" \"{}\"\r\n", plain("nope")).as_bytes())
            .await
            .unwrap();
        read_until(
            &mut s,
            if i < 3 {
                "NO \"Authentication failed\""
            } else {
                "BYE \"Too many failed logins\""
            },
        )
        .await;
    }
    let mut b = [0u8; 1];
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(10), s.read(&mut b))
            .await
            .unwrap(),
        Ok(0) | Err(_)
    ));
    state.remove_server(sid).await;
}
