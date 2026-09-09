//! HTTP/2 server push — the one type the server actually uses.
//!
//! `push_resource` (`actions.rs`) turns a model action into a JSON directive tagged
//! `_push_directive`; `handle_h2_request` (`h2_server.rs`) decodes each one into a
//! [`PendingPush`] and sends the PUSH_PROMISE + push stream inline, before the main
//! response, because `h2` requires the promise on the parent stream's `SendResponse`.
//!
//! This module used to also carry a `PushManager` (queue + `execute_pushes`) and an
//! mpsc `PushChannel`/`PushReceiver` pair. Nothing ever drove them: `handle_h2_request`
//! constructed a `PushManager`, cloned it into a binding named `_push_manager_clone`,
//! and then did the pushing itself; `create_push_channel` had no callers at all. They
//! are removed rather than kept, because a queue that looks like the push path but is
//! never on it is exactly the trap the second, never-spawned `Http2Server` was (see
//! `src/server/http2/CLAUDE.md`).

use std::collections::HashMap;

/// One resource the model asked to push alongside the current response.
#[derive(Debug, Clone)]
pub struct PendingPush {
    pub path: String,
    pub method: String,
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}
