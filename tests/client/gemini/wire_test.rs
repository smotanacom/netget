use netget::client::gemini::wire::{self, Request};
use serde_json::json;
use tokio::io::BufReader;
fn request() -> Request {
    Request::from_action(&json!({"url":"gemini://localhost:1965/dir/page.gmi"})).unwrap()
}
#[test]
fn url_validation_and_input_encoding() {
    let r =
        Request::from_action(&json!({"url":"gemini://localhost/search?old","input":"café + &%"}))
            .unwrap();
    assert_eq!(
        String::from_utf8(r.bytes).unwrap(),
        "gemini://localhost/search?caf%C3%A9%20%2B%20%26%25\r\n"
    );
    for raw in [
        "https://localhost/".to_string(),
        "gemini://user@localhost/".into(),
        "gemini://localhost/#fragment".into(),
        "gemini://localhost/\r\n20 forged".into(),
        format!("gemini://localhost/{}", "x".repeat(1024)),
        "gemini://localhost:0/".into(),
    ] {
        assert!(Request::from_action(&json!({"url":raw})).is_err());
    }
}
#[tokio::test]
async fn gemtext_and_status_classes_are_structured() {
    let r = request();
    let data=b"20 text/gemini; charset=utf-8\r\n# Title\n=> ../about About\n## Small\n### Smallest\n* One\n> Quote\n```art\n=> preformatted\n```\nText\n";
    let v = wire::response(&mut BufReader::new(&data[..]), &r)
        .await
        .unwrap();
    assert_eq!(v["lines"][0]["type"], "heading1");
    assert_eq!(v["lines"][1]["url"], "gemini://localhost:1965/about");
    assert_eq!(v["lines"][6]["text"], "=> preformatted");
    for (line, kind) in [
        ("11 Password\r\n", "input"),
        ("31 ../next\r\n", "redirect"),
        ("44 17\r\n", "temporary_failure"),
        ("51 Missing\r\n", "permanent_failure"),
        ("60 Identity required\r\n", "certificate_required"),
        ("29 text/plain\r\nBody", "success"),
    ] {
        let v = wire::response(&mut BufReader::new(line.as_bytes()), &r)
            .await
            .unwrap();
        assert_eq!(v["kind"], kind);
        if v["status"] == 11 {
            assert_eq!(v["sensitive"], true);
        }
        if v["status"] == 31 {
            assert_eq!(v["url"], "gemini://localhost:1965/next");
        }
        if v["status"] == 44 {
            assert_eq!(v["retry_after_secs"], 17);
        }
    }
}
#[tokio::test]
async fn malformed_unsupported_and_excessive_responses_fail() {
    let r = request();
    for data in [
        b"20 text/gemini\nbody".to_vec(),
        b"70 invalid\r\n".to_vec(),
        b"20 text/plain\r\n\xff".to_vec(),
        b"20 image/png\r\n".to_vec(),
        b"20 text/plain; charset=iso-8859-1\r\n".to_vec(),
        b"30 \r\n".to_vec(),
        b"44 not-seconds\r\n".to_vec(),
        b"20 text/plain\rBAD\r\n".to_vec(),
        vec![b'x'; 1030],
        format!("20 text/plain\r\n{}", "x".repeat(wire::MAX_BODY_BYTES + 1)).into_bytes(),
        format!("20 text/gemini\r\n{}", "line\n".repeat(wire::MAX_LINES + 1)).into_bytes(),
    ] {
        assert!(wire::response(&mut BufReader::new(data.as_slice()), &r)
            .await
            .is_err());
    }
}
