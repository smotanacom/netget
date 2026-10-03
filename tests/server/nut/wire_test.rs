use netget::server::nut::wire::*;
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
#[test]
fn quoting_and_request_validation() {
    let v = "UPS \"rack\" \\ main";
    assert_eq!(split(&quote(v).unwrap()).unwrap(), vec![v]);
    for bad in [
        "unterminated \"value",
        "GET VAR ups v\nLOGOUT",
        "GET \"v\"junk",
        "PASSWORD \"a\\z\"",
        "",
    ] {
        assert!(split(bad).is_err(), "{bad}");
    }
    assert!(quote("ups\nSECOND").is_err());
    assert!(
        Request::from_action(&json!({"operation":"get_var","ups":"x\ny","name":"ups.status"}))
            .is_err()
    );
    assert!(Request::from_action(&json!({"operation":"username","value":"x\nLOGOUT"})).is_err());
    for (line, op) in [
        ("LIST UPS", "list_ups"),
        ("LIST VAR ups", "list_var"),
        ("GET VAR ups ups.status", "get_var"),
        ("INSTCMD ups test.panel.start", "instcmd"),
        ("SET VAR ups ups.delay.start \"12\"", "set_var"),
    ] {
        let r = Request::parse(line).unwrap();
        assert_eq!(r.operation, op);
        assert_eq!(r.encode().unwrap(), format!("{line}\n"));
    }
}
#[test]
fn renders_correlated_lists_and_rejects_invalid_success() {
    let r = Request::parse("LIST VAR rack1").unwrap();
    assert_eq!(
        render(
            &r,
            &json!({"entries":[{"name":"ups.status","value":"OB LB"}]})
        )
        .unwrap(),
        "BEGIN LIST VAR rack1\nVAR rack1 ups.status \"OB LB\"\nEND LIST VAR rack1\n"
    );
    assert!(render(&r, &json!({"value":"missing entries"})).is_err());
    assert!(render(
        &r,
        &json!({"entries":vec![json!({"name":"x","value":"y"});MAX_ENTRIES+1]})
    )
    .is_err());
    assert!(render(
        &r,
        &json!({"entries":[{"name":"x","value":"x".repeat(MAX_LINE_BYTES)}]})
    )
    .is_err());
    assert!(render(
        &r,
        &json!({"entries":vec![json!({"name":"x","value":"y".repeat(300)});MAX_ENTRIES]})
    )
    .is_err());
    assert!(render(
        &Request::parse("SET VAR rack1 x \"3\"").unwrap(),
        &json!({})
    )
    .is_err());
    assert!(error_reply("BOGUS\nOK").is_err());
}
#[tokio::test]
async fn reader_rejects_truncation_and_limits_and_has_whole_line_deadline() {
    let mut eof = BufReader::new(&b"GET VAR x y"[..]);
    assert!(read_line(&mut eof, Duration::from_secs(1)).await.is_err());
    let large = vec![b'x'; MAX_LINE_BYTES + 1];
    assert!(read_line(
        &mut BufReader::new(large.as_slice()),
        Duration::from_secs(1)
    )
    .await
    .is_err());
    let (mut sender, receiver) = tokio::io::duplex(64);
    let feed = tokio::spawn(async move {
        loop {
            if sender.write_all(b"x").await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    assert!(
        read_line(&mut BufReader::new(receiver), Duration::from_millis(25))
            .await
            .is_err()
    );
    feed.abort();
}
#[tokio::test]
async fn client_correlates_rows_and_rejects_malformed_or_oversized_responses() {
    let r = Request::parse("LIST VAR rack1").unwrap();
    for bytes in [
        "BEGIN LIST VAR other\n",
        "BEGIN LIST VAR rack1\nVAR other x \"1\"\nEND LIST VAR rack1\n",
        "BEGIN LIST VAR rack1\nEND LIST VAR other\n",
        "BEGIN LIST VAR rack1\n",
    ] {
        assert!(
            read_response(&mut BufReader::new(bytes.as_bytes()), &r)
                .await
                .is_err(),
            "{bytes}"
        );
    }
    let mut data = String::from("BEGIN LIST VAR rack1\n");
    for _ in 0..=MAX_ENTRIES {
        data.push_str("VAR rack1 x \"1\"\n");
    }
    data.push_str("END LIST VAR rack1\n");
    assert!(read_response(&mut BufReader::new(data.as_bytes()), &r)
        .await
        .is_err());
    let response = read_response(
        &mut BufReader::new(
            &b"BEGIN LIST VAR rack1\nVAR rack1 ups.status \"OL\"\nEND LIST VAR rack1\n"[..],
        ),
        &r,
    )
    .await
    .unwrap();
    assert_eq!(response["entries"][0]["value"], "OL");
}

#[test]
fn idle_timeout_validates_nonzero_and_upper_bound() {
    use netget::server::nut::{idle_timeout, IDLE_TIMEOUT, MAX_IDLE_TIMEOUT_SECS};
    assert_eq!(idle_timeout(None).unwrap(), IDLE_TIMEOUT);
    assert_eq!(
        idle_timeout(Some(MAX_IDLE_TIMEOUT_SECS)).unwrap().as_secs(),
        MAX_IDLE_TIMEOUT_SECS
    );
    for n in [0, MAX_IDLE_TIMEOUT_SECS + 1, u64::MAX] {
        assert!(idle_timeout(Some(n)).is_err());
    }
}

#[tokio::test]
async fn whole_response_deadline_and_byte_cap_bound_a_streaming_list() {
    let request = Request::parse("LIST VAR ups").unwrap();
    let (mut sender, receiver) = tokio::io::duplex(1024);
    let producer = tokio::spawn(async move {
        sender.write_all(b"BEGIN LIST VAR ups\n").await.unwrap();
        loop {
            if sender.write_all(b"VAR ups x \"1\"\n").await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    assert!(read_response_with_timeout(
        &mut BufReader::new(receiver),
        &request,
        Duration::from_millis(25)
    )
    .await
    .is_err());
    producer.abort();
    let mut body = "BEGIN LIST VAR ups\n".to_owned();
    for _ in 0..MAX_ENTRIES {
        body.push_str(&format!("VAR ups x \"{}\"\n", "y".repeat(300)));
    }
    body.push_str("END LIST VAR ups\n");
    assert!(
        read_response(&mut BufReader::new(body.as_bytes()), &request)
            .await
            .is_err()
    );
}
