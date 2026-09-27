//! The `answer_with` hint `zabbix_sender_data` carries.
//!
//! The model accepted a value from an unmonitored host five runs in five (it never compared
//! `items[].host` with the instruction), and answered a one-value request with the example's
//! `processed: 2` two runs in five. The hint names each host and the counts the answer takes.

use netget::server::zabbix::actions::answer_with_for_items;

#[test]
fn one_host_gets_both_answers_as_literal_actions_refusal_first() {
    let one = answer_with_for_items(["mystery-box"]);
    assert!(
        one.starts_with(
            "this request carries 1 value from host \"mystery-box\". First look in your \
             instructions for the host \"mystery-box\"."
        ),
        "{one}"
    );
    let reject = r#"{"type": "send_zabbix_result", "processed": 0, "failed": 1}"#;
    let accept = r#"{"type": "send_zabbix_result", "processed": 1, "failed": 0}"#;
    assert!(one.contains(reject), "{one}");
    assert!(one.contains(accept), "{one}");
    assert!(one.find(reject) < one.find(accept), "{one}");

    let three = answer_with_for_items(["web1", "web1", "web1"]);
    assert!(three.contains(r#""processed": 3, "failed": 0"#), "{three}");
}

#[test]
fn a_batch_names_each_host_and_the_total() {
    let batch = answer_with_for_items(["web1", "db1", "web1"]);
    assert!(
        batch.contains("2 values from host \"web1\", 1 value from host \"db1\""),
        "{batch}"
    );
    assert!(batch.contains("processed + failed = 3"), "{batch}");
}
