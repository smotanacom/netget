//! Client protocol trait
//!
//! This module defines the trait that all client protocols must implement
//! to provide their own action systems.

use super::protocol_trait::Protocol;
use super::ActionDefinition;
use crate::state::app_state::AppState;
use anyhow::Result;

/// Result of executing a client action
#[derive(Debug)]
pub enum ClientActionResult {
    /// Data to send to the server
    SendData(Vec<u8>),

    /// Disconnect from the server
    Disconnect,

    /// Wait for more data before responding (accumulating state)
    WaitForMore,

    /// No action needed (e.g., logging, state update)
    NoAction,

    /// Multiple results (e.g., send data + disconnect)
    Multiple(Vec<ClientActionResult>),

    /// Custom protocol-specific result with structured data
    ///
    /// This is used when a client needs to return structured information
    /// that isn't just "send these bytes". Clients encode their responses
    /// as JSON in the 'data' field, and the client handler decodes and
    /// processes them.
    ///
    /// Examples:
    /// - HTTP: {"name": "http_request", "data": {"method": "GET", "path": "/"}}
    /// - Redis: {"name": "redis_command", "data": {"command": "SET", "args": ["key", "value"]}}
    /// - SSH: {"name": "ssh_command", "data": {"command": "ls -la"}}
    Custom {
        name: String,
        data: serde_json::Value,
    },
}

impl ClientActionResult {
    /// Check if this result contains data to send
    pub fn has_data(&self) -> bool {
        match self {
            ClientActionResult::SendData(_) => true,
            ClientActionResult::Multiple(results) => results.iter().any(|r| r.has_data()),
            _ => false,
        }
    }

    /// Check if this result disconnects
    pub fn disconnects(&self) -> bool {
        match self {
            ClientActionResult::Disconnect => true,
            ClientActionResult::Multiple(results) => results.iter().any(|r| r.disconnects()),
            _ => false,
        }
    }

    /// Check if this result waits for more data
    pub fn waits_for_more(&self) -> bool {
        match self {
            ClientActionResult::WaitForMore => true,
            ClientActionResult::Multiple(results) => results.iter().any(|r| r.waits_for_more()),
            _ => false,
        }
    }

    /// Extract all data from results
    pub fn get_all_data(&self) -> Vec<Vec<u8>> {
        match self {
            ClientActionResult::SendData(data) => vec![data.clone()],
            ClientActionResult::Multiple(results) => {
                results.iter().flat_map(|r| r.get_all_data()).collect()
            }
            _ => Vec::new(),
        }
    }
}

