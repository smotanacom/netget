//! The dashboard's no-LLM create/connect/send flow, driven through the same
//! models the modals use: protocol picker → instance form → apply, then the
//! composer → `send_to_client`.
//!
//! This is the capability the dashboard exists for: creating and modifying
//! servers and clients without asking the LLM to do it. The mock LLM URL is
//! unreachable on purpose — nothing here may contact a model.

#![cfg(feature = "tcp")]

use std::time::Duration;

use netget::privilege::SystemCapabilities;
use netget::state::app_state::AppState;
use netget::state::{AccessLogOwner, ServerId};
use netget::tui::app::Section;
use netget::tui::modal::composer::ComposerModel;
use netget::tui::modal::form::{FieldTarget, FormModel};
use netget::tui::modal::protocol_picker;
use netget::tui::projection;
use tokio::sync::mpsc;

async fn new_state() -> AppState {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".to_string());
    state
        .set_llm_client(netget::llm::OllamaClient::new(
            "http://127.0.0.1:1".to_string(),
        ))
        .await;
    state
}

fn llm() -> netget::llm::OllamaClient {
    netget::llm::OllamaClient::new("http://127.0.0.1:1".to_string())
}

async fn wait_for_port(state: &AppState, id: ServerId) -> u16 {
    for _ in 0..100 {
        if let Some(s) = state.get_server(id).await {
            if let Some(addr) = s.local_addr {
                return addr.port();
            }
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("server #{} never bound a port", id.as_u32());
}

#[test]
fn picker_lists_and_filters_protocols_with_badges() {
    let caps = SystemCapabilities::detect();
    let entries = protocol_picker::entries(Section::Servers, &caps);
    assert!(!entries.is_empty(), "server protocols should be listed");

    // Sorted case-insensitively.
    let names: Vec<String> = entries.iter().map(|e| e.name.to_lowercase()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "picker entries must be sorted");

    let tcp = entries
        .iter()
        .find(|e| e.name.eq_ignore_ascii_case("tcp"))
        .expect("TCP should be listed in a tcp-featured build");
    assert!(!tcp.description.is_empty());
    assert!(tcp.badge().starts_with('['));

    // Filtering matches names and descriptions, case-insensitively.
    let filtered = protocol_picker::filter(&entries, "TCP");
    assert!(filtered.iter().any(|e| e.name.eq_ignore_ascii_case("tcp")));
    assert!(protocol_picker::filter(&entries, "zzzz-no-such").is_empty());
    assert_eq!(protocol_picker::filter(&entries, "").len(), entries.len());

    // An exact name match ranks first. In an all-protocols build, plenty of
    // protocols mention TCP in their description (Modbus, MQTT, …), and
    // alphabetical order would put one of those ahead of TCP itself — so
    // typing "tcp" and pressing Enter would start the wrong server.
    assert!(
        filtered[0].name.eq_ignore_ascii_case("tcp"),
        "exact name match must rank first, got {:?}",
        filtered.iter().take(3).map(|e| &e.name).collect::<Vec<_>>()
    );
}

#[test]
fn create_form_offers_each_field_once() {
    // TCP declares a `send_first` startup parameter and the form has a
    // first-class field for it; the user must not get two controls for it.
    let form = FormModel::for_create(Section::Servers, "TCP", None);
    let mut labels: Vec<&str> = form.fields.iter().map(|f| f.label.as_str()).collect();
    let count = labels.len();
    labels.sort_unstable();
    labels.dedup();
    assert_eq!(
        labels.len(),
        count,
        "form fields must be unique: {labels:?}"
    );

    // A protocol that declares no default port still gets a usable one: port 0
    // asks the OS for a free port, so picking a protocol can start it without
    // asking the user anything.
    let port = form
        .fields
        .iter()
        .find(|f| f.target == FieldTarget::Port)
        .expect("server form has a port field");
    assert_eq!(port.value, "0");
    assert!(
        form.missing_required().is_none(),
        "a server form should be startable on defaults, missing: {:?}",
        form.missing_required()
    );
}

/// The interactive defaults, pinned. A server parks everything for the human
/// (`*` → manual). A client parks everything *except* its connect event: that
/// one is answered with nothing by a preceding zero-action static rule,
/// because several clients (TCP among them) handle it inline in `connect()` —
/// parking it stalled creation itself on a question with no useful answer.
/// Order matters: `EventHandlerConfig::find_handler` is first-match-wins, so
/// the connect rule must precede the wildcard.
#[test]
fn create_defaults_route_the_connect_event_past_the_human() {
    let read_handlers = |form: &FormModel| -> serde_json::Value {
        let raw = form
            .fields
            .iter()
            .find(|f| f.target == FieldTarget::EventHandlersJson)
            .expect("create form carries the event_handlers field")
            .value
            .clone();
        serde_json::from_str(&raw).expect("default event_handlers is valid JSON")
    };

    // Server: exactly the one manual wildcard, unchanged.
    let server_form = FormModel::for_create(Section::Servers, "TCP", None);
    let handlers = read_handlers(&server_form);
    let handlers = handlers.as_array().expect("array");
    assert_eq!(handlers.len(), 1, "server default is a single rule");
    assert_eq!(handlers[0]["event_pattern"], "*");
    assert_eq!(handlers[0]["handler"]["type"], "manual");

    // Client: connect first (static, zero actions), then the manual wildcard.
    let client_form = FormModel::for_create(Section::Clients, "TCP", None);
    let handlers = read_handlers(&client_form);
    let handlers = handlers.as_array().expect("array");
    assert_eq!(
        handlers.len(),
        2,
        "client default is connect-rule + manual wildcard: {handlers:?}"
    );
    assert_eq!(
        handlers[0]["event_pattern"], "tcp_connected",
        "the connect rule must come first so first-match-wins picks it"
    );
    assert_eq!(handlers[0]["handler"]["type"], "static");
    assert_eq!(
        handlers[0]["handler"]["actions"],
        serde_json::json!([]),
        "the connect answer is deliberately empty — acknowledge, say nothing"
    );
    assert_eq!(handlers[1]["event_pattern"], "*");
    assert_eq!(handlers[1]["handler"]["type"], "manual");
}

/// The one case that legitimately cannot be defaulted: a client has nowhere to
/// connect until it is told. It must say so rather than fail silently.
#[test]
fn a_client_form_requires_an_address_before_it_can_start() {
    let form = FormModel::for_create(Section::Clients, "TCP", None);
    assert_eq!(form.missing_required().as_deref(), Some("remote_addr"));

    // Supplying one makes it startable.
    let mut form = form;
    form.set_field_value(&FieldTarget::RemoteAddr, "127.0.0.1:9".to_string());
    assert!(form.missing_required().is_none());
}

#[tokio::test]
async fn create_server_then_client_then_send_without_an_llm() {
    let state = new_state().await;
    let (tx, _rx) = mpsc::unbounded_channel();

    // 1. Pick TCP and create a server on an OS-assigned port — the [+ server]
    //    flow, with everything else defaulted.
    let mut server_form = FormModel::for_create(Section::Servers, "TCP", None);
    server_form.set_field_value(&FieldTarget::Port, "0".to_string());
    // A static handler so the server answers deterministically (the routing
    // editor's job; set here as raw JSON, which the form also accepts).
    server_form.set_field_value(
        &FieldTarget::EventHandlersJson,
        serde_json::json!([{
            "event_pattern": "tcp_data_received",
            "handler": {
                "type": "static",
                "actions": [{ "type": "send_tcp_data", "data": "pong" }]
            }
        }])
        .to_string(),
    );
    let summary = server_form
        .apply(&state, llm(), &tx)
        .await
        .expect("create server");
    assert!(summary.contains("Started server"), "{summary}");

    let server_id = state.get_all_server_ids().await[0];
    let port = wait_for_port(&state, server_id).await;

    // 2. The projection must offer the [+ client] affordance for a dual
    //    protocol, and that is what drives the prefilled remote address.
    let snapshot = projection::build_snapshot(&state).await;
    let server_row = &snapshot.servers[0];
    assert_eq!(
        server_row.client_counterpart.as_deref(),
        Some("TCP"),
        "TCP has a client counterpart, so [+ client] must be offered"
    );

    // 3. Create the client aimed at our own server. The client was created
    //    interactively, so it defaults to MANUAL routing — but its connect
    //    event is covered by a preceding zero-action static rule ("answer
    //    with nothing"), so establishing the connection never parks. It used
    //    to: the `*` → manual rule matched `tcp_connected`, which the TCP
    //    client handles inline in `connect()`, so `apply` sat waiting for a
    //    human to acknowledge a question nobody needed answered.
    let mut client_form = FormModel::for_create(Section::Clients, "TCP", None);
    client_form.set_field_value(&FieldTarget::RemoteAddr, format!("127.0.0.1:{port}"));
    let summary = client_form
        .apply(&state, llm(), &tx)
        .await
        .expect("create client");
    assert!(summary.contains("Connected client"), "{summary}");
    assert!(
        state.list_intercepts().await.is_empty(),
        "the connect event must not park under the default client routing"
    );

    let client_id = state.get_all_client_ids().await[0];
    for _ in 0..100 {
        if state.has_client_handle(client_id).await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        state.has_client_handle(client_id).await,
        "the command channel must register as part of connect()"
    );

    // 4. Compose a send from the protocol's own vocabulary and deliver it.
    let actions = ComposerModel::vocabulary("TCP", &state);
    assert!(
        actions.iter().any(|a| a.name == "send_tcp_data"),
        "composer must offer the protocol's own actions"
    );
    let mut composer = ComposerModel::new(client_id, "TCP", actions);
    composer.selected = composer
        .actions
        .iter()
        .position(|a| a.name == "send_tcp_data")
        .unwrap();
    composer.choose();
    assert!(
        !composer.fields.is_empty(),
        "send_tcp_data declares parameters"
    );
    let data_index = composer
        .fields
        .iter()
        .position(|f| f.name == "data")
        .expect("send_tcp_data takes a data field");
    composer.fields[data_index].value = "ping-from-dashboard".to_string();

    let action = composer.build_action().expect("build action");
    assert_eq!(action["type"], "send_tcp_data");
    assert_eq!(action["data"], "ping-from-dashboard");

    let outcome = composer.send(&state).await.expect("send");
    assert!(
        matches!(
            outcome,
            netget::state::client_handles::ClientSendOutcome::Sent { .. }
        ),
        "expected Sent, got {outcome:?}"
    );

    // 5. The server received it and its static handler answered — visible in
    //    the per-server request log the dashboard renders.
    let mut seen = false;
    for _ in 0..100 {
        let entries = state
            .list_access_logs_for(Some(AccessLogOwner::Server(server_id.as_u32())), None)
            .await;
        if entries.iter().any(|e| {
            serde_json::to_string(e)
                .unwrap_or_default()
                .contains("ping-from-dashboard")
        }) {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        seen,
        "the server's request log should show the injected send"
    );

    // And the client's own request pane shows the injection.
    let client_entries = state
        .list_access_logs_for(Some(AccessLogOwner::Client(client_id.as_u32())), None)
        .await;
    assert!(
        client_entries
            .iter()
            .any(|e| e.event_type == "injected_action"),
        "the client's request log should record the injected action"
    );

    // 6. The server can message that peer directly — TCP adopted the peer
    //    channel, so the connection advertises it in the projection.
    let snapshot = projection::build_snapshot(&state).await;
    let conn = snapshot.servers[0]
        .conns
        .iter()
        .find(|c| c.can_message)
        .expect("the live TCP connection should accept injected actions");

    let outcome = state
        .send_to_peer(
            server_id,
            conn.id,
            serde_json::json!({"type": "send_tcp_data", "data": "hello-from-server"}),
            Duration::from_secs(10),
        )
        .await
        .expect("send to peer");
    assert!(
        matches!(
            outcome,
            netget::state::client_handles::ClientSendOutcome::Sent { bytes_sent } if bytes_sent > 0
        ),
        "expected Sent, got {outcome:?}"
    );

    // Proof it reached the wire: the client's manual rule parks the received
    // data as a pending question carrying exactly those bytes. (The TCP client
    // reports payloads hex-encoded, and earlier traffic may have parked its
    // own questions, so scan for ours.)
    let expected_hex = hex::encode("hello-from-server");
    let mut delivered = false;
    'outer: for _ in 0..200 {
        for view in state.list_intercepts().await {
            let payload = view.event_data.clone().unwrap_or_default().to_string();
            if payload.contains("hello-from-server") || payload.contains(&expected_hex) {
                delivered = true;
                break 'outer;
            }
            // Not ours (e.g. the client's parked question about the earlier
            // "pong"): answer it with nothing so the loop can surface the next
            // event — exactly what the human does at the dashboard.
            let _ = state.resolve_intercept(view.id, Vec::new()).await;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        delivered,
        "the client should have received the server's message on the wire"
    );

    // The injected send shows in the server's request log too, under this
    // connection.
    let entries = state
        .list_access_logs_for(Some(AccessLogOwner::Server(server_id.as_u32())), None)
        .await;
    assert!(
        entries
            .iter()
            .any(|e| e.event_type == "injected_action" && e.connection_id == Some(conn.id)),
        "the server's request log should record the peer message"
    );
}

/// A parameter that declares a closed value set (`Parameter::with_choices`)
/// is a selector in the composer, not a free-text field: Enter/Space cycle
/// the valid values, the pick submits as a plain string, and an optional
/// field can cycle back to unset. The worked example is tcp's `encoding`
/// (utf8 | hex) — the field a model once had to spell correctly from prose.
#[test]
fn a_choice_parameter_cycles_instead_of_editing_text() {
    use netget::llm::actions::protocol_trait::Protocol;
    use netget::tui::modal::composer::FieldKind;

    let tcp = netget::protocol::server_registry::registry()
        .get("TCP")
        .expect("TCP server registered in a tcp-featured build");
    let actions = tcp.get_sync_actions();

    // The declaration reaches the native tool schemas as an enum.
    let send = actions
        .iter()
        .find(|a| a.name == "send_tcp_data")
        .expect("tcp declares send_tcp_data");
    let schema = send.to_tool_schema();
    assert_eq!(
        schema["function"]["parameters"]["properties"]["encoding"]["enum"],
        serde_json::json!(["utf8", "hex"]),
        "a choice set must be advertised as a JSON-Schema enum"
    );

    // …and reaches the composer as a cycling selector.
    let mut composer = ComposerModel::for_peer(ServerId::new(1), 1, "TCP", actions);
    composer.selected = composer
        .actions
        .iter()
        .position(|a| a.name == "send_tcp_data")
        .unwrap();
    composer.choose();

    let encoding = composer
        .fields
        .iter()
        .position(|f| f.name == "encoding")
        .expect("send_tcp_data has an encoding field");
    assert_eq!(composer.fields[encoding].kind, FieldKind::Choice);
    assert_eq!(composer.fields[encoding].choices, vec!["utf8", "hex"]);

    composer.selected = encoding;
    composer.begin_edit();
    assert!(
        composer.editing.is_none(),
        "a choice field cycles in place; it never opens the text editor"
    );
    assert_eq!(composer.fields[encoding].value, "utf8");
    composer.begin_edit();
    assert_eq!(composer.fields[encoding].value, "hex");

    // The pick submits as a plain string next to the other fields.
    let data = composer
        .fields
        .iter()
        .position(|f| f.name == "data")
        .expect("send_tcp_data has a data field");
    composer.fields[data].value = "48656c6c6f".to_string();
    let action = composer.build_action().expect("build action");
    assert_eq!(action["encoding"], "hex");

    // Optional field: one more step wraps back to unset, and the parameter
    // is then omitted rather than sent empty.
    composer.begin_edit();
    assert_eq!(composer.fields[encoding].value, "");
    let action = composer.build_action().expect("build action");
    assert!(
        action.get("encoding").is_none(),
        "an unset optional choice must be omitted: {action}"
    );
}

#[tokio::test]
async fn a_missing_required_field_is_reported_not_silently_sent() {
    let state = new_state().await;
    let actions = ComposerModel::vocabulary("TCP", &state);
    let mut composer = ComposerModel::new(netget::state::ClientId::new(1), "TCP", actions);
    // Choose an action with a required parameter and leave it empty.
    if let Some(index) = composer
        .actions
        .iter()
        .position(|a| a.parameters.iter().any(|p| p.required))
    {
        composer.selected = index;
        composer.choose();
        let err = composer.build_action().expect_err("required field missing");
        assert!(err.to_string().contains("required"), "{err}");
    }
}

#[tokio::test]
async fn editing_a_server_hot_applies_without_a_restart() {
    let state = new_state().await;
    let (tx, _rx) = mpsc::unbounded_channel();

    let mut form = FormModel::for_create(Section::Servers, "TCP", None);
    form.set_field_value(&FieldTarget::Port, "0".to_string());
    form.apply(&state, llm(), &tx).await.expect("create");
    let server_id = state.get_all_server_ids().await[0];
    wait_for_port(&state, server_id).await;

    // Edit just the instruction: a hot field, so the id must not change.
    let snapshot = projection::build_snapshot(&state).await;
    let mut edit = FormModel::for_edit_server(&snapshot.servers[0]);
    edit.set_field_value(&FieldTarget::Instruction, "answer politely".to_string());
    let summary = edit.apply(&state, llm(), &tx).await.expect("update");

    assert!(
        state.get_server(server_id).await.is_some(),
        "a hot update must not replace the server: {summary}"
    );
    let updated = state.get_server(server_id).await.unwrap();
    assert_eq!(updated.instruction, "answer politely");
}

/// A server created with `port: 0` must open its edit form showing the port it is **bound
/// to**, not the `0` that was asked for.
///
/// The form showed `row.port` — the requested value — while the `host` field beside it
/// already read `local_addr`, so an auto-assigned server presented a real host next to a
/// literal `0`. That is not cosmetic: an edit form is a snapshot the operator changes in
/// place, and submitting the `0` it displays is a request for *a different* random port. The
/// field's own description says changing it restarts the server and drops every connection.
#[tokio::test]
async fn an_auto_assigned_server_edits_with_the_port_it_actually_bound() {
    let state = new_state().await;
    let (tx, _rx) = mpsc::unbounded_channel();

    let mut form = FormModel::for_create(Section::Servers, "TCP", None);
    form.set_field_value(&FieldTarget::Port, "0".to_string());
    form.apply(&state, llm(), &tx).await.expect("create");
    let server_id = state.get_all_server_ids().await[0];
    let bound = wait_for_port(&state, server_id).await;
    assert_ne!(bound, 0, "the OS must have assigned a real port");

    let snapshot = projection::build_snapshot(&state).await;
    let edit = FormModel::for_edit_server(&snapshot.servers[0]);
    let port = edit
        .fields
        .iter()
        .find(|f| matches!(f.target, FieldTarget::Port))
        .expect("port field");

    assert_eq!(
        port.value,
        bound.to_string(),
        "the edit form must show the bound port, not the requested one"
    );
    // `original` must match, or an untouched port field reads as an edit and restarts the
    // server for no reason — the opposite failure, and a worse one.
    assert_eq!(
        port.original, port.value,
        "an untouched port must not count as a change"
    );
}

/// `[ + connect a … client ]` on a server pre-fills the remote address, but a
/// client protocol can still declare a required startup parameter nothing
/// can default (openai's `api_key`). The form must surface that and land the
/// cursor on the missing field rather than fail the connect in the chat.
#[cfg(feature = "openai")]
#[test]
fn a_client_with_a_required_startup_param_is_asked_for_it_not_started_blind() {
    let mut form = FormModel::for_create(Section::Clients, "openai", None);
    form.set_field_value(&FieldTarget::RemoteAddr, "127.0.0.1:9".to_string());
    assert_eq!(form.missing_required().as_deref(), Some("api_key"));

    form.focus_first_missing_required();
    assert_eq!(
        form.selected_field().map(|f| f.label.as_str()),
        Some("api_key")
    );
    assert!(form.focused_button.is_none());

    form.set_field_value(
        &FieldTarget::StartupParam("api_key".to_string()),
        "sk-test".to_string(),
    );
    assert!(form.missing_required().is_none());
}
