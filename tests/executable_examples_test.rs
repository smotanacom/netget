//! Every action's own `example` must survive its own `execute_action`.
//!
//! `ActionDefinition.example` is not decoration: it is rendered into the model's tool list, so
//! it is the thing the model copies. An example the protocol's own executor rejects hands the
//! model a shape it is then punished for using — and the rejection arrives as a runtime error
//! on a live connection, not as anything a declaration check can see.
//!
//! `event_action_declarations_test` already probes each advertised **name** with a bare
//! `{"type": name}`, which finds "unknown action". It cannot find a wrong *field*, because it
//! never sends one. This does, and that is how it found:
//!
//! * `ospf` advertising `list_neighbors` and `list_lsdb` with no executor arm for either;
//! * `tor_relay` advertising `tor_relay_log` under a doc comment claiming `execute_action`
//!   "has always handled it", which it did not;
//! * `dhcp` and `bootp` shipping a `data` example containing a literal `...` ellipsis, so the
//!   documented hex string cannot be decoded by anything.
//!
//! # Two rejections that are not defects
//!
//! `execute_action` is dispatched on the **stateless** protocol struct the registry holds — no
//! socket, no peer table, no request context. So two whole classes of action refuse an example
//! for reasons that say nothing about the example:
//!
//! * *needs a live connection or request context* — `websocket`'s send verbs, `radius`'s
//!   authenticator, `usb-*`'s attached host, `usb-fido2`'s approval state.
//! * *needs an id that only exists at runtime* — `amqp`/`mqtt`'s `server_id`, a consumer tag.
//!
//! Those are recognised by their message and excluded. The excluded set is asserted to be
//! non-trivial and the *total* is asserted too, so this cannot quietly degrade into a test that
//! examines nothing.
//!
//! Run with:
//!   ./cargo-isolated.sh test --all-features --test executable_examples_test

use netget::llm::actions::client_trait::{client_llm_action_set, Client};
use netget::llm::actions::protocol_trait::Protocol as _;
use netget::protocol::server_registry;
use netget::state::app_state::AppState;
use std::collections::BTreeSet;

/// `protocol::action` whose declared example its own executor refuses for a reason that is
/// about the example itself.
///
/// **Empty, and it may only stay that way.** It once held eighteen entries, all malformed
/// literals in the documented example — a `...` ellipsis inside a hex string, an odd digit
/// count, or a `{{event.field}}` template where the executor decodes hex. The template case is
/// worth remembering: `{{event.…}}` is substituted only for **static event handlers**
/// (`llm/event_handler_executor.rs`), so an example written that way is executable for a
/// handler and garbage for the model, which is the one audience `example` is rendered to.
/// Each was replaced with wire bytes assembled from the protocol's own spec.
///
/// **This list may only shrink.**
const BAD_EXAMPLE_BASELINE: &[&str] = &[];

/// A rejection that is about missing *runtime context* rather than about the example.
///
/// Matched on the executor's own wording. Deliberately specific: a catch-all here would let a
/// genuinely broken example hide behind a vague error, which is the failure mode this whole
/// file exists to prevent.
fn is_context_rejection(message: &str) -> bool {
    const MARKERS: &[&str] = &[
        "connection",
        "context",
        "adapter handle",
        "execute_action_with_state",
        "is attached",
        "host is attached",
        "did not offer",
        "no 'xid' given",
        "Missing 'server_id'",
        "No consumer",
        "No connected",
        "can only run while answering",
        "Failed to open disk image",
    ];
    MARKERS.iter().any(|m| message.contains(m))
}

struct Outcome {
    checked: usize,
    context_skipped: usize,
    offenders: BTreeSet<String>,
    detail: Vec<String>,
}

