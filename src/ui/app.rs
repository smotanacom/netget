//! Simplified application state for rolling terminal
//!
//! This module provides the minimal state needed for the rolling terminal interface.
//! Rendering lives in `src/tui/`; this keeps the shared command-line state.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write as _};
use std::path::PathBuf;
use tracing::{debug, warn};

/// Log level for output verbosity
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum LogLevel {
    /// ERROR: Critical errors only
    Error,
    /// WARN: Warnings and errors
    Warn,
    /// INFO: One line per request/response (default)
    #[default]
    Info,
    /// DEBUG: Detailed LLM responses, memory updates, actions
    Debug,
    /// TRACE: Full protocol and LLM content
    Trace,
}

impl LogLevel {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "error" => Some(LogLevel::Error),
            "warn" => Some(LogLevel::Warn),
            "info" | "transactional" => Some(LogLevel::Info),
            "debug" => Some(LogLevel::Debug),
            "trace" | "verbose" => Some(LogLevel::Trace),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Error => "ERROR",
            LogLevel::Warn => "WARN",
            LogLevel::Info => "INFO",
            LogLevel::Debug => "DEBUG",
            LogLevel::Trace => "TRACE",
        }
    }

    /// Cycle to the next log level (only cycles through main 3 levels)
    pub fn cycle(&self) -> Self {
        match self {
            LogLevel::Error => LogLevel::Info,
            LogLevel::Info => LogLevel::Trace,
            LogLevel::Trace => LogLevel::Error,
            // If currently on granular levels (set via CLI/slash), cycle to next main level
            LogLevel::Warn => LogLevel::Info,
            LogLevel::Debug => LogLevel::Trace,
        }
    }

    /// Get the color for this log level (matches output rendering colors)
    pub fn color(&self) -> crossterm::style::Color {
        match self {
            LogLevel::Error => crossterm::style::Color::Red,
            LogLevel::Warn => crossterm::style::Color::Yellow,
            LogLevel::Info => crossterm::style::Color::Green,
            LogLevel::Debug => crossterm::style::Color::Cyan,
            LogLevel::Trace => crossterm::style::Color::Blue,
        }
    }
}

/// Server information for display
#[derive(Debug, Clone)]
pub struct ServerDisplayInfo {
    pub id: String,
    pub protocol: String,
    pub port: u16,
    pub status: String,
    pub connections: usize,
}

/// Client information for display
#[derive(Debug, Clone)]
pub struct ClientDisplayInfo {
    pub id: String,
    pub protocol: String,
    pub remote_addr: String,
    pub status: String,
}

/// Connection information for display
#[derive(Debug, Clone)]
pub struct ConnectionDisplayInfo {
    pub id: u32,
    pub server_id: String,
    pub address: String,
    pub state: String,
}

/// Scheduled task information for display
#[derive(Debug, Clone)]
pub struct TaskDisplayInfo {
    pub id: String,
    pub name: String,
    pub scope: String,
    pub status: String,
    pub task_type: String,
}

/// Connection information for status bar
#[derive(Default, Clone)]
pub struct ConnectionInfo {
    pub mode: String,
    pub protocol: String,
    pub model: String,
    pub local_addr: Option<String>,
    pub remote_addr: Option<String>,
    pub state: String,
}

/// Packet statistics for status bar
#[derive(Default, Clone)]
pub struct PacketStats {
    pub packets_received: u64,
    pub packets_sent: u64,
}

