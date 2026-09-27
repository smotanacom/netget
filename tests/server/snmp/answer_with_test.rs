//! The `answer_with` hint `snmp_request` carries, and the example it replaced.
//!
//! `send_snmp_response`'s example used to answer sysDescr with "System Description" and add
//! sysName = "hostname"; llama3.1:8b sent both, for a one-OID GET, ten runs in ten. The hint names
//! each requested OID with its MIB-2 meaning and says nothing else belongs in the answer.

use netget::server::snmp::actions::{answer_with_for_request, SNMP_REQUEST_EVENT};

#[test]
fn a_get_names_each_oid_by_meaning_and_forbids_others() {
    let hint = answer_with_for_request(
        "GetRequest",
        &[
            "1.3.6.1.2.1.1.1.0".to_string(),
            "1.3.6.1.2.1.1.5.0".to_string(),
        ],
    );
    assert!(
        hint.starts_with("send_snmp_response with exactly 2 variables"),
        "{hint}"
    );
    assert!(
        hint.contains(
            "1.3.6.1.2.1.1.1.0 (sysDescr, the system description your instructions give), \
             1.3.6.1.2.1.1.5.0 (sysName, the device's name exactly as your instructions \
             write it - a hostname, character for character, never reworded or capitalised)"
        ),
        "{hint}"
    );
    assert!(hint.contains("add no other OIDs"), "{hint}");
    assert!(hint.contains("noSuchName"), "{hint}");

    let one = answer_with_for_request("GetRequest", &["1.3.6.1.4.1.8072.1.1".to_string()]);
    assert!(
        one.starts_with("send_snmp_response with exactly 1 variable,"),
        "{one}"
    );
}

#[test]
fn walking_and_setting_take_their_own_answers() {
    let next = answer_with_for_request("GetNextRequest", &["1.3.6.1.2.1.1".to_string()]);
    assert!(next.contains("the OID that follows"), "{next}");
    let set = answer_with_for_request("SetRequest", &["1.3.6.1.2.1.1.5.0".to_string()]);
    assert!(set.contains("readOnly"), "{set}");
}

/// No example the model reads carries a plausible system description or host name any more.
#[test]
fn the_examples_carry_no_plausible_values() {
    let mut texts: Vec<String> = SNMP_REQUEST_EVENT
        .actions
        .iter()
        .map(|action| action.example.to_string())
        .collect();
    texts.push(SNMP_REQUEST_EVENT.effective_response_example().to_string());
    for text in texts {
        assert!(
            !text.contains("System Description") && !text.contains("\"hostname\""),
            "{text}"
        );
    }
}