fn probe(
    protocol: &str,
    name: &str,
    example: &serde_json::Value,
    out: &mut Outcome,
    run: impl FnOnce(serde_json::Value) -> anyhow::Result<()>,
) {
    out.checked += 1;
    let key = format!("{protocol}::{name}");

    if !example.is_object() {
        out.offenders.insert(key.clone());
        out.detail
            .push(format!("  {key}: example is not a JSON object: {example}"));
        return;
    }
    match example.get("type").and_then(|v| v.as_str()) {
        Some(t) if t == name => {}
        other => {
            out.offenders.insert(key.clone());
            out.detail.push(format!(
                "  {key}: example declares type={other:?}, so copying it invokes a different action"
            ));
            return;
        }
    }

    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(example.clone()))) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            let msg = format!("{e:#}");
            if is_context_rejection(&msg) {
                out.context_skipped += 1;
            } else {
                out.offenders.insert(key.clone());
                out.detail.push(format!(
                    "  {key}: its own executor rejects its example: {msg}"
                ));
            }
        }
        Err(_) => {
            out.offenders.insert(key.clone());
            out.detail.push(format!(
                "  {key}: execute_action PANICKED on its own example"
            ));
        }
    }
}

fn audit() -> Outcome {
    let state = AppState::new();
    let mut out = Outcome {
        checked: 0,
        context_skipped: 0,
        offenders: BTreeSet::new(),
        detail: Vec::new(),
    };

    for (_name, proto) in server_registry::registry().all_protocols() {
        let mut actions = proto.get_async_actions(&state);
        actions.extend(proto.get_sync_actions());
        for ev in proto.get_event_types() {
            actions.extend(ev.actions.clone());
        }
        let mut seen = BTreeSet::new();
        for action in actions {
            if !seen.insert(action.name.clone()) {
                continue;
            }
            let p = proto.clone();
            probe(
                proto.protocol_name(),
                &action.name,
                &action.example,
                &mut out,
                move |v| p.execute_action(v).map(|_| ()),
            );
        }
    }

    for client in netget::protocol::CLIENT_REGISTRY.get_all() {
        let mut seen = BTreeSet::new();
        for action in client_llm_action_set(client.as_ref(), &state, None) {
            if !seen.insert(action.name.clone()) {
                continue;
            }
            let c = client.clone();
            probe(
                client.protocol_name(),
                &action.name,
                &action.example,
                &mut out,
                move |v| c.execute_action(v).map(|_| ()),
            );
        }
    }
    out
}

#[test]
fn no_action_ships_an_example_its_own_executor_refuses() {
    let out = audit();
    let baseline: BTreeSet<&str> = BAD_EXAMPLE_BASELINE.iter().copied().collect();
    let found: BTreeSet<&str> = out.offenders.iter().map(String::as_str).collect();

    let new: Vec<_> = found.difference(&baseline).copied().collect();
    assert!(
        new.is_empty(),
        "these actions declare an example their own execute_action refuses, and are not in \
         BAD_EXAMPLE_BASELINE: {new:?}\n{}\n\
         The example is rendered into the model's tool list, so it is the shape the model \
         copies. Fix the example, or fix the executor — do not add a line here.",
        out.detail.join("\n")
    );

    let fixed: Vec<_> = baseline.difference(&found).copied().collect();
    assert!(
        fixed.is_empty(),
        "these examples are accepted now — remove them from BAD_EXAMPLE_BASELINE so the \
         ratchet keeps its grip: {fixed:?}"
    );
}

/// The audit must be looking at the whole tree, and the context exclusion must not have
/// swallowed it.
///
/// Without this, deleting a registry accessor or broadening `is_context_rejection` would turn
/// the test above into one that passes by examining nothing.
#[test]
fn the_example_audit_has_something_to_inspect() {
    let out = audit();
    assert!(
        out.checked > 900,
        "only {} examples were checked; the audit is not reaching the registries",
        out.checked
    );
    assert!(
        out.context_skipped > 10,
        "only {} rejections were classified as needing runtime context. That number is stable \
         and non-trivial; a sudden zero means the executors' wording changed and the exclusion \
         no longer matches, which would bury real findings",
        out.context_skipped
    );
    assert!(
        out.context_skipped < out.checked / 4,
        "{} of {} examples were excluded as context rejections — the exclusion has become a \
         catch-all and is hiding real defects",
        out.context_skipped,
        out.checked
    );
}
