//! The prompt must not grow without bound (IMPROVEMENTS.md item 15).
//!
//! Three independent leaks fed the same prompt:
//!
//! * `ConversationHandler.messages` was never trimmed across up to 5 tool iterations × 5
//!   retries, and every failed attempt plus its correction stayed for the rest of the
//!   conversation.
//! * `ConversationState`'s window evicted old messages to make room for a new one but never
//!   bounded the new one, so a single huge response blew straight past the limit.
//! * Server `memory` is injected into every prompt for that server and `append_memory` had
//!   no cap at all.

use std::sync::Arc;

use netget::llm::actions::executor::{bound_server_memory, MAX_SERVER_MEMORY_CHARS};
use netget::llm::rate_limiter::RateLimiterConfig;
use netget::llm::{
    ConversationHandler, ConversationState, OllamaClient, RateLimiter, RequestSource,
};

// --- ConversationHandler: what is actually sent ----------------------------

fn handler(system: &str) -> ConversationHandler {
    ConversationHandler::new(
        system.to_string(),
        // Never contacted: these tests only exercise message bookkeeping.
        Arc::new(OllamaClient::new("http://127.0.0.1:1")),
        "test-model".to_string(),
        RateLimiter::new(RateLimiterConfig::default()),
        RequestSource::Network,
    )
}

#[test]
fn history_is_bounded_by_message_count() {
    let mut conv = handler("SYSTEM");
    conv.add_user_message("the original request".to_string());
    for i in 0..200 {
        conv.add_user_message(format!("turn {}", i));
    }

    conv.trim_history();

    assert!(
        conv.message_count() <= 24,
        "history should be capped, got {} messages",
        conv.message_count()
    );
}

#[test]
fn history_is_bounded_by_total_size() {
    let mut conv = handler("SYSTEM");
    conv.add_user_message("the original request".to_string());
    // Ten messages of 10 KB each: well under any message-count cap, far over the char cap.
    for i in 0..10 {
        conv.add_user_message(format!("{}{}", i, "x".repeat(10_000)));
    }

    conv.trim_history();

    let non_system: usize = conv
        .messages()
        .iter()
        .skip(1)
        .map(|m| m.content.len())
        .sum();
    assert!(
        non_system <= 32_000,
        "history should be capped by size, got {} chars",
        non_system
    );
}

#[test]
fn trimming_keeps_the_system_message_and_the_original_request() {
    let mut conv = handler("SYSTEM INSTRUCTION");
    conv.add_user_message("THE ORIGINAL REQUEST".to_string());
    for i in 0..200 {
        conv.add_user_message(format!("turn {} {}", i, "y".repeat(500)));
    }

    conv.trim_history();

    let messages = conv.messages();
    assert_eq!(messages[0].role, "system");
    assert_eq!(messages[0].content, "SYSTEM INSTRUCTION");
    assert_eq!(
        messages[1].content, "THE ORIGINAL REQUEST",
        "the request the conversation is about must survive trimming"
    );

    // The most recent turn is still there.
    assert!(
        messages
            .last()
            .expect("non-empty")
            .content
            .contains("turn 199"),
        "the newest turn must survive trimming"
    );

    // And the model is told history was cut rather than left with a silent gap.
    assert!(
        messages.iter().any(|m| m.content.contains("omitted")),
        "a trim notice should stand in for the dropped turns"
    );
}

#[test]
fn trimming_is_idempotent_and_stable() {
    let mut conv = handler("SYSTEM");
    conv.add_user_message("request".to_string());
    for i in 0..100 {
        conv.add_user_message(format!("turn {} {}", i, "z".repeat(1000)));
    }

    conv.trim_history();
    let after_first = conv.message_count();
    conv.trim_history();
    conv.trim_history();

    assert_eq!(
        after_first,
        conv.message_count(),
        "repeated trims must not keep eating the conversation"
    );
}

#[test]
fn a_short_conversation_is_left_alone() {
    let mut conv = handler("SYSTEM");
    conv.add_user_message("request".to_string());
    conv.add_user_message("a follow-up".to_string());

    conv.trim_history();

    assert_eq!(conv.message_count(), 3);
    assert!(conv
        .messages()
        .iter()
        .all(|m| !m.content.contains("omitted")));
}