/// Trait for client protocol implementations
///
/// Each client protocol implements both the Protocol trait (for common functionality)
/// and this Client trait (for client-specific functionality like connecting).
///
/// The Client trait provides:
/// 1. Client connection - how to connect to a remote server
/// 2. Action executor - parses and executes client actions
pub trait Client: Protocol {
    /// Connect to a remote server for this protocol
    ///
    /// This is called when a client needs to be started. The implementation
    /// should connect to the remote address, set up any necessary resources,
    /// and return the connected local socket address.
    ///
    /// # Arguments
    /// * `ctx` - Connect context with all necessary dependencies
    ///
    /// # Returns
    /// * `Ok(SocketAddr)` - The actual local address of the connection
    /// * `Err(_)` - If connection failed
    fn connect(
        &self,
        ctx: crate::protocol::ConnectContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    >;

    /// Execute a protocol-specific action
    ///
    /// # Arguments
    /// * `action` - The action JSON object from LLM
    ///
    /// # Returns
    /// * `Ok(ClientActionResult)` - Result of execution (data to send, disconnect, etc.)
    /// * `Err(_)` - If action execution failed
    fn execute_action(&self, action: serde_json::Value) -> Result<ClientActionResult>;
}

/// The exact protocol-specific action set that
/// [`crate::llm::action_helper::call_llm_for_client`] advertises to the model.
///
/// # Why this is a union, when the server path is a narrowing
///
/// `call_llm` (servers) advertises `event.event_type.actions` and treats that narrowing as
/// authoritative: SSH's `ssh_auth` accepts `ssh_auth_decision` and nothing else, and unioning
/// in `get_sync_actions()` there would offer the model `sftp_error` as a way to answer an
/// authentication request. That works because servers have **two** LLM entry points — one for
/// user input (async actions) and one for network events (sync/event actions) — so the
/// async/sync split carries real meaning and an event can express a narrowing.
///
/// Clients have **one**. `call_llm_for_client` serves both the initial instruction
/// (`event: None`) and every subsequent network event, so no client can express a narrowing:
/// a "sync" action on a client is just an action that happens to make sense mid-connection.
/// The split is vestigial, and the tree shows it — of 91 client protocols, 85 attach no
/// actions to any event type at all, and roughly forty duplicate their entire list verbatim
/// into both `get_async_actions()` and `get_sync_actions()` purely to work around this
/// function's former shape.
///
/// # The defect this fixes
///
/// `call_llm_for_client` used to build its tool list from `get_async_actions()` **alone**. It
/// never read `get_sync_actions()`, and it never read `event.event_type.actions`. Any action
/// declared only as sync, or only attached to an event, was therefore rejected at runtime as
/// an unknown action — advertised nowhere, so the model could not name it, and rejected when
/// it guessed. TFTP was the case that surfaced it: `send_ack` was sync-only, so every DATA
/// block came back `Unknown Action` and a transfer stalled at block 1. **53 of the 91 client
/// protocols had at least one action invisible this way**, 11 of them a protocol-specific one
/// (`send_privmsg`, `send_apdu`, `sign_request`, `write_characteristic`, …) and the rest
/// `wait_for_more`, which left every stream client unable to say "that response was partial".
///
/// Ordering is `async`, then `sync`, then the event's own list, deduplicated by name with the
/// first occurrence winning — so a protocol that declares an action in more than one list (the
/// common shape) gets its async description, which is the one written for a model choosing
/// what to do next.
pub fn client_llm_action_set(
    protocol: &dyn Client,
    state: &AppState,
    event: Option<&crate::protocol::Event>,
) -> Vec<ActionDefinition> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<ActionDefinition> = Vec::new();

    let event_actions = event
        .map(|e| e.event_type.actions.clone())
        .unwrap_or_default();

    for action in protocol
        .get_async_actions(state)
        .into_iter()
        .chain(protocol.get_sync_actions())
        .chain(event_actions)
    {
        if seen.insert(action.name.clone()) {
            out.push(action);
        }
    }

    out
}

/// The action names a **deterministic handler** (`{"type": "static", …}`) for `pattern` may
/// name on this client, or `None` if the client declares no event matching `pattern`.
///
/// This is [`client_llm_action_set`]'s rule applied to the other consumer of a protocol's
/// vocabulary. `events::handler::action_catalog_for_pattern` validates a handler's action
/// names at startup, and it built that catalog from `get_sync_actions()` plus the matching
/// event types' own actions — never `get_async_actions()`. That is the same defect
/// `client_llm_action_set` was written to fix, one layer over: WHOIS declares `query_whois`
/// async-only, so `{"event_pattern": "whois_connected", "handler": {"type": "static",
/// "actions": [{"type": "query_whois", …}]}}` — the client's own documented static-mode
/// startup example — was **rejected at startup** as an unknown action, and the client could
/// not be driven deterministically at all.
///
/// The reasoning is the one on [`client_llm_action_set`] and it is why this function exists
/// rather than the server rule being loosened for everyone: **clients union, servers narrow.**
/// A server has two LLM entry points, so `ssh_auth` narrowing to `ssh_auth_decision` is a real
/// statement about what may answer an authentication request, and a static handler for that
/// event should be held to it. A client has one entry point and therefore cannot express a
/// narrowing — its async/sync split is vestigial — so anything it would offer the model, a
/// handler may name. Handlers and the model must see the same vocabulary: a verb the model can
/// use but a script or static handler cannot is a difference nothing in the client means.
///
/// `state` is only ever passed through to `get_async_actions`, which no client reads (all 100
/// implementations take `_state`); enumerating names does not need the live state.
pub fn client_action_names_for_pattern(
    protocol: &dyn Client,
    state: &AppState,
    pattern: &crate::scripting::EventPattern,
) -> Option<Vec<String>> {
    let event_types = protocol.get_event_types();
    let matching: Vec<_> = event_types
        .iter()
        .filter(|et| pattern.matches(&et.id))
        .collect();
    if matching.is_empty() {
        return None;
    }

    // async ∪ sync, exactly as the model is shown it…
    let mut names: Vec<String> = client_llm_action_set(protocol, state, None)
        .into_iter()
        .map(|a| a.name)
        .collect();
    // …plus whatever the matching events attach, which is the third term of the union that
    // `client_llm_action_set` adds once an event is actually firing.
    for event_type in matching {
        names.extend(event_type.actions.iter().map(|a| a.name.clone()));
    }
    Some(names)
}

