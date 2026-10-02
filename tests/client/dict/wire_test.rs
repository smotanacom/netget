use netget::client::dict::wire::{self, Request};
use serde_json::json;
use tokio::io::BufReader;
fn request(operation: &str) -> Request {
    Request::from_action(&json!({"operation":operation,"word":"hello"})).unwrap()
}
#[test]
fn requests_quote_text_and_reject_injection_and_oversize() {
    let r =
        Request::from_action(&json!({"operation":"define","word":"quote\"slash\\café"})).unwrap();
    assert_eq!(
        String::from_utf8(r.bytes).unwrap(),
        "DEFINE \"*\" \"quote\\\"slash\\\\café\"\r\n"
    );
    for value in [
        json!({"operation":"define","word":"x\r\nQUIT"}),
        json!({"operation":"match","word":"x","database":"bad\nname"}),
        json!({"operation":"info"}),
        json!({"operation":"define","word":"x".repeat(1024)}),
        json!({"operation":"unknown"}),
    ] {
        assert!(Request::from_action(&value).is_err(), "{value}");
    }
}
#[tokio::test]
async fn definitions_and_listings_unstuff_and_correlate_counts() {
    let mut r=BufReader::new(&b"150 2 definitions retrieved\r\n151 \"hello\" first \"First dictionary\" - text follows\r\n..dot\r\n...two\r\n.literal\r\n.\r\n151 \"hello\" second \"Second\"\r\nOther\r\n.\r\n250 ok\r\n"[..]);
    let v = wire::response(&mut r, &request("define")).await.unwrap();
    assert_eq!(v["definitions"][0]["text"], ".dot\n..two\n.literal");
    assert_eq!(v["definitions"][1]["database"], "second");
    let mut r =
        BufReader::new(&b"152 1 matches found\r\nfirst \"hello world\"\r\n.\r\n250 ok\r\n"[..]);
    assert_eq!(
        wire::response(&mut r, &request("match")).await.unwrap()["entries"][0]["word"],
        "hello world"
    );
    let mut r = BufReader::new(&b"552 no match\r\n"[..]);
    assert_eq!(
        wire::response(&mut r, &request("define")).await.unwrap()["code"],
        552
    );
}
#[tokio::test]
async fn malformed_truncated_mismatched_and_oversize_replies_fail() {
    for data in [
        "150 1 definitions\n",
        "150 4097 definitions\r\n",
        "150 1 definitions\r\n151 \"word\" db \"desc\"\r\ntext\r\n",
        "150 1 definitions\r\n151 \"word\" db\r\n",
        "152 0 matches\r\n.\r\n250 ok\r\n",
        "150 0 definitions\r\n221 bye\r\n",
    ] {
        assert!(
            wire::response(&mut BufReader::new(data.as_bytes()), &request("define"))
                .await
                .is_err(),
            "{data}"
        );
    }
    assert!(wire::response(
        &mut BufReader::new(&b"152 2 matches\r\ndb \"one\"\r\n.\r\n250 ok\r\n"[..]),
        &request("match")
    )
    .await
    .is_err());
    let huge = format!(
        "114 text\r\n{}\r\n.\r\n250 ok\r\n",
        "x".repeat(wire::MAX_RESPONSE_LINE)
    );
    assert!(
        wire::response(&mut BufReader::new(huge.as_bytes()), &request("server"))
            .await
            .is_err()
    );
    let huge = format!(
        "114 text\r\n{}\r\n.\r\n250 ok\r\n",
        format!("{}\r\n", "x".repeat(1022)).repeat(1025)
    );
    assert!(
        wire::response(&mut BufReader::new(huge.as_bytes()), &request("server"))
            .await
            .is_err()
    );
    assert!(
        wire::greeting(&mut BufReader::new(&b"420 unavailable\r\n"[..]))
            .await
            .is_err()
    );
}
