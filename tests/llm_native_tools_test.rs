//! Network-event and client LLM calls must not attach native tool schemas.
//!
//! # What went wrong
//!
//! `call_llm` built its prompt from `advertised_network_event_actions`, which
//! appends the network-event tools (`generate_random`, `read_file`, `list_tasks`,
//! …) to the protocol's own actions — and then handed that same list to
//! `ConversationHandler::with_native_tools`, attaching native function-call
//! schemas to the request.
//!
//! That gave the model two ways to answer the same question: the JSON action
//! envelope the entire prompt teaches (`{"actions": [{"type": "send_..."}]}`), and
//! a parallel native function-calling channel. It reached for the second and
//! stopped emitting protocol actions at all — answering a Modbus read with a tool
//! call, a RADIUS accounting request with narration, an LDAP unbind with neither.
//!
//! The code never meant to do this. `action_helper.rs` said so twice, in comments
//! that had been true of an earlier version and were left behind:
//!
//! ```text
//! // Note: Network events don't use tools (immediate response), but get retry logic
//! // Generate response with retry (no tool calling for network events)
//! ```
//!
//! # How it was measured
//!
//! Six live cases that failed with schemas attached, run again with them removed
//! and nothing else changed — same model (qwen3.8:27b-mlx), same prompts, same
//! assertions:
//!
//! | case | schemas on | schemas off |
//! |---|---|---|
//! | `modbus_read_bits_returns_one_value_per_coil` | FAILED | ok |
//! | `modbus_read_registers_returns_one_value_per_register` | FAILED | ok |
//! | `radius_accounting_is_acknowledged` | FAILED | ok |
//! | `memcached_flush_all_is_acknowledged` | FAILED | ok |
//! | `etcd_put_reports_a_revision` | FAILED | ok |
//! | `ldap_unbind_is_not_answered_on_the_wire` | FAILED | ok |
//!
//! 6 of 6. Across the wider re-validation the rate was ~11% of all event cases,
//! spread evenly over unrelated protocols — which is what one shared defect looks
//! like, and not what a dozen independent prompting weaknesses look like.
//!
//! # Current contract
//!
//! Network events now advertise response actions only. The JSON tool loop is retained
//! for operator conversations and scheduled tasks, whose tool results inform a final
//! actions-only response. The independent native-schema regression below still applies:
//! protocol response actions travel in the JSON envelope, not a second schema channel.

use std::path::Path;

/// Strip `//` line comments so a rationale that *names* the call cannot satisfy
/// the check that the call is absent.
fn strip_line_comments(source: &str) -> String {
    source
        .lines()
        .map(|line| match line.find("//") {
            Some(i) => &line[..i],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn no_native_tool_schemas_on_network_events() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/llm/action_helper.rs");
    let source = std::fs::read_to_string(&path).expect("read action_helper.rs");
    let code = strip_line_comments(&source);

    let calls = code.matches(".with_native_tools(").count();

    // The only legitimate remaining caller is the feedback conversation, which is
    // RequestSource::User. If that one is ever removed too, this expectation drops
    // to zero — it is a ceiling on network-event usage, not a floor.
    assert!(
        calls <= 1,
        "action_helper.rs has {} `.with_native_tools(...)` call(s); at most the \
         user-initiated feedback conversation may have one.\n\n\
         Attaching native function-call schemas to a network-event or client call \
         gives the model a second way to answer alongside the JSON action envelope \
         the prompt teaches, and it takes it — 6 of 6 failing protocol cases pass \
         once the schemas are removed (see this file's module docs). Do not re-add a competing schema channel.",
        calls
    );

    // And the one that may remain must be the User-source feedback path, not a
    // Network one that happens to sit last in the file.
    if calls == 1 {
        let idx = code
            .find(".with_native_tools(")
            .expect("counted one call, so it must be findable");
        let preceding = &code[..idx];
        let block_start = preceding
            .rfind("ConversationHandler::new(")
            .expect("a with_native_tools call should follow a ConversationHandler::new");
        let block = &code[block_start..idx];
        assert!(
            block.contains("RequestSource::User"),
            "the remaining `.with_native_tools(...)` is on a {} conversation. Only \
             user-initiated conversations may attach native tool schemas; network \
             events and client calls must not (see this file's module docs).",
            if block.contains("RequestSource::Network") {
                "RequestSource::Network"
            } else {
                "non-User"
            }
        );
    }
}
