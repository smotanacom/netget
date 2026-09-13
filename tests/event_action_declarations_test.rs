//! Every registered protocol must advertise actions for the events it emits
//! (IMPROVEMENTS.md items 56 and 75).
//!
//! An `EventType` with an empty action list, on a protocol that declares sync actions,
//! hands the model only `set_memory`/`append_memory`/`show_message`/`append_to_log`. Every
//! protocol action it then returns is rejected as unknown, retried twice, and the request
//! fails. That shape silently disabled sixteen protocols. An event that genuinely needs no
//! actions says so with `.with_no_actions()`.
//!
//! The runtime guard for this used to be a `debug_assert!(false)` on the per-event path in
//! `action_helper::call_llm` — inside a tokio task, once per connection, where the panic is
//! swallowed and the server keeps reporting `Running`. This test is where that check
//! belongs: it covers every registered protocol at build time instead of only the ones a
//! given run happens to start, and it names them all at once.
//!
//! # The client half
//!
//! Everything above walks the **server** registry. The ~90 registered clients had no guard of
//! any kind, and a defect of exactly the same shape lived there undetected:
//! `call_llm_for_client` built the model's tool list from `get_async_actions()` alone — never
//! `get_sync_actions()`, never `event.event_type.actions` — so an action declared only as sync
//! was advertised nowhere and rejected as unknown when the model guessed it. TFTP's sync-only
//! `send_ack` stalled every transfer at block 1; 53 of 91 clients had at least one action
//! invisible this way. The client half of this file (bottom) is the guard for that.

use netget::llm::actions::client_trait::{audit_client_action_declarations, client_llm_action_set};
use netget::llm::actions::protocol_trait::{audit_event_action_declarations, Protocol};
use netget::protocol::client_registry::CLIENT_REGISTRY;
use netget::protocol::server_registry::registry;
use netget::state::app_state::AppState;

/// Events that had this defect when the audit was written and are not yet fixed.
///
/// Empty, and meant to stay that way. Every entry that was here — all eight SSH-Agent events,
/// the twelve USB events, `http3_connection_opened` and `mongodb_disconnected` — now either
/// attaches the actions that answer it or declares `.with_no_actions()`. Leave this list empty
/// and let the test fail rather than re-quarantining a new occurrence.
const KNOWN_MISDECLARED: &[(&str, &str)] = &[];

fn is_quarantined(protocol_name: &str, event_id: &str) -> bool {
    KNOWN_MISDECLARED
        .iter()
        .any(|(p, e)| *p == protocol_name && *e == event_id)
}

#[test]
fn no_registered_protocol_emits_an_event_without_actions() {
    let mut findings: Vec<String> = Vec::new();
    let mut quarantined_seen = 0usize;

    for (name, protocol) in registry().all_protocols() {
        let protocol_name = protocol.protocol_name();
        // Re-derive the offending event ids so the quarantine can be matched precisely.
        let offending: Vec<String> = protocol
            .get_event_types()
            .into_iter()
            .filter(|et| et.has_no_usable_actions())
            .map(|et| et.id.clone())
            .collect();

        // The audit is the thing under test; the ids above only classify its findings.
        let problems = audit_event_action_declarations(protocol.as_ref() as &dyn Protocol);
        if problems.is_empty() {
            continue;
        }

        for (problem, event_id) in problems.iter().zip(offending.iter()) {
            if is_quarantined(protocol_name, event_id) {
                quarantined_seen += 1;
                continue;
            }
            findings.push(format!("[{}] {}", name, problem));
        }
    }

    assert!(
        findings.is_empty(),
        "{} event type(s) leave the model with no way to answer them.\n\
         Add .with_actions(...) to the event type, or .with_no_actions() if it genuinely \
         needs none. A protocol that declares no sync actions *either* is the worst case, \
         not an exempt one — it offers the model nothing at all:\n\n{}",
        findings.len(),
        findings.join("\n\n")
    );

    if quarantined_seen > 0 {
        eprintln!(
            "note: {} pre-existing misdeclared event(s) are quarantined in KNOWN_MISDECLARED",
            quarantined_seen
        );
    }
}

/// The audit is only meaningful if it actually inspects something: a registry whose
/// protocols all return an empty `get_event_types()` would pass the test above vacuously.
#[test]
fn the_audit_has_something_to_inspect() {
    let with_events = registry()
        .all_protocols()
        .into_iter()
        .filter(|(_, p)| !p.get_event_types().is_empty())
        .count();

    assert!(
        with_events > 0,
        "no registered protocol declares any event types; the audit above proves nothing"
    );
}

