use anyhow::Result;
use serde_json::Value as JsonValue;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::llm::OllamaClient;
use crate::protocol::Event;
use crate::state::AppState;

/// Easy protocol trait - simplified LLM interaction mode for "dumb models"
///
/// Easy protocols act as a translation layer between network events and simplified LLM prompts.
/// They use existing Server/Client protocols underneath but provide a simpler interface where:
/// - LLM responds in natural language (Markdown) instead of JSON actions
/// - Network details are abstracted into conversational prompts
/// - Protocol-specific complexity is hidden from the LLM
///
/// Architecture:
/// 1. Easy protocol generates open_server/open_client action for underlying protocol
/// 2. Underlying protocol handles network I/O and fires events
/// 3. EventHandler routes events to Easy protocol handler
/// 4. Easy protocol transforms event into simplified prompt
/// 5. LLM responds in Markdown (not JSON actions)
/// 6. Easy protocol transforms response into protocol actions
/// 7. Actions are executed on underlying protocol
pub trait Easy: Send + Sync {
    /// Protocol name (e.g., "http-easy")
    fn protocol_name(&self) -> &'static str;

    /// Underlying protocol name (e.g., "http")
    fn underlying_protocol(&self) -> &'static str;

    /// Default port for this Easy protocol (e.g., 8080 for http-easy)
    fn default_port(&self) -> Option<u16>;

    /// Generate startup action to create underlying server or client
    ///
    /// Returns a JSON action (open_server or open_client) that will be executed
    /// to start the underlying protocol instance.
    ///
    /// # Arguments
    /// * `user_instruction` - Optional custom instruction from user (e.g., "Give cooking recipes")
    /// * `port` - Optional port override (if None, uses default_port)
    ///
    /// # Returns
    /// JSON action object for underlying protocol
    fn generate_startup_action(
        &self,
        user_instruction: Option<String>,
        port: Option<u16>,
    ) -> Result<JsonValue>;

    /// Handle network event from underlying protocol
    ///
    /// Transforms network event into simplified LLM prompt, calls LLM, and returns
    /// protocol actions to execute on underlying protocol.
    ///
    /// # Arguments
    /// * `event` - Network event from underlying protocol (owned for async move)
    /// * `user_instruction` - Optional user instruction for this Easy instance
    /// * `llm_client` - LLM client for making simplified calls
    /// * `app_state` - Application state
    ///
    /// # Returns
    /// Vector of JSON actions to execute on underlying protocol
    fn handle_event(
        &self,
        event: Event,
        user_instruction: Option<String>,
        llm_client: Arc<OllamaClient>,
        app_state: Arc<AppState>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<JsonValue>>> + Send>>;

    /// Get list of event type IDs this Easy protocol handles
    ///
    /// Used by EventHandler to route events to Easy protocol.
    /// Should return event type IDs from underlying protocol (e.g., ["http_request"])
    fn get_handled_event_type_ids(&self) -> Vec<&'static str>;

    /// Get description of this Easy protocol for help messages
    fn description(&self) -> &'static str;
}
