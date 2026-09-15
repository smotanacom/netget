//! An LLM backend that asks the host to answer.
//!
//! The browser build has no HTTP client it could point at Ollama and no model of its own;
//! what it has is a page. The page may run a model in-browser (WebLLM over WebGPU), forward
//! to a local Ollama the user opened up to it, or show the request to the person at the
//! keyboard and let *them* be the model. NetGet does not need to know which: every request
//! `OllamaClient` would have sent over HTTP becomes a [`BridgeRequest`] on a channel, and
//! whoever drains the channel answers it with a [`BridgeReply`].
//!
//! The request carries everything the wire request would have: the model name, the full
//! message list (system prompt included) and the tool schemas. That is deliberate: the demo
//! page shows all of it, so a visitor sees exactly what NetGet says to a model and what comes
//! back. Nothing here is browser-specific; a native test can drain the same channel, which is
//! how the mapping from request to `ChatResponse` is verified.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::llm::ollama_client::Message;

/// Which client entry point produced the request. The host may treat them alike (both are
/// "here are messages, give me a completion"); the distinction is kept because the two
/// paths parse the answer differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeRequestKind {
    /// `generate_with_format`: one flattened prompt, the answer is parsed from its text.
    /// Every network event goes this way.
    Generate,
    /// `chat_with_tools`: structured messages plus tool schemas; the answer may carry
    /// native tool calls. User chat input and scheduled tasks go this way.
    Chat,
}

/// One LLM request, as the host sees it.
#[derive(Debug, Serialize)]
pub struct BridgeRequest {
    pub id: u64,
    pub kind: BridgeRequestKind,
    pub model: String,
    /// The full prompt. For `Generate` this is a single `user` message holding the
    /// flattened prompt; for `Chat` it is the conversation, system message first.
    pub messages: Vec<Message>,
    /// Tool schemas in OpenAI function-tool format; empty for `Generate`.
    pub tools: Vec<serde_json::Value>,
    /// Where the answer goes. Dropping it without answering fails the request.
    #[serde(skip)]
    pub reply: oneshot::Sender<Result<BridgeReply, String>>,
}

/// A native tool call in the host's answer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BridgeToolCall {
    #[serde(default)]
    pub id: Option<String>,
    /// Accepts either spelling so a host can pass an OpenAI `function.name` through
    /// untouched.
    #[serde(default, alias = "function_name")]
    pub name: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// The host's answer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BridgeReply {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<BridgeToolCall>,
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
}

/// The channel between `OllamaClient` and the host.
pub struct LlmBridge {
    tx: mpsc::UnboundedSender<BridgeRequest>,
    next_id: AtomicU64,
    models: Mutex<Vec<String>>,
}

impl LlmBridge {
    /// Create a bridge and the receiver the host drains.
    pub fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<BridgeRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                tx,
                next_id: AtomicU64::new(1),
                models: Mutex::new(Vec::new()),
            }),
            rx,
        )
    }

    /// Hand a request to the host. The returned receiver yields the answer; an `Err` inside
    /// it is the host's own error text, an `Err` on the receive is the host dropping the
    /// request unanswered.
    pub fn submit(
        &self,
        kind: BridgeRequestKind,
        model: String,
        messages: Vec<Message>,
        tools: Vec<serde_json::Value>,
    ) -> (u64, oneshot::Receiver<Result<BridgeReply, String>>) {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (reply, rx) = oneshot::channel();
        let request = BridgeRequest {
            id,
            kind,
            model,
            messages,
            tools,
            reply,
        };
        // A closed receiver means no host; the request's reply sender is dropped with it,
        // which the caller sees as an unanswered request.
        let _ = self.tx.send(request);
        (id, rx)
    }

    /// What `list_models` reports. The host sets this to whatever it can run.
    pub fn set_models(&self, models: Vec<String>) {
        *self.models.lock().expect("bridge model list") = models;
    }

    pub fn models(&self) -> Vec<String> {
        self.models.lock().expect("bridge model list").clone()
    }
}