/// The audit used to early-return whenever `get_sync_actions()` was empty, so a protocol that
/// declares **no** sync actions and **no** event actions passed vacuously — the guard was
/// weakest on the most broken protocol in the tree. `usb-fido2` was exactly that shape and the
/// audit reported it clean.
///
/// This is the regression test for that hole, written against a stub rather than the registry
/// so it holds no matter which features are compiled in.
#[test]
fn a_protocol_that_offers_the_model_nothing_is_flagged() {
    let findings = audit_event_action_declarations(&stub::OffersNothing as &dyn Protocol);

    assert_eq!(
        findings.len(),
        1,
        "a protocol with no sync actions and an event with no actions must be flagged, not \
         waved through; got {:?}",
        findings
    );
    assert!(
        findings[0].contains("offers the model nothing"),
        "the finding must say the protocol offers the model nothing, not talk about \
         withholding sync actions it does not have: {}",
        findings[0]
    );
}

/// The counterpart: an event that *says* it needs no actions is the deliberate case and must
/// stay unflagged, whether or not the protocol declares sync actions. Otherwise every
/// informational event (a peer disconnected, a session ended) becomes a false positive.
#[test]
fn deliberate_no_actions_is_never_flagged() {
    assert!(
        audit_event_action_declarations(&stub::DeliberatelySilent as &dyn Protocol).is_empty(),
        "an event marked with_no_actions() must not be flagged"
    );
}

/// Delegation — forwarding another protocol's action set, as `doh`/`dot` do for DNS — is the
/// one legitimate way to have a vocabulary you did not define yourself, and must not be
/// flagged either.
#[test]
fn a_protocol_that_delegates_its_actions_is_not_flagged() {
    assert!(
        audit_event_action_declarations(&stub::Delegating as &dyn Protocol).is_empty(),
        "a protocol whose events carry a borrowed action set must not be flagged"
    );
}

/// The registry sweep proves nothing about protocols that were not compiled in, and CI
/// compiles six of them. Make the blind spot visible rather than silent: report how many
/// protocols the sweep actually inspected, and fail if a build somehow registers none.
///
/// The wide sweep is `--all-features`; see `tests/README` notes in
/// `src/llm/actions/protocol_trait.rs` for why it is worth running out of band.
#[test]
fn the_audit_reports_its_own_coverage() {
    let all = registry().all_protocols();
    let with_events = all
        .iter()
        .filter(|(_, p)| !p.get_event_types().is_empty())
        .count();

    eprintln!(
        "event-action audit coverage: {} registered protocol(s) in this build, {} declaring \
         event types. This is a lower bound on the real inventory — protocols behind \
         uncompiled features are invisible here. Run with --all-features for the full sweep.",
        all.len(),
        with_events
    );

    assert!(
        !all.is_empty(),
        "no protocol is registered in this build; the audit above inspected nothing"
    );
}

// ---------------------------------------------------------------------------
// Client half.
//
// `call_llm_for_client` is the only LLM entry point clients have, so unlike the server path
// there is no narrowing to preserve: the action set it advertises must be the union of
// `get_async_actions()`, `get_sync_actions()` and the event's own `.with_actions(...)` list.
// `client_llm_action_set` is that union and is what the runtime calls; these tests pin it.
// ---------------------------------------------------------------------------

/// The sweep. Every registered client, every action it declares anywhere, must be visible to
/// the model.
///
/// Before `call_llm_for_client` was fixed this failed for 53 of the 91 client protocols —
/// eleven of them hiding a protocol-specific action (`send_privmsg`, `send_apdu`,
/// `sign_request`, `write_characteristic`, `send_echo_request`, …) and the rest hiding
/// `wait_for_more`, which left every stream client unable to say "that response was partial".
#[test]
fn no_registered_client_declares_an_action_the_model_cannot_see() {
    let state = AppState::new();
    let mut findings: Vec<String> = Vec::new();

    for client in CLIENT_REGISTRY.get_all() {
        for problem in audit_client_action_declarations(client.as_ref(), &state) {
            findings.push(format!("[{}] {}", client.protocol_name(), problem));
        }
    }

    assert!(
        findings.is_empty(),
        "{} client action declaration problem(s).\n\n\
         `call_llm_for_client` advertises `client_llm_action_set(...)`, which is the union of \
         async actions, sync actions and the event type's own actions. An action that is not \
         in that union is advertised to the model nowhere and rejected at runtime as an \
         unknown action — the shape that stalled every TFTP transfer at block 1.\n\n{}",
        findings.len(),
        findings.join("\n\n")
    );
}

