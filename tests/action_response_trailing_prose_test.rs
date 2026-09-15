//! A model's answer must survive the prose it comes wrapped in.
//!
//! **This is the defect the first real-model eval found, and it was the only one it could
//! find.** Over 30 runs across `http`, `dns` and `whois`, a small model named the right action
//! with the right parameters *every single time* — `send_http_response`, `send_dns_a_response`,
//! `send_dns_nxdomain`, `send_dns_txt_response`, `send_whois_record` — and NetGet discarded 29
//! of those 30 answers with `Invalid JSON`. To the protocol that is indistinguishable from a
//! backend failure, so it failed closed and the client saw nothing.
//!
//! The cause was one property of `serde_json::from_str`: it requires the **whole** string to be
//! a single JSON value. Small models append an explanation after the JSON constantly:
//!
//! ```text
//! {"actions": [{"type": "send_http_response", "status": 200, …}]}
//! Explanation: Since this is an HTTP request event, I'm emitting a send_http_response action…
//! ```
//!
//! Nothing in the existing suite could see it. Every mock returns exactly the JSON its test
//! author wrote, so the mock and the parser agree by construction — the defect lives in the gap
//! between a mock and a model, which is the entire argument for having a real-model eval at all.
//!
//! Note the shape of the near-miss: **the fenced form already worked**, because the fence
//! stripper cuts at the closing ```` ``` ````. So the same model producing the same answer
//! succeeded or failed depending on whether it used a code fence, which is why this read as a
//! formatting quirk rather than a parsing bug.

use netget::llm::actions::ActionResponse;

fn parse(s: &str) -> ActionResponse {
    ActionResponse::from_str(s).unwrap_or_else(|e| panic!("should have parsed: {e}\ninput: {s}"))
}

/// The exact shape that failed 29 of 30 eval runs.
#[test]
fn json_followed_by_an_explanation_still_parses() {
    let raw = "{\"actions\": [{\"type\": \"send_http_response\", \"status\": 200, \
                \"body\": \"<html><body>Hello World</body></html>\"}]}\n\
                Explanation: Since this is an HTTP request event, I'm emitting a \
                `send_http_response` action with the requested body.";

    let parsed = parse(raw);
    assert_eq!(parsed.actions.len(), 1, "the single action must survive");
    assert_eq!(parsed.actions[0]["type"], "send_http_response");
    assert_eq!(parsed.actions[0]["status"], 200);
}

/// A preamble before the JSON was already handled, and must stay handled — the fix widens what
/// is accepted and must not narrow anything.
#[test]
fn prose_on_both_sides_still_parses() {
    let raw = "Sure! Here is the action you asked for:\n\
                {\"actions\": [{\"type\": \"send_dns_a_response\", \"query_id\": 4242, \
                \"domain\": \"example.com\", \"ip\": \"1.2.3.4\"}]}\n\
                Let me know if you need anything else.";

    let parsed = parse(raw);
    assert_eq!(parsed.actions.len(), 1);
    assert_eq!(parsed.actions[0]["query_id"], 4242);
    assert_eq!(parsed.actions[0]["ip"], "1.2.3.4");
}

/// The fenced form worked before the fix and must keep working. Keeping it here beside the
/// unfenced case is the point: the two differed only in the fence, which is what disguised a
/// parsing defect as a formatting one.
#[test]
fn a_fenced_block_with_trailing_prose_still_parses() {
    let raw = "```json\n\
                {\"actions\": [{\"type\": \"send_whois_record\", \"domain\": \"example.com\"}]}\n\
                ```\n\
                That record answers the query.";

    let parsed = parse(raw);
    assert_eq!(parsed.actions.len(), 1);
    assert_eq!(parsed.actions[0]["domain"], "example.com");
}

/// A bare top-level array is a valid answer and must not be corrupted by the leading-brace
/// search — `{` appears inside the first element, so preferring it unconditionally would strip
/// the opening `[`.
#[test]
fn a_bare_array_with_trailing_prose_still_parses() {
    let raw = "[{\"type\": \"send_dns_nxdomain\", \"query_id\": 7, \"domain\": \"nope.test\"}]\n\
                I chose NXDOMAIN because the name is not served here.";

    let parsed = parse(raw);
    assert_eq!(parsed.actions.len(), 1);
    assert_eq!(parsed.actions[0]["type"], "send_dns_nxdomain");
}

/// Widening must not swallow genuine failure. A reply with no JSON in it at all is still an
/// error — otherwise a model that answered in pure prose would look like a model that answered
/// with nothing, and those two are different decisions the log has to keep apart
/// (`decision=model_silent` vs a parse failure).
#[test]
fn prose_with_no_json_at_all_is_still_an_error() {
    assert!(
        ActionResponse::from_str("I am not sure what to do with this request.").is_err(),
        "a reply containing no JSON must fail rather than silently becoming zero actions"
    );
}

/// Truncated JSON — a model cut off mid-answer by a token limit — must also stay an error.
/// `StreamDeserializer` yields an `Err` for an incomplete value rather than a partial one, and
/// this pins that: half an action is not an action.
#[test]
fn truncated_json_is_still_an_error() {
    let raw = "{\"actions\": [{\"type\": \"send_http_response\", \"status\": 2";
    assert!(
        ActionResponse::from_str(raw).is_err(),
        "a truncated answer must fail rather than parse as something partial"
    );
}

/// Two JSON values in one reply: the first wins and the second is ignored. This is the
/// deliberate consequence of taking the first complete value, and it is the right one — the
/// alternative is rejecting the whole reply, which is the behaviour being fixed.
#[test]
fn a_second_json_value_after_the_first_is_ignored() {
    let raw = "{\"actions\": [{\"type\": \"send_http_response\", \"status\": 200}]}\n\
                {\"actions\": [{\"type\": \"send_http_response\", \"status\": 500}]}";

    let parsed = parse(raw);
    assert_eq!(parsed.actions.len(), 1);
    assert_eq!(
        parsed.actions[0]["status"], 200,
        "the first value is the answer; a second is trailing content like any other"
    );
}
