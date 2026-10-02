use netget::client::beanstalkd::wire::*;
use serde_json::json;
use tokio::io::BufReader;
#[test]
fn requests_use_utf8_byte_counts_and_reject_injection_overflow_and_oversize() {
    let r = Request::from_action(&json!({"operation":"put","body":"résumé\r\nOK"})).unwrap();
    assert_eq!(
        String::from_utf8(r.bytes).unwrap(),
        "put 1024 0 60 12\r\nrésumé\r\nOK\r\n"
    );
    for bad in [
        json!({"operation":"use","tube":"x\r\nquit"}),
        json!({"operation":"put","body":"x","priority":4294967296u64}),
        json!({"operation":"put","body":"x".repeat(65536)}),
        json!({"operation":"reserve","timeout_secs":26}),
        json!({"operation":"delete","id":0}),
    ] {
        assert!(Request::from_action(&bad).is_err(), "{bad}");
    }
}
#[tokio::test]
async fn exact_lengths_correlation_truncation_and_bounds() {
    let request = Request::from_action(&json!({"operation":"reserve"})).unwrap();
    for data in [
        b"DELETED\r\n".to_vec(),
        b"RESERVED 0 0\r\n\r\n".to_vec(),
        b"RESERVED +1 0\r\n\r\n".to_vec(),
        b"RESERVED 1 4\r\nx\r\n".to_vec(),
        b"RESERVED 1 1\r\nx!!".to_vec(),
        b"RESERVED 1 65536\r\n".to_vec(),
        b"RESERVED 1 1\nx\r\n".to_vec(),
        vec![b'x'; 225],
        b"RESERVED 1 1\r\n\xff\r\n".to_vec(),
    ] {
        assert!(response(&mut BufReader::new(data.as_slice()), &request)
            .await
            .is_err());
    }
    let mut data = &b"RESERVED 17 10\r\nx\r\nDELETED\r\n"[..];
    let result = response(&mut BufReader::new(&mut data), &request)
        .await
        .unwrap();
    assert_eq!(result["body"], "x\r\nDELETED");
    let stats = Request::from_action(&json!({"operation":"stats"})).unwrap();
    assert!(
        response(&mut BufReader::new(&b"OK 1048577\r\n"[..]), &stats)
            .await
            .is_err()
    );
}
#[test]
fn flat_yaml_is_typed_and_bounds_reject_alias_graphs_and_duplicate_keys() {
    assert_eq!(
        parse_yaml("---\ncount: 3\nname: \"images\"\n", false).unwrap(),
        json!({"count":3,"name":"images"})
    );
    for bad in [
        "---\na: 1\na: 2\n",
        "---\na: {b: 1}\n",
        "---\na: &x 1\nb: *x\n",
    ] {
        assert!(parse_yaml(bad, false).is_err());
    }
    assert!(parse_yaml(&format!("---\n{}", "- tube\n".repeat(513)), true).is_err());
}

#[tokio::test]
async fn specific_job_replies_must_match_the_requested_id() {
    let request = Request::from_action(&json!({"operation":"peek","id":42})).unwrap();
    assert!(
        response(&mut BufReader::new(&b"FOUND 43 0\r\n\r\n"[..]), &request)
            .await
            .is_err()
    );
}

#[test]
fn scalar_parser_rejects_nested_and_single_value_alias_graphs() {
    for scalar in [
        format!("{}1{}", "[".repeat(512), "]".repeat(512)),
        "[&a [1,2], &b [*a,*a,*a,*a], &c [*b,*b,*b,*b], *c]".into(),
        "&recursive [*recursive]".into(),
        "{deep: {nested: [1,2]}}".into(),
    ] {
        assert!(parse_yaml(&format!("---\nvalue: {scalar}\n"), false).is_err());
        assert!(parse_yaml(&format!("---\n- {scalar}\n"), true).is_err());
    }
}