/// The teeth, pinned against a stub so it holds whatever features are compiled: an action
/// declared **only** in `get_sync_actions()` must reach the model.
///
/// This is the exact defect. Revert `client_llm_action_set` to `get_async_actions()` alone and
/// this test fails, with or without a registry.
#[test]
fn a_sync_only_client_action_is_still_advertised() {
    let state = AppState::new();
    let visible = client_llm_action_set(&client_stub::SyncOnly, &state, None);
    let names: Vec<&str> = visible.iter().map(|a| a.name.as_str()).collect();

    assert!(
        names.contains(&"sync_only_action"),
        "an action declared only in get_sync_actions() must still be advertised to the model; \
         got {:?}",
        names
    );
}

/// The other half of the defect: an action attached to an event type via `.with_actions(...)`
/// and declared in neither action list must reach the model when that event fires. TFTP, ICMP,
/// RSS, websocket and kafka are the clients that declare event actions at all.
#[test]
fn an_event_only_client_action_is_advertised_when_that_event_fires() {
    let state = AppState::new();
    let event =
        netget::protocol::Event::new(&client_stub::EVENT_WITH_OWN_ACTION, serde_json::json!({}));

    let visible = client_llm_action_set(&client_stub::SyncOnly, &state, Some(&event));
    let names: Vec<&str> = visible.iter().map(|a| a.name.as_str()).collect();

    assert!(
        names.contains(&"event_only_action"),
        "an action attached to the firing event must be advertised even when neither action \
         list declares it; got {:?}",
        names
    );

    // …and without the event it is correctly absent, so the union is not silently global.
    let without = client_llm_action_set(&client_stub::SyncOnly, &state, None);
    assert!(
        !without.iter().any(|a| a.name == "event_only_action"),
        "an event's own action must not leak into calls for other events"
    );
}

/// Deduplication: ~40 clients duplicate their whole list into both `get_async_actions()` and
/// `get_sync_actions()` (a workaround for the very defect this fixes). The union must not
/// advertise those twice — a duplicated tool name confuses native tool-calling backends.
#[test]
fn duplicated_client_actions_are_advertised_once() {
    let state = AppState::new();
    let visible = client_llm_action_set(&client_stub::Duplicated, &state, None);

    assert_eq!(
        visible.len(),
        1,
        "an action declared in both lists must be advertised once, got {:?}",
        visible.iter().map(|a| &a.name).collect::<Vec<_>>()
    );
    assert_eq!(
        visible[0].description, "async description",
        "the first occurrence wins, so the model sees the async description — the one written \
         for a model choosing what to do next"
    );
}

/// The hole the server audit used to have, ported forward deliberately unclosed-proof: a
/// client that declares **nothing** anywhere must FAIL, not pass vacuously.
#[test]
fn a_client_that_offers_the_model_nothing_is_flagged() {
    let state = AppState::new();
    let findings = audit_client_action_declarations(&client_stub::OffersNothing, &state);

    assert!(
        !findings.is_empty(),
        "a client with no async actions, no sync actions and no event actions must be flagged"
    );
    assert!(
        findings[0].contains("advertises no actions at all"),
        "the finding must say the client offers nothing: {}",
        findings[0]
    );
}

