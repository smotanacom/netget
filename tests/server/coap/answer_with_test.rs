//! The `answer_with` hint `coap_request` carries: what a small model reads for this one request.
//!
//! Told "you are a greenhouse sensor whose only resource is /temperature", llama3.1:8b answered a
//! GET of /humidity with an invented 2.05 five runs in five while the code table sat in the
//! action description. These pin the per-request sentence that replaced reliance on the table,
//! and `e2e_test.rs` proves it reaches the event (its not-found rule only matches when it does).

use netget::server::coap::actions::answer_with_for_request;

#[test]
fn a_get_leads_with_the_lookup_and_gives_the_404_as_the_literal_action() {
    let hint = answer_with_for_request("GET", "/humidity");
    assert!(
        hint.starts_with("first look in your instructions for a resource at /humidity"),
        "{hint}"
    );
    assert!(
        hint.contains(r#"{"type": "send_coap_response", "code": "4.04"}"#),
        "{hint}"
    );
    assert!(hint.contains("\"2.05\""), "{hint}");
    // The 4.04 comes before the success code: with the success code first, the model still
    // invented a representation in 2 runs of 5.
    assert!(hint.find("4.04") < hint.find("2.05"), "{hint}");
}

#[test]
fn each_method_names_its_own_success_code() {
    assert!(answer_with_for_request("POST", "/x").contains("\"2.04\""));
    assert!(answer_with_for_request("PUT", "/x").contains("\"2.04\""));
    assert!(answer_with_for_request("DELETE", "/x").contains("\"2.02\""));
    for method in ["GET", "POST", "PUT", "DELETE"] {
        assert!(
            answer_with_for_request(method, "/x").contains("\"4.04\""),
            "{method}"
        );
    }
}