/// Simplified application state for rolling terminal
pub struct App {
    /// Command history
    pub command_history: Vec<String>,
    /// Current position in history (None = not browsing history)
    pub history_position: Option<usize>,
    /// Temporary buffer when browsing history
    pub history_temp_input: Option<String>,
    /// Connection information
    pub connection_info: ConnectionInfo,
    /// Packet statistics
    pub packet_stats: PacketStats,
    /// Current log level
    pub log_level: LogLevel,
    /// Slash command suggestions
    pub slash_suggestions: Vec<String>,
    /// Server list for display
    pub servers: Vec<ServerDisplayInfo>,
    /// Client list for display
    pub clients: Vec<ClientDisplayInfo>,
    /// Connection list for display
    pub connections: Vec<ConnectionDisplayInfo>,
    /// Scheduled tasks for display
    pub tasks: Vec<TaskDisplayInfo>,
    /// Whether to expand all connections (E key toggle)
    pub expand_all_connections: bool,
    /// Next global connection ID to assign
    pub next_global_connection_id: u32,
    /// Mapping from network ConnectionId to global UI ID
    pub connection_id_map: std::collections::HashMap<String, u32>,
    /// System capabilities (for privilege warnings in status bar)
    pub system_capabilities: crate::privilege::SystemCapabilities,
    /// Active and recently-completed conversations
    pub conversations: Vec<crate::state::app_state::ConversationInfo>,
    /// Whether to show the usage stats section
    pub show_usage_stats: bool,
    /// Current system stats (CPU, memory, GPU)
    pub system_stats: crate::system_stats::SystemStats,
    /// Active interactive create/update form (`/create`, `/edit`). When `Some`,
    /// keystrokes drive the form (Enter submits the current field, Esc cancels)
    /// instead of the normal command line.
    pub active_form: Option<crate::cli::management::InteractiveForm>,
}

impl Default for App {
    fn default() -> Self {
        Self {
            command_history: Vec::new(),
            history_position: None,
            history_temp_input: None,
            connection_info: ConnectionInfo::default(),
            packet_stats: PacketStats::default(),
            log_level: LogLevel::default(), // Defaults to INFO
            slash_suggestions: Vec::new(),
            servers: Vec::new(),
            clients: Vec::new(),
            connections: Vec::new(),
            tasks: Vec::new(),
            expand_all_connections: false,
            next_global_connection_id: 1,
            connection_id_map: std::collections::HashMap::new(),
            system_capabilities: crate::privilege::SystemCapabilities::detect(),
            conversations: Vec::new(),
            show_usage_stats: false,
            system_stats: crate::system_stats::SystemStats::default(),
            active_form: None,
        }
    }
}

impl App {
    /// Create a new App with system capabilities (loads history from ~/.netget_history)
    pub fn new(system_capabilities: crate::privilege::SystemCapabilities) -> Self {
        let mut app = Self {
            system_capabilities,
            ..Default::default()
        };
        app.command_history = Self::load_history();
        app
    }

    /// Get the path to the history file
    fn history_file_path() -> Option<PathBuf> {
        dirs::home_dir().map(|mut path| {
            path.push(".netget_history");
            path
        })
    }

    /// Escape newlines and other special characters for storage
    fn escape_for_storage(s: &str) -> String {
        s.replace('\\', "\\\\")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t")
    }

    /// Unescape newlines and other special characters from storage
    fn unescape_from_storage(s: &str) -> String {
        let mut result = String::with_capacity(s.len());
        let mut chars = s.chars();

        while let Some(ch) = chars.next() {
            if ch == '\\' {
                match chars.next() {
                    Some('n') => result.push('\n'),
                    Some('r') => result.push('\r'),
                    Some('t') => result.push('\t'),
                    Some('\\') => result.push('\\'),
                    Some(c) => {
                        result.push('\\');
                        result.push(c);
                    }
                    None => result.push('\\'),
                }
            } else {
                result.push(ch);
            }
        }

        result
    }