/// Audit a client protocol's action declarations against what the model can actually see.
///
/// This is the client-side counterpart of
/// [`crate::llm::actions::protocol_trait::audit_event_action_declarations`], and it exists
/// because the server audit walks the **server** registry only: ~90 registered clients had no
/// guard of any kind, which is how the `get_async_actions()`-only tool list survived.
///
/// Two things are checked, and neither is vacuous:
///
/// 1. **The client offers the model something.** The effective set — what
///    [`client_llm_action_set`] returns — must be non-empty for at least the no-event case.
///    A client with no actions anywhere cannot be driven at all. (The server audit used to
///    early-return when `get_sync_actions()` was empty and so passed hardest on the most
///    broken protocol in the tree; that hole is deliberately not reproduced here.)
/// 2. **Nothing the client declares is invisible.** Every action in `get_sync_actions()` and
///    every action attached to an event type must appear in the effective set. This is the
///    actual regression guard: revert `call_llm_for_client` to `get_async_actions()` alone
///    and this fails for every client whose lists differ.
///
/// An event marked [`crate::protocol::EventType::with_no_actions`] is the deliberate case and
/// contributes nothing to check.
pub fn audit_client_action_declarations(protocol: &dyn Client, state: &AppState) -> Vec<String> {
    let mut findings = Vec::new();

    let visible = client_llm_action_set(protocol, state, None);
    let visible_names: std::collections::HashSet<&str> =
        visible.iter().map(|a| a.name.as_str()).collect();

    if visible.is_empty() {
        findings.push(format!(
            "client '{}' advertises no actions at all: get_async_actions(), get_sync_actions() \
             and every event type are empty, so the model is handed nothing protocol-specific \
             and anything it returns is rejected as an unknown action. The client cannot be \
             driven.",
            protocol.protocol_name()
        ));
    }

    let mut missing_sync: Vec<String> = protocol
        .get_sync_actions()
        .into_iter()
        .filter(|a| !visible_names.contains(a.name.as_str()))
        .map(|a| a.name)
        .collect();
    missing_sync.sort();
    missing_sync.dedup();
    if !missing_sync.is_empty() {
        findings.push(format!(
            "client '{}' declares {} sync action(s) the model is never shown: {}. \
             call_llm_for_client must advertise the union of async, sync and the event's own \
             actions (see client_llm_action_set); anything it omits is rejected at runtime as \
             an unknown action.",
            protocol.protocol_name(),
            missing_sync.len(),
            missing_sync.join(", ")
        ));
    }

    for event_type in protocol.get_event_types() {
        // An event with a deliberately empty list has nothing to hide.
        let mut missing_event: Vec<String> = event_type
            .actions
            .iter()
            .filter(|a| !visible_names.contains(a.name.as_str()))
            .map(|a| a.name.clone())
            .collect();
        missing_event.sort();
        missing_event.dedup();
        if !missing_event.is_empty() {
            findings.push(format!(
                "client '{}' attaches action(s) to event '{}' that the model is never shown: \
                 {}. Either add them to get_async_actions()/get_sync_actions(), or fix \
                 call_llm_for_client to advertise the event's own list.",
                protocol.protocol_name(),
                event_type.id,
                missing_event.join(", ")
            ));
        }
    }

    findings
}
