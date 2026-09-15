use crate::utils::clock::Instant;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use super::client::ClientId;
use super::server::ServerId;
use crate::server::connection::ConnectionId;

/// Unique identifier for a scheduled task
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(u64);

impl TaskId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }

    pub fn as_u64(&self) -> u64 {
        self.0
    }

    pub fn from_string(s: &str) -> Option<Self> {
        s.parse::<u64>().ok().map(Self)
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Task execution scope
#[derive(Debug, Clone)]
pub enum TaskScope {
    /// Global task - uses user input prompt with all common actions
    Global,
    /// Server-scoped task - uses server's instruction and protocol actions
    Server(ServerId),
    /// Connection-scoped task - uses server's instruction and protocol actions
    /// for a specific connection. Automatically cleaned up when connection closes.
    Connection(ServerId, ConnectionId),
    /// Client-scoped task - uses client's instruction and protocol actions
    /// Automatically cleaned up when client disconnects.
    Client(ClientId),
}

/// Task type
#[derive(Debug, Clone)]
pub enum TaskType {
    /// Execute once after delay
    OneShot { delay_secs: u64 },
    /// Execute repeatedly at interval
    Recurring {
        interval_secs: u64,
        max_executions: Option<u64>,
        executions_count: u64,
    },
}

/// Task status
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskStatus {
    /// Scheduled, waiting to execute
    Scheduled,
    /// Currently executing
    Executing,
    /// Completed (one-shot) or cancelled
    Completed,
    /// Failed with error
    Failed(String),
}

/// Scheduled task state
#[derive(Debug, Clone)]
pub struct ScheduledTask {
    /// Unique task ID
    pub id: TaskId,
    /// Human-readable task ID string (e.g., "cleanup_logs")
    pub name: String,
    /// Task scope (global or server-specific)
    pub scope: TaskScope,
    /// Task type (one-shot or recurring)
    pub task_type: TaskType,
    /// Instruction for LLM
    pub instruction: String,
    /// Optional context data
    pub context: Option<serde_json::Value>,
    /// Current status
    pub status: TaskStatus,
    /// When task was created
    pub created_at: Instant,
    /// When task will execute next
    pub next_execution: Instant,
    /// Previous execution error (for retry logic)
    pub last_error: Option<String>,
    /// Number of consecutive failures
    pub failure_count: u64,
}

impl ScheduledTask {
    /// Create a new one-shot task
    pub fn new_one_shot(
        id: TaskId,
        name: String,
        scope: TaskScope,
        delay_secs: u64,
        instruction: String,
        context: Option<serde_json::Value>,
    ) -> Self {
        let now = Instant::now();
        Self {
            id,
            name,
            scope,
            task_type: TaskType::OneShot { delay_secs },
            instruction,
            context,
            status: TaskStatus::Scheduled,
            created_at: now,
            next_execution: now + Duration::from_secs(delay_secs),
            last_error: None,
            failure_count: 0,
        }
    }

    /// Create a new recurring task
    pub fn new_recurring(
        id: TaskId,
        name: String,
        scope: TaskScope,
        interval_secs: u64,
        max_executions: Option<u64>,
        instruction: String,
        context: Option<serde_json::Value>,
    ) -> Self {
        let now = Instant::now();
        Self {
            id,
            name,
            scope,
            task_type: TaskType::Recurring {
                interval_secs,
                max_executions,
                executions_count: 0,
            },
            instruction,
            context,
            status: TaskStatus::Scheduled,
            created_at: now,
            next_execution: now + Duration::from_secs(interval_secs),
            last_error: None,
            failure_count: 0,
        }
    }

    /// Create a new connection-scoped one-shot task
    pub fn new_connection_one_shot(
        id: TaskId,
        name: String,
        server_id: ServerId,
        connection_id: ConnectionId,
        delay_secs: u64,
        instruction: String,
        context: Option<serde_json::Value>,
    ) -> Self {
        Self::new_one_shot(
            id,
            name,
            TaskScope::Connection(server_id, connection_id),
            delay_secs,
            instruction,
            context,
        )
    }

    /// Create a new connection-scoped recurring task
    #[allow(clippy::too_many_arguments)]
    pub fn new_connection_recurring(
        id: TaskId,
        name: String,
        server_id: ServerId,
        connection_id: ConnectionId,
        interval_secs: u64,
        max_executions: Option<u64>,
        instruction: String,
        context: Option<serde_json::Value>,
    ) -> Self {
        Self::new_recurring(
            id,
            name,
            TaskScope::Connection(server_id, connection_id),
            interval_secs,
            max_executions,
            instruction,
            context,
        )
    }

    /// Get the interval for recurring tasks
    pub fn interval_secs(&self) -> Option<u64> {
        match &self.task_type {
            TaskType::Recurring { interval_secs, .. } => Some(*interval_secs),
            _ => None,
        }
    }

    /// Get execution count for recurring tasks
    pub fn executions_count(&self) -> u64 {
        match &self.task_type {
            TaskType::Recurring {
                executions_count, ..
            } => *executions_count,
            _ => 0,
        }
    }

    /// Check if recurring task has reached max executions
    pub fn is_max_executions_reached(&self) -> bool {
        match &self.task_type {
            TaskType::Recurring {
                max_executions: Some(max),
                executions_count,
                ..
            } => executions_count >= max,
            _ => false,
        }
    }

    /// Format task for display in prompts
    pub fn format_for_prompt(&self) -> String {
        let scope_str = match &self.scope {
            TaskScope::Global => "global".to_string(),
            TaskScope::Server(sid) => format!("server #{}", sid.as_u32()),
            TaskScope::Connection(sid, cid) => {
                format!("connection {} on server #{}", cid, sid.as_u32())
            }
            TaskScope::Client(cid) => format!("client #{}", cid.as_u32()),
        };

        let type_str = match &self.task_type {
            TaskType::OneShot { delay_secs } => {
                format!("one-shot ({}s delay)", delay_secs)
            }
            TaskType::Recurring {
                interval_secs,
                max_executions,
                executions_count,
            } => {
                if let Some(max) = max_executions {
                    format!(
                        "recurring ({}s interval, {}/{} executions)",
                        interval_secs, executions_count, max
                    )
                } else {
                    format!(
                        "recurring ({}s interval, {} executions)",
                        interval_secs, executions_count
                    )
                }
            }
        };

        let next_in = if self.next_execution > Instant::now() {
            let secs = (self.next_execution - Instant::now()).as_secs();
            format!("{}s", secs)
        } else {
            "now".to_string()
        };

        format!(
            "- Task '{}' (ID: {}): {} scope, {}, next in {}, status: {:?}",
            self.name, self.id, scope_str, type_str, next_in, self.status
        )
    }
}

/// Task execution result
pub struct TaskExecutionResult {
    /// Whether execution succeeded
    pub success: bool,
    /// Actions returned by LLM/script
    pub actions: Vec<serde_json::Value>,
    /// Optional error message
    pub error: Option<String>,
}

/// Format duration as human-readable string
pub fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}