    /// Load command history from file
    fn load_history() -> Vec<String> {
        let Some(path) = Self::history_file_path() else {
            warn!("Could not determine home directory for history file");
            return Vec::new();
        };

        if !path.exists() {
            debug!("History file does not exist yet: {:?}", path);
            return Vec::new();
        }

        match File::open(&path) {
            Ok(file) => {
                let reader = BufReader::new(file);
                let history: Vec<String> = reader
                    .lines()
                    .map_while(Result::ok)
                    .filter(|line| !line.trim().is_empty())
                    .map(|line| Self::unescape_from_storage(&line))
                    .collect();
                debug!("Loaded {} commands from history", history.len());
                history
            }
            Err(e) => {
                warn!("Failed to open history file: {}", e);
                Vec::new()
            }
        }
    }

    /// Save command history to file
    pub fn save_history(&self) -> std::io::Result<()> {
        let Some(path) = Self::history_file_path() else {
            return Ok(()); // Silently skip if can't determine home dir
        };

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;

        for command in &self.command_history {
            writeln!(file, "{}", Self::escape_for_storage(command))?;
        }

        debug!("Saved {} commands to history", self.command_history.len());
        Ok(())
    }

    /// Add command to history (deduplicates)
    pub fn add_to_history(&mut self, command: String) {
        if !command.trim().is_empty()
            && (self.command_history.is_empty() || self.command_history.last() != Some(&command))
        {
            self.command_history.push(command);
        }
        // Reset history navigation state so pressing up shows the last command
        self.history_position = None;
        self.history_temp_input = None;
    }

    /// Get or allocate a global connection ID for a network connection
    pub fn get_or_allocate_connection_id(&mut self, network_conn_id: String) -> u32 {
        if let Some(&id) = self.connection_id_map.get(&network_conn_id) {
            id
        } else {
            let id = self.next_global_connection_id;
            self.next_global_connection_id += 1;
            self.connection_id_map.insert(network_conn_id, id);
            id
        }
    }

    /// Remove a connection from the ID map (when connection closes)
    pub fn remove_connection_id(&mut self, network_conn_id: &str) {
        self.connection_id_map.remove(network_conn_id);
    }

    /// Update slash command suggestions based on input
    pub fn update_slash_suggestions(&mut self, input: &str) {
        // Only show suggestions if input starts with "/"
        if !input.starts_with('/') {
            self.slash_suggestions.clear();
            return;
        }

        // Define all available slash commands
        let all_commands = vec![
            "/exit - Exit the application",
            "/model [<name>] - List/select a model",
            "/log [<level>] - Show/set log level (error, warn, info, debug, trace)",
            "/script [<env>] - Show/set scripting environment (llm, python, javascript, go)",
            "/web [on/off/ask] - Show/set web search mode",
            "/test_ask - Test web search approval prompt",
            "/backend [ollama|openai <url>] - Show/switch LLM backend",
            "/docs [<protocol>] - Show protocol documentation",
            "/stability - List protocols grouped by development state",
            "/manage - List running servers/clients and show create/update shapes",
            "/create <protocol> [--client] - Interactive prefilled create form",
            "/edit <id> - Interactive form prefilled with a running instance's config",
            "/update <id> <instruction> - Update a running server/client in place",
        ];

        // Filter commands based on current input
        let input_lower = input.to_lowercase();
        self.slash_suggestions = all_commands
            .into_iter()
            .filter(|cmd| cmd.to_lowercase().starts_with(&input_lower))
            .map(|s| s.to_string())
            .collect();
    }

    /// Set log level
    pub fn set_log_level(&mut self, level: LogLevel) {
        self.log_level = level;
    }

    /// Toggle expand all connections
    pub fn toggle_expand_all(&mut self) {
        self.expand_all_connections = !self.expand_all_connections;
    }

    /// Toggle usage stats visibility
    pub fn toggle_usage_stats(&mut self) {
        self.show_usage_stats = !self.show_usage_stats;
    }

    /// Legacy compatibility: add_llm_message (no-op in rolling terminal, output goes to stdout)
    pub fn add_llm_message(&mut self, _message: String) {
        // In rolling terminal mode, messages are printed directly to stdout
        // This method exists for compatibility with event handler
    }
}
