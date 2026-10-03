//! Types for script-based response handling

use serde::{Deserialize, Serialize};

/// Supported scripting languages
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScriptLanguage {
    Python,
    JavaScript,
    Go,
    Perl,
}

impl ScriptLanguage {
    /// Get the command to execute this language
    pub fn command(&self) -> &'static str {
        match self {
            ScriptLanguage::Python => "python3",
            ScriptLanguage::JavaScript => "node",
            ScriptLanguage::Go => "go",
            ScriptLanguage::Perl => "perl",
        }
    }

    /// Parse from string
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "python" | "python3" => Some(ScriptLanguage::Python),
            "javascript" | "js" | "node" => Some(ScriptLanguage::JavaScript),
            "go" | "golang" => Some(ScriptLanguage::Go),
            "perl" => Some(ScriptLanguage::Perl),
            _ => None,
        }
    }

    /// Convert to string
    pub fn as_str(&self) -> &'static str {
        match self {
            ScriptLanguage::Python => "python",
            ScriptLanguage::JavaScript => "javascript",
            ScriptLanguage::Go => "go",
            ScriptLanguage::Perl => "perl",
        }
    }

    /// Get all available scripting languages
    pub fn all_languages() -> Vec<Self> {
        vec![
            ScriptLanguage::Python,
            ScriptLanguage::JavaScript,
            ScriptLanguage::Go,
            ScriptLanguage::Perl,
        ]
    }

    /// Format all available languages as a quoted, comma-separated list
    /// Example: "'python', 'javascript', or 'go'"
    pub fn all_languages_formatted() -> String {
        let all = Self::all_languages();
        let count = all.len();

        if count == 0 {
            return String::new();
        }

        if count == 1 {
            return format!("'{}'", all[0].as_str());
        }

        let mut result = String::new();
        for (i, lang) in all.iter().enumerate() {
            if i == count - 1 {
                // Last item
                result.push_str(&format!("or '{}'", lang.as_str()));
            } else if i == count - 2 {
                // Second to last
                result.push_str(&format!("'{}', ", lang.as_str()));
            } else {
                // All others
                result.push_str(&format!("'{}', ", lang.as_str()));
            }
        }
        result
    }
}

/// Source of the script code
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptSource {
    /// Script loaded from a file path
    FilePath(String),
    /// Script provided as inline code
    Inline(String),
}

impl ScriptSource {
    /// Source code is configuration, not an unbounded stream.
    pub const MAX_CODE_BYTES: usize = 4 * 1024 * 1024;

    /// Get the script code (either by reading file or returning inline code)
    pub fn get_code(&self) -> Result<String, std::io::Error> {
        match self {
            ScriptSource::FilePath(path) => {
                use std::io::Read;
                let mut options = std::fs::OpenOptions::new();
                options.read(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    // Opening a FIFO must not wait for a writer before metadata can
                    // reject it; validate the opened descriptor to avoid a path race.
                    options.custom_flags(libc::O_NONBLOCK);
                }
                let file = options.open(path)?;
                if !file.metadata()?.is_file() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "script source must be a regular file",
                    ));
                }
                let mut code = String::new();
                file.take(Self::MAX_CODE_BYTES as u64 + 1)
                    .read_to_string(&mut code)?;
                Self::check_size(&code)?;
                Ok(code)
            }
            ScriptSource::Inline(code) => {
                Self::check_size(code)?;
                Ok(code.clone())
            }
        }
    }

    fn check_size(code: &str) -> std::io::Result<()> {
        if code.len() > Self::MAX_CODE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "script source exceeds the 4 MiB cap",
            ));
        }
        Ok(())
    }

    /// Keep filesystem work off async workers; callers include this in their budget.
    pub async fn get_code_async(&self) -> std::io::Result<String> {
        if let Self::Inline(_) = self {
            return self.get_code();
        }
        #[cfg(target_arch = "wasm32")]
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "file-backed scripts are not available in the browser",
        ));
        #[cfg(not(target_arch = "wasm32"))]
        {
            let source = self.clone();
            tokio::task::spawn_blocking(move || source.get_code())
                .await
                .map_err(std::io::Error::other)?
        }
    }
}

/// Script configuration for a server
#[derive(Debug, Clone)]
pub struct ScriptConfig {
    /// Scripting language
    pub language: ScriptLanguage,