// --- ConversationState: the cross-call window ------------------------------

#[test]
fn tiny_history_windows_include_truncation_markers_in_their_budget() {
    for limit in [0, 1, 3, 10, 30, 255, 256, 512] {
        let mut state = ConversationState::new(limit);
        for _ in 0..10 {
            state.add_user_input("界🦀abc".repeat(100));
            state.add_llm_response("large response".repeat(100), None);
            assert!(
                state.current_size <= limit,
                "limit {limit}: {} bytes",
                state.current_size
            );
        }
        assert_eq!(
            state.current_size,
            state
                .messages
                .iter()
                .map(|m| m.content.len())
                .sum::<usize>()
        );
    }
}

#[test]
fn empty_messages_do_not_accumulate_outside_the_history_byte_budget() {
    let mut state = ConversationState::new(8000);
    for _ in 0..1000 {
        state.add_user_input(String::new());
        state.add_llm_response(String::new(), None);
        state.add_retry_instruction(String::new());
    }
    assert!(state.is_empty());
    assert_eq!(state.current_size, 0);
}

#[test]
fn a_single_oversized_message_cannot_exceed_the_window() {
    let mut state = ConversationState::new(8000);

    state.add_llm_response("x".repeat(1_000_000), None);

    assert!(
        state.current_size <= 8000,
        "one huge message blew the window: current_size = {}",
        state.current_size
    );

    let history = state.get_history_for_prompt();
    assert!(
        history.len() < 20_000,
        "the rendered history is unbounded: {} chars",
        history.len()
    );
    assert!(
        history.contains("truncated"),
        "the cut must be marked so the model knows the value is a prefix"
    );
}

#[test]
fn the_window_still_holds_several_normal_messages() {
    let mut state = ConversationState::new(8000);
    for i in 0..5 {
        state.add_user_input(format!("message {}", i));
    }
    assert_eq!(state.message_count(), 5);
    assert!(state.current_size < 8000);
}

// --- Server memory: injected into every prompt -----------------------------

#[test]
fn short_memory_is_untouched() {
    let memory = "line one\nline two".to_string();
    assert_eq!(bound_server_memory(memory.clone()), memory);
}

#[test]
fn appended_memory_is_bounded_and_keeps_the_newest_lines() {
    // Simulate a protocol appending a line per request, forever.
    let mut memory = String::new();
    for i in 0..5000 {
        if !memory.is_empty() {
            memory.push('\n');
        }
        memory.push_str(&format!("request {} handled", i));
        memory = bound_server_memory(memory);
    }

    assert!(
        memory.len() <= MAX_SERVER_MEMORY_CHARS,
        "memory grew to {} chars",
        memory.len()
    );
    assert!(
        memory.contains("request 4999 handled"),
        "the newest entry must survive"
    );
    assert!(
        !memory.contains("request 0 handled"),
        "the oldest entries should be the ones dropped"
    );
    assert!(
        memory.starts_with("[older memory dropped"),
        "the model must be told that history was dropped, got: {}",
        &memory[..60.min(memory.len())]
    );
}

#[test]
fn one_oversized_line_is_truncated_rather_than_dropped_whole() {
    let memory = "q".repeat(MAX_SERVER_MEMORY_CHARS * 3);
    let bounded = bound_server_memory(memory);

    assert!(bounded.len() <= MAX_SERVER_MEMORY_CHARS);
    assert!(bounded.starts_with("[older memory dropped"));
    assert!(bounded.len() > 1000, "it should keep as much as it can");
}

#[test]
fn bounding_is_char_safe() {
    // Multi-byte content must not be cut mid-character.
    let memory = "日本語のメモ ".repeat(MAX_SERVER_MEMORY_CHARS);
    let bounded = bound_server_memory(memory);
    assert!(bounded.len() <= MAX_SERVER_MEMORY_CHARS);
    // Reaching here without a panic is the assertion; confirm it is still valid UTF-8 text.
    assert!(bounded.chars().count() > 0);
}

#[test]
fn bounding_is_idempotent() {
    let memory = (0..2000)
        .map(|i| format!("entry {}", i))
        .collect::<Vec<_>>()
        .join("\n");
    let once = bound_server_memory(memory);
    let twice = bound_server_memory(once.clone());
    assert_eq!(once, twice);
}