/// The mirror image, and the third way an action can be useless: **advertised but not
/// executable**. `execute_action` is a pure `serde_json::Value -> ClientActionResult`
/// translator with no I/O, so every advertised action name can simply be fed to it and the
/// rejection path observed.
///
/// Two clients failed this when it was written, both with the same inverted shape: the name
/// the model was shown and the name the executor accepted were different, and the action's own
/// `example` used the executor's name — so a model that called the tool by name was rejected,
/// and only a model that copied the contradictory example verbatim worked.
///
/// * `torrent_dht` advertised `dht_ping` / `dht_find_node` / `dht_get_peers` /
///   `dht_announce_peer`; the executor accepted only `dht_query`, declared nowhere.
/// * `torrent_peer` advertised `peer_interested` / `peer_not_interested` /
///   `peer_request_piece` / `peer_send_piece`; the executor accepted only `peer_message`,
///   declared nowhere.
///
/// Only an outright *unknown action* rejection is a finding. Feeding `{"type": name}` with no
/// parameters legitimately fails with "missing 'index'" and similar; those are printed, not
/// flagged.
#[test]
fn every_advertised_client_action_is_accepted_by_its_own_executor() {
    let state = AppState::new();
    let mut findings: Vec<String> = Vec::new();
    let mut checked = 0usize;

    for client in CLIENT_REGISTRY.get_all() {
        for action in client_llm_action_set(client.as_ref(), &state, None) {
            checked += 1;
            let probe = serde_json::json!({ "type": action.name });
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                client.execute_action(probe)
            }));

            match outcome {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    let msg = e.to_string();
                    let lower = msg.to_lowercase();
                    let says_unknown = lower.contains("unknown")
                        || lower.contains("unsupported")
                        || lower.contains("unrecogni");
                    if says_unknown && msg.contains(&action.name) {
                        findings.push(format!(
                            "[{}] advertises action '{}' but its own execute_action rejects \
                             that name: {}",
                            client.protocol_name(),
                            action.name,
                            msg
                        ));
                    } else {
                        // A parameter complaint, which is the expected outcome for a bare probe.
                        eprintln!(
                            "  ok (params) {} / {}: {}",
                            client.protocol_name(),
                            action.name,
                            msg
                        );
                    }
                }
                Err(_) => findings.push(format!(
                    "[{}] execute_action panicked on '{}' — an LLM can send exactly this",
                    client.protocol_name(),
                    action.name
                )),
            }
        }
    }

    eprintln!(
        "executor round-trip: probed {} advertised client action(s)",
        checked
    );

    assert!(
        findings.is_empty(),
        "{} advertised client action(s) their own executor cannot run. The model is shown a \
         tool name it is then punished for using:\n\n{}",
        findings.len(),
        findings.join("\n")
    );
}

/// Coverage disclosure, same as the server sweep: the client registry only contains protocols
/// whose feature was compiled in, and CI compiles a handful.
#[test]
fn the_client_audit_reports_its_own_coverage() {
    let all = CLIENT_REGISTRY.get_all();

    eprintln!(
        "client action audit coverage: {} registered client(s) in this build. The real \
         inventory is 91; clients behind uncompiled features are invisible here. Run with a \
         wider --features set for the full sweep.",
        all.len()
    );

    // An empty registry is the honest answer at a feature set that compiles no client at all
    // — `--features bluetooth-ble-presenter` is one — so asserting non-empty makes the test
    // binary permanently red for anyone working on a server-only protocol, which trains people
    // to ignore it. That is the same failure the `executable_examples_test` coverage guard had.
    //
    // Nothing is lost by degrading to a disclosure: `client_event_wiring_test` reads the source
    // tree rather than the registry, so it inspects all 91 clients at every feature set and is
    // what actually guarantees the client half is never skipped.
    if all.is_empty() {
        eprintln!(
            "this build compiles no client feature, so the registry-walking client audit had \
             nothing to inspect. client_event_wiring_test covers the client tree from source \
             at any feature set."
        );
    }
}

/// Minimal `Client` implementations for the client half.
mod client_stub {
    use netget::llm::actions::client_trait::{Client, ClientActionResult};
    use netget::llm::actions::protocol_trait::Protocol;
    use netget::llm::actions::{ActionDefinition, StartupExamples};
    use netget::protocol::metadata::ProtocolMetadataV2;
    use netget::protocol::EventType;
    use netget::state::app_state::AppState;

    fn action(name: &str, description: &str) -> ActionDefinition {
        ActionDefinition {
            name: name.to_string(),
            description: description.to_string(),
            parameters: Vec::new(),
            example: serde_json::json!({ "type": name }),
            log_template: None,
        }
    }

    /// `Event::new` takes a `&'static EventType`, as every real protocol's event constants are.
    pub static EVENT_WITH_OWN_ACTION: std::sync::LazyLock<EventType> =
        std::sync::LazyLock::new(|| {
            EventType::new(
                "stub_client_event",
                "something arrived",
                serde_json::json!({"type": "event_only_action"}),
            )
            .with_actions(vec![action("event_only_action", "only on this event")])
        });