    /// Source of the script
    pub source: ScriptSource,

    /// Context types this script handles (e.g., ["ssh_auth", "ssh_banner"] or ["all"])
    pub handles_contexts: Vec<String>,
}

impl ScriptConfig {
    /// Check if this script handles a given event type
    pub fn handles_context(&self, event_type_id: &str) -> bool {
        self.handles_contexts
            .iter()
            .any(|context| context == "all" || context == event_type_id)
    }

    /// Add context types to the handles list
    pub fn add_contexts(&mut self, contexts: Vec<String>) {
        for context in contexts {
            if !self.handles_contexts.contains(&context) {
                self.handles_contexts.push(context);
            }
        }
        // If "all" is present, no need for specific contexts
        if self.handles_contexts.contains(&"all".to_string()) {
            self.handles_contexts = vec!["all".to_string()];
        }
    }

    /// Remove context types from the handles list
    pub fn remove_contexts(&mut self, contexts: &[String]) {
        self.handles_contexts.retain(|c| !contexts.contains(c));
    }
}

/// Structured input sent to scripts
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptInput {
    /// Type of event/context (e.g., "ssh_auth", "ssh_banner", "http_request")
    pub event_type_id: String,

    /// Server information (absent for client-side events). Serialization skips
    /// `None`, so scripts attached to servers see exactly the input shape they
    /// always did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<ServerContext>,

    /// Client information (present only for client-side events)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<ClientContext>,

    /// Connection information (if applicable)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection: Option<ConnectionContext>,

    /// Protocol-specific event data
    pub event: serde_json::Value,
}

/// Server context information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerContext {
    /// Server ID
    pub id: u32,

    /// Listening port
    pub port: u16,

    /// Protocol stack name
    pub stack: String,

    /// Server memory (state storage)
    pub memory: String,

    /// User instructions for the server
    pub instruction: String,
}

/// Client context information (for scripts attached to client protocols)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientContext {
    /// Client ID
    pub id: u32,

    /// Remote address the client is connected to
    pub remote_addr: String,

    /// Protocol name (e.g. "TCP", "Telnet")
    pub protocol: String,

    /// Client memory (state storage)
    pub memory: String,

    /// User instructions for the client
    pub instruction: String,
}

/// Connection context information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionContext {
    /// Connection ID
    pub id: String,

    /// Remote address
    pub remote_addr: String,

    /// Bytes received on this connection
    pub bytes_received: u64,

    /// Bytes sent on this connection
    pub bytes_sent: u64,
}

/// Response from a script - object with actions array
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ScriptResponse {
    pub actions: Vec<serde_json::Value>,
}

/// Parse script response from JSON string
///
/// Scripts should return a JSON object with actions array: `{"actions": [{"type": "...", ...}, ...]}`
/// For backwards compatibility, also accepts a bare array: `[{"type": "...", ...}, ...]`
pub fn parse_script_response(s: &str) -> anyhow::Result<ScriptResponse> {
    // Try parsing as ScriptResponse (object with actions field)
    if let Ok(response) = serde_json::from_str::<ScriptResponse>(s) {
        return Ok(response);
    }

    // Try parsing as bare array for backwards compatibility
    if let Ok(actions) = serde_json::from_str::<Vec<serde_json::Value>>(s) {
        return Ok(ScriptResponse { actions });
    }

    // Neither format worked
    anyhow::bail!(
        "Failed to parse script response. Expected either {{\"actions\": [...]}} or [...]. Input: {}",
        s
    )
}

/// Operations for updating script configuration
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptUpdateOperation {
    /// Replace entire script configuration
    Set,

    /// Add context types to existing configuration
    AddContexts,

    /// Remove context types from existing configuration
    RemoveContexts,

    /// Disable scripts entirely (remove configuration)
    Disable,
}

impl ScriptUpdateOperation {
    /// Parse from string
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "set" => Some(ScriptUpdateOperation::Set),
            "add_contexts" | "add" => Some(ScriptUpdateOperation::AddContexts),
            "remove_contexts" | "remove" => Some(ScriptUpdateOperation::RemoveContexts),
            "disable" | "clear" => Some(ScriptUpdateOperation::Disable),
            _ => None,
        }
    }
}
