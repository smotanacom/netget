use crate::helpers::p2p as h;
use serde_json::json;
fn handlers() -> Vec<serde_json::Value> {
    vec![
        json!({"event_pattern":"nntp_command_received","handler":{"type":"script","language":"python","code":"import json,sys\ne=json.load(sys.stdin)['event']\na={'type':'send_nntp_response','code':200 if e['command']=='GREETING' else 205,'text':'Ready'} if e['command'] in ['GREETING','QUIT'] else {'type':e.get('answer_with','nntp_article_result'),'accepted':True}\nprint(json.dumps({'actions':[a]}))"}}),
    ]
}
#[tokio::test]
async fn independent_nntplib_post_and_feed() {
    let s = h::state();
    let (id, addr) = h::server_in(&s, "nntp", handlers(), json!({})).await;
    let result = h::peer("nntp", "client", addr).await;
    assert_eq!(result["ok"], true);
    s.remove_server(id).await;
}
#[tokio::test]
async fn cleartext_credentials_and_invalid_feed_errors() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let s = h::state();
    let (id, addr) = h::server_in(&s, "nntp", handlers(), json!({})).await;
    let c = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut read, mut write) = c.into_split();
    let mut r = BufReader::new(&mut read);
    let mut line = String::new();
    r.read_line(&mut line).await.unwrap();
    for (cmd, code) in [
        ("AUTHINFO USER bob", 483),
        ("IHAVE invalid", 501),
        ("TAKETHIS <a@b>", 500),
        ("MODE STREAM", 203),
        ("CHECK invalid", 501),
    ] {
        write
            .write_all(format!("{cmd}\r\n").as_bytes())
            .await
            .unwrap();
        line.clear();
        r.read_line(&mut line).await.unwrap();
        assert!(line.starts_with(&code.to_string()), "{cmd}: {line}");
    }
    s.remove_server(id).await;
}
#[test]
fn header_injection_and_dot_stuffing() {
    use netget::server::nntp::extensions::*;
    assert!(article(&json!({"headers":{"Subject":"bad\r\nPOST"},"body":"hi"})).is_err());
    assert!(message_id("<a@b>\r\nQUIT").is_err());
    assert_eq!(dot_block(".first\n\n").unwrap(), b"..first\r\n\r\n.\r\n");
}
#[tokio::test]
async fn independent_nntplib_tls_authentication() {
    let d = h::tls_certificate().await;
    let s = h::state();
    let(id,addr)=h::server_in(&s,"nntp",handlers(),json!({"use_tls":true,"require_auth":true,"cert_path":d.path().join("cert.pem"),"key_path":d.path().join("key.pem")})).await;
    let script="import nntplib,ssl,sys,json\nc=ssl.create_default_context(cafile=sys.argv[3])\nwith nntplib.NNTP_SSL('localhost',int(sys.argv[2]),ssl_context=c,user='reader',password='secret with spaces',usenetrc=False,timeout=10) as n:\n assert 'AUTHINFO' in n.getcapabilities()\nprint(json.dumps({'ok':True}))";
    let result = tokio::process::Command::new(h::python())
        .args([
            "-c",
            script,
            &addr.ip().to_string(),
            &addr.port().to_string(),
        ])
        .arg(d.path().join("cert.pem"))
        .output()
        .await
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    s.remove_server(id).await;
}
