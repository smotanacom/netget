//! The `answer_with` hint `bolt_authenticate` carries.
//!
//! Told "accepts any login. Reject any query that is not valid Cypher", llama3.1:8b rejected the
//! login five runs in five - "Reject" was the operative verb and nothing said that no query was
//! being decided yet. The hint says what the login turns on and that query rules wait.

use netget::server::bolt::actions::login_answer_with;

#[test]
fn the_login_hint_names_the_user_and_sets_query_rules_aside() {
    let hint = login_answer_with("neo4j", "basic", true);
    assert!(
        hint.starts_with(
            "decide only whether user \"neo4j\" (scheme basic, a credential) may log in"
        ),
        "{hint}"
    );
    assert!(hint.contains("accept_bolt_login unless"), "{hint}");
    assert!(hint.contains("No query has been sent yet"), "{hint}");
    assert!(hint.contains("never to the login"), "{hint}");

    let anonymous = login_answer_with("", "none", false);
    assert!(
        anonymous.contains("no user name (scheme none, no credential)"),
        "{anonymous}"
    );
}
