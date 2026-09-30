//! A server's memory and instruction reach the network-event prompt verbatim.
//!
//! Memory is the one thing a model keeps between events, and it reads it back from the
//! prompt's "Current State" section. That section is a Handlebars partial, and a
//! double-brace `{{ }}` there HTML-escapes: memory the model wrote as `room: "Hall"` came back
//! as `room: &quot;Hall&quot;`, an apostrophe as `&#x27;`, and a model that copied its memory
//! forward turned `&` into `&amp;` a little more each event. The instruction's copy in the same
//! section showed the demo's `"play"` as `&quot;play&quot;`.

use netget::llm::actions::get_network_event_common_actions;
use netget::llm::PromptBuilder;
use netget::state::app_state::AppState;
use netget::state::server::{ServerInstance, ServerStatus};
use netget::state::ServerId;

#[tokio::test]
async fn memory_and_instruction_are_not_html_escaped_in_the_event_prompt() {
    let state = AppState::new();
    let instruction = r#"Run a tiny adventure if they type "play" & keep it <short>; it's fine."#;
    let memory = "room: \"Hall\" & dark\nnote: it's <locked>";
    let mut server = ServerInstance::new(
        ServerId::new(1),
        2323,
        "Telnet".to_string(),
        instruction.to_string(),
    );
    server.status = ServerStatus::Running;
    server.memory = memory.to_string();
    let id = state.add_server(server).await;

    let prompt = PromptBuilder::build_network_event_action_prompt_for_server(
        &state,
        id,
        get_network_event_common_actions(),
    )
    .await;

    let current = &prompt[prompt
        .find("# Current State")
        .expect("the Current State section")..];
    assert!(
        current.contains(&format!("- **Memory**: {memory}")),
        "memory is not verbatim:\n{current}"
    );
    assert!(
        current.contains(&format!("- **Instruction**: {instruction}")),
        "the instruction is not verbatim:\n{current}"
    );
    for escaped in ["&quot;", "&#x27;", "&amp;", "&lt;", "&gt;"] {
        assert!(!current.contains(escaped), "{escaped} in:\n{current}");
    }
}
