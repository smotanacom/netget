//! Easy protocol instance management

use crate::utils::clock::Instant;
use tokio::task::JoinHandle;

use super::client::ClientId;
use super::server::ServerId;

/// Unique identifier for an easy protocol instance
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EasyId(u32);

impl EasyId {
    /// Create a new easy ID from a u32
    pub fn new(id: u32) -> Self {
        Self(id)
    }

    /// Get the raw ID value
    pub fn as_u32(&self) -> u32 {
        self.0
    }

    /// Parse from string (expects format "easy-123" or just "123")
    pub fn from_string(s: &str) -> Option<Self> {
        let s = s.trim();
        let id_str = s.strip_prefix("easy-").unwrap_or(s);
        id_str.parse::<u32>().ok().map(Self)
    }
}

impl std::fmt::Display for EasyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "easy-{}", self.0)
    }
}

/// Status of an easy protocol instance
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EasyStatus {
    /// Easy protocol is starting up (creating underlying server/client)
    Starting,
    /// Easy protocol is running (underlying server/client active)
    Running,
    /// Easy protocol has been stopped
    Stopped,
    /// Easy protocol encountered an error
    Error(String),
}

impl EasyStatus {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Starting => "Starting",
            Self::Running => "Running",
            Self::Stopped => "Stopped",
            Self::Error(_) => "Error",
        }
    }
}

impl std::fmt::Display for EasyStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Starting => write!(f, "Starting"),
            Self::Running => write!(f, "Running"),
            Self::Stopped => write!(f, "Stopped"),
            Self::Error(msg) => write!(f, "Error: {}", msg),
        }
    }
}

/// An easy protocol instance that wraps an underlying server or client
#[derive(Debug)]
pub struct EasyInstance {
    /// Unique easy protocol ID
    pub id: EasyId,
    /// Easy protocol name (e.g., "http-easy")
    pub protocol_name: String,
    /// Underlying protocol name (e.g., "http")
    pub underlying_protocol: String,
    /// Optional user instruction for this easy instance
    pub user_instruction: Option<String>,
    /// Underlying server ID (if this easy protocol wraps a server)
    pub underlying_server_id: Option<ServerId>,
    /// Underlying client ID (if this easy protocol wraps a client)
    pub underlying_client_id: Option<ClientId>,
    /// Easy protocol status
    pub status: EasyStatus,
    /// When the easy protocol was created
    pub created_at: Instant,
    /// When the status last changed
    pub status_changed_at: Instant,
    /// Task handle for cleanup (if needed)
    pub handle: Option<JoinHandle<()>>,
}

impl EasyInstance {
    /// Create a new easy protocol instance
    pub fn new(
        id: EasyId,
        protocol_name: String,
        underlying_protocol: String,
        user_instruction: Option<String>,
    ) -> Self {
        let now = Instant::now();
        Self {
            id,
            protocol_name,
            underlying_protocol,
            user_instruction,
            underlying_server_id: None,
            underlying_client_id: None,
            status: EasyStatus::Starting,
            created_at: now,
            status_changed_at: now,
            handle: None,
        }
    }

    /// Set underlying server ID
    pub fn set_underlying_server(&mut self, server_id: ServerId) {
        self.underlying_server_id = Some(server_id);
    }

    /// Set underlying client ID
    pub fn set_underlying_client(&mut self, client_id: ClientId) {
        self.underlying_client_id = Some(client_id);
    }

    /// Update status
    pub fn set_status(&mut self, status: EasyStatus) {
        self.status = status;
        self.status_changed_at = Instant::now();
    }
}
