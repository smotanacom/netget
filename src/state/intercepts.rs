//! Events parked for a **human** to answer.
//!
//! A `manual` event handler (`EventHandlerType::Manual`) is the fourth way to answer a
//! request, next to static / script / LLM: the event becomes a pending question at the
//! dashboard, the connection waits exactly as it would for a slow model, and whatever
//! actions the operator composes are executed as the answer.
//!
//! Mechanically this is a rendezvous: the dispatcher (inside the connection's event task)
//! calls [`AppState::park_intercept`], gets a `oneshot::Receiver`, and awaits it under the
//! handler's timeout. The dashboard lists pending intercepts from its snapshot and calls
//! [`AppState::resolve_intercept`] with the composed actions — or
//! [`AppState::dismiss_intercept`] to refuse, which the dispatcher sees as "no answer" and
//! turns into the fail-closed path. Nothing here touches a socket; the waiting dispatcher
//! owns the wire.
//!
//! The types live here; the `impl AppState` accessors live in `app_state.rs` (same
//! split as `client_handles.rs`), because `AppState::inner` is module-private.

use serde_json::Value;
use tokio::sync::oneshot;

use crate::state::{ClientId, ServerId};

/// Which instance the parked event belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterceptOwner {
    Server(ServerId),
    Client(ClientId),
}

/// One parked event, held in `AppState` until answered, dismissed, or timed out.
pub struct PendingIntercept {
    pub id: u64,
    pub owner: InterceptOwner,
    pub connection_id: Option<u32>,
    pub event_type: String,
    pub description: String,
    pub event_data: Option<Value>,
    pub created_unix_ms: u64,
    /// Taken by `resolve_intercept`; dropped by `dismiss_intercept`. When the
    /// *receiver* side is gone (the dispatcher timed out or its connection task
    /// was cancelled), the entry is dead and is pruned lazily.
    pub(crate) reply_tx: Option<oneshot::Sender<Vec<Value>>>,
}

/// A cloneable view of a pending intercept, for the dashboard's snapshot.
#[derive(Debug, Clone)]
pub struct InterceptView {
    pub id: u64,
    pub owner: InterceptOwner,
    pub connection_id: Option<u32>,
    pub event_type: String,
    pub description: String,
    pub event_data: Option<Value>,
    pub created_unix_ms: u64,
}

impl PendingIntercept {
    pub(crate) fn view(&self) -> InterceptView {
        InterceptView {
            id: self.id,
            owner: self.owner,
            connection_id: self.connection_id,
            event_type: self.event_type.clone(),
            description: self.description.clone(),
            event_data: self.event_data.clone(),
            created_unix_ms: self.created_unix_ms,
        }
    }

    /// Dead entries: answered/dismissed (`reply_tx` taken) or abandoned (the
    /// dispatcher's receiver dropped on timeout or task cancellation).
    pub(crate) fn is_dead(&self) -> bool {
        match &self.reply_tx {
            None => true,
            Some(tx) => tx.is_closed(),
        }
    }
}

pub(crate) fn now_unix_ms() -> u64 {
    crate::utils::clock::SystemTime::now()
        .duration_since(crate::utils::clock::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