    macro_rules! stub_client {
        ($name:ident, $async_:expr, $sync:expr, $events:expr) => {
            pub struct $name;

            impl Protocol for $name {
                fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
                    $async_
                }
                fn get_sync_actions(&self) -> Vec<ActionDefinition> {
                    $sync
                }
                fn get_event_types(&self) -> Vec<EventType> {
                    $events
                }
                fn protocol_name(&self) -> &'static str {
                    stringify!($name)
                }
                fn stack_name(&self) -> &'static str {
                    "STUB"
                }
                fn keywords(&self) -> Vec<&'static str> {
                    vec!["stub client"]
                }
                fn metadata(&self) -> ProtocolMetadataV2 {
                    ProtocolMetadataV2::builder()
                        .implementation("test stub")
                        .llm_control("none")
                        .e2e_testing("none")
                        .build()
                }
                fn description(&self) -> &'static str {
                    "test stub"
                }
                fn example_prompt(&self) -> &'static str {
                    "stub"
                }
                fn group_name(&self) -> &'static str {
                    "Core"
                }
                fn get_startup_examples(&self) -> StartupExamples {
                    let e = serde_json::json!({"type": "open_client", "base_stack": "stub"});
                    StartupExamples::new(e.clone(), e.clone(), e)
                }
            }

            impl Client for $name {
                fn connect(
                    &self,
                    _ctx: netget::protocol::ConnectContext,
                ) -> std::pin::Pin<
                    Box<
                        dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>>
                            + Send,
                    >,
                > {
                    Box::pin(async { anyhow::bail!("stub") })
                }
                fn execute_action(
                    &self,
                    _action: serde_json::Value,
                ) -> anyhow::Result<ClientActionResult> {
                    Ok(ClientActionResult::NoAction)
                }
            }
        };
    }

    // The TFTP shape: the action that answers every inbound block lives only in the sync list.
    stub_client!(
        SyncOnly,
        vec![action("async_action", "user-triggered")],
        vec![action("sync_only_action", "network-triggered")],
        vec![EVENT_WITH_OWN_ACTION.clone()]
    );

    // The shape ~40 clients hand-copied to work around the defect.
    stub_client!(
        Duplicated,
        vec![action("both", "async description")],
        vec![action("both", "sync description")],
        Vec::new()
    );

    // Nothing anywhere. Must fail, not pass vacuously.
    stub_client!(OffersNothing, Vec::new(), Vec::new(), Vec::new());
}

/// Minimal `Protocol` implementations for the three shapes above.
///
/// Stubs rather than registry entries because the point is to pin the audit's *logic*, which
/// must not depend on which protocol features a given build happens to enable.
mod stub {
    use netget::llm::actions::protocol_trait::Protocol;
    use netget::llm::actions::{ActionDefinition, Parameter, StartupExamples};
    use netget::protocol::metadata::ProtocolMetadataV2;
    use netget::protocol::EventType;
    use netget::state::app_state::AppState;

    fn an_action() -> ActionDefinition {
        ActionDefinition {
            name: "do_something".to_string(),
            description: "does something".to_string(),
            parameters: vec![Parameter {
                name: "value".to_string(),
                type_hint: "string".to_string(),
                description: "a value".to_string(),
                required: true,
            }],
            example: serde_json::json!({"type": "do_something", "value": "x"}),
            log_template: None,
        }
    }

    fn an_event() -> EventType {
        EventType::new(
            "stub_event",
            "something happened",
            serde_json::json!({"type": "do_something", "value": "x"}),
        )
    }

    macro_rules! stub_protocol {
        ($name:ident, $sync:expr, $events:expr) => {
            pub struct $name;

            impl Protocol for $name {
                fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
                    Vec::new()
                }
                fn get_sync_actions(&self) -> Vec<ActionDefinition> {
                    $sync
                }
                fn get_event_types(&self) -> Vec<EventType> {
                    $events
                }
                fn protocol_name(&self) -> &'static str {
                    stringify!($name)
                }
                fn stack_name(&self) -> &'static str {
                    "STUB"
                }
                fn keywords(&self) -> Vec<&'static str> {
                    vec!["stub"]
                }
                fn metadata(&self) -> ProtocolMetadataV2 {
                    ProtocolMetadataV2::builder()
                        .implementation("test stub")
                        .llm_control("none")
                        .e2e_testing("none")
                        .build()
                }
                fn description(&self) -> &'static str {
                    "test stub"
                }
                fn example_prompt(&self) -> &'static str {
                    "stub"
                }
                fn group_name(&self) -> &'static str {
                    "Core"
                }
                fn get_startup_examples(&self) -> StartupExamples {
                    let e = serde_json::json!({"type": "open_server", "base_stack": "stub"});
                    StartupExamples::new(e.clone(), e.clone(), e)
                }
            }
        };
    }

    // No sync actions, one event with no actions: the usb-fido2 shape.
    stub_protocol!(OffersNothing, Vec::new(), vec![an_event()]);

    // No sync actions, one event that says so on purpose.
    stub_protocol!(
        DeliberatelySilent,
        Vec::new(),
        vec![an_event().with_no_actions()]
    );

    // No sync actions of its own, but the event carries a borrowed set (doh/dot shape).
    stub_protocol!(
        Delegating,
        Vec::new(),
        vec![an_event().with_actions(vec![an_action()])]
    );
}
