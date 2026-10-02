//! Script-based response handling system
//!
//! This module provides support for Python and JavaScript scripts to handle
//! protocol responses deterministically, with fallback to LLM for complex cases.

pub mod environment;
pub mod event_handler;
pub mod executor;
pub mod highlight;
pub mod manager;
pub mod process_io;
pub mod resident;
pub mod types;

// Re-export commonly used types
pub use environment::ScriptingEnvironment;
pub use event_handler::{
    EventHandler, EventHandlerConfig, EventHandlerType, EventPattern, DEFAULT_MANUAL_TIMEOUT_SECS,
};
pub use executor::{
    execute_script, execute_script_async, execute_script_with_timeout_async,
    DEFAULT_SCRIPT_TIMEOUT, SCRIPT_TIMEOUT_SECS,
};
pub use manager::ScriptManager;
pub use resident::{
    resident_available, resident_language_supported, ResidentScope, ResidentScript,
    ResidentScriptManager,
};
pub use types::{
    ConnectionContext, ScriptConfig, ScriptInput, ScriptLanguage, ScriptResponse, ScriptSource,
    ServerContext,
};
