//! The `answer_with` hint an OPTIONS request carries.
//!
//! The OPTIONS event used to say "answer with sip_options listing the methods you support",
//! and the action's only example is a 200. Told "you are a SIP phone in do-not-disturb mode;
//! tell anyone who checks on you that you are busy", llama3.1:8b answered sipsak with 200 OK
//! five runs in five. `e2e_test.rs` proves the hint reaches the event.

use netget::server::sip::actions::{default_reason_phrase, OPTIONS_ANSWER_WITH};

#[test]
fn options_names_the_status_for_each_situation() {
    for needle in [
        "status an INVITE would get",
        "status_code 200",
        "486 (Busy Here)",
        "do-not-disturb",
        "480 (Temporarily Unavailable)",
    ] {
        assert!(
            OPTIONS_ANSWER_WITH.contains(needle),
            "missing {needle:?}: {OPTIONS_ANSWER_WITH}"
        );
    }
}

/// A code with no `reason_phrase` gets RFC 3261's phrase, not "OK": the eval's busy phone went
/// out as "SIP/2.0 486 OK" until this existed.
#[test]
fn a_code_without_a_phrase_gets_its_own() {
    assert_eq!(default_reason_phrase(200), "OK");
    assert_eq!(default_reason_phrase(486), "Busy Here");
    assert_eq!(default_reason_phrase(480), "Temporarily Unavailable");
    assert_eq!(default_reason_phrase(603), "Decline");
    // Unlisted codes take their class's generic phrase.
    assert_eq!(default_reason_phrase(499), "Request Failure");
    assert_eq!(default_reason_phrase(299), "OK");
}
