//! Keybinding reference. Doubles as the discovery surface for everything the
//! dashboard can do, which is why it enumerates the old slash commands too.

/// Lines of the help modal: `(key, description)`; a `None` key marks a
/// section heading.
pub fn help_lines() -> Vec<(Option<&'static str>, &'static str)> {
    vec![
        (None, "Panes"),
        (
            Some("Tab / Shift-Tab"),
            "instances → inspector → activity → chat",
        ),
        (Some("Esc"), "step back: inspector → list → chat"),
        (Some("Ctrl-T"), "free the mouse for native text selection"),
        (Some("F1"), "this help"),
        (Some("Ctrl-C"), "quit"),
        (None, "Instances (the list, top left)"),
        (
            Some("↑ / ↓"),
            "pick an instance; Enter or → opens it in the inspector",
        ),
        (
            Some("a  /  A"),
            "start a server / start a client (protocol picker)",
        ),
        (
            Some("+ new server"),
            "the last row of each section does the same",
        ),
        (Some("x"), "stop a server / remove a client — immediate"),
        (
            Some("e"),
            "edit config (port, host, instruction, parameters)",
        ),
        (Some("r"), "rules: who answers each event"),
        (
            Some("m"),
            "cycle the driver: MANUAL → LLM → SILENT (keeps specific rules)",
        ),
        (
            Some("c"),
            "on a server: connect a client of its protocol to it",
        ),
        (Some("n"), "on a client: compose and send one of its verbs"),
        (Some("w"), "Wireshark / tshark capture recipe"),
        (Some("d"), "protocol description and maturity, into chat"),
        (Some("1 … 6"), "jump to an inspector tab"),
        (None, "Inspector (bottom left)"),
        (Some("← / →"), "switch tab — or move along the action bar"),
        (
            Some("↑ / ↓"),
            "move through the tab's items; ↑ past the top reaches the buttons",
        ),
        (Some("Enter"), "press the button, or act on the item:"),
        (
            Some("  a peer"),
            "narrow traffic to it ([ message ] / [ disconnect ] in the bar)",
        ),
        (Some("  a request"), "open its full request and response"),
        (
            Some("  a rule"),
            "edit it ([ + add ] [ delete ] [ up ] [ down ] in the bar)",
        ),
        (Some("  a config row"), "open the edit form"),
        (
            Some("  a verb (send tab)"),
            "compose it with its parameters as fields",
        ),
        (
            Some("  ⚠ waiting"),
            "answer a request a MANUAL rule parked for you",
        ),
        (None, "Activity (top right)"),
        (
            Some("↑ / ↓"),
            "walk the feed; Enter opens what a line points at",
        ),
        (Some("f"), "only the selected instance's lines"),
        (
            Some("Ctrl-L"),
            "cycle the log level the feed shows (retroactive)",
        ),
        (None, "Chat (bottom right)"),
        (Some("Enter"), "send to the model, or run a slash command"),
        (Some("Alt-Enter / Ctrl-N"), "newline"),
        (Some("↑ / ↓"), "command history (at the first / last line)"),
        (Some("PageUp"), "scroll the conversation"),
        (None, "Who answers an event — the driver, and rules"),
        (
            Some("MANUAL"),
            "you: each event parks until you compose the answer",
        ),
        (
            Some("LLM"),
            "the model, from the instance instruction (needs a model)",
        ),
        (Some("SILENT"), "nobody: acknowledged, never answered"),
        (Some("STATIC"), "a rule with fixed actions, no model call"),
        (
            Some("SCRIPT"),
            "a rule running your python / js / perl / go per event",
        ),
        (None, "Global toggles"),
        (Some("Ctrl-W"), "web search: on / ask / off"),
        (Some("Ctrl-H"), "handler mode: any / script / static / llm"),
        (Some("Ctrl-E"), "scripting mode"),
        (None, "Slash commands still work in chat"),
        (
            Some("/model /backend"),
            "pick the model (click llm: on the status bar)",
        ),
        (Some("/log /web /handler"), "also Ctrl-L / Ctrl-W / Ctrl-H"),
        (Some("/docs /env /usage"), "also d on an instance"),
        (Some("/save /load"), "persist and restore instances"),
        (Some("/stop [id]"), "also x on an instance"),
        (Some("/quit"), "also Ctrl-C"),
    ]
}
