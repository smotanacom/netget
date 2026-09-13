//! SQLite database management
//!
//! Provides database instances, schema tracking, and query execution for protocols

use crate::utils::clock::Instant;
#[cfg(feature = "sqlite")]
use anyhow::{Context, Result};
#[cfg(feature = "sqlite")]
use rusqlite::Connection;
#[cfg(feature = "sqlite")]
use std::collections::HashMap;
#[cfg(feature = "sqlite")]
use std::path::PathBuf;
#[cfg(feature = "sqlite")]
use std::sync::Mutex; // Always import for DatabaseInstance fields

use crate::state::{ClientId, ServerId};

/// Path value that marks an in-memory (non-file-backed) database.
pub const MEMORY_DATABASE_PATH: &str = ":memory:";

/// Longest accepted database name.
pub const MAX_DATABASE_NAME_LEN: usize = 64;

/// Validate a database name supplied by the model.
///
/// The name is not just a label: `create_database` turns it into a filesystem path
/// (`./netget_db_<name>.db`) that `delete_database` later `remove_file`s. A name
/// containing `/`, `..`, a NUL, or a leading `/` therefore reads and *destroys* files
/// outside the working directory. Generated text must never reach a path builder
/// unchecked, so the name is checked against a strict allowlist and **rejected** rather
/// than sanitised — a silently rewritten name would make the model's own bookkeeping
/// (it refers to databases by name) disagree with what exists on disk.
pub fn validate_database_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!(
            "Invalid database name: name must not be empty. \
             Allowed: 1-{} characters from A-Z, a-z, 0-9, '_' and '-'.",
            MAX_DATABASE_NAME_LEN
        );
    }
    if name.len() > MAX_DATABASE_NAME_LEN {
        anyhow::bail!(
            "Invalid database name '{}': {} bytes exceeds the {}-byte limit.",
            name,
            name.len(),
            MAX_DATABASE_NAME_LEN
        );
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '_' || *c == '-'))
    {
        anyhow::bail!(
            "Invalid database name '{}': character {:?} is not allowed. \
             A database name may contain only A-Z, a-z, 0-9, '_' and '-' \
             (it becomes the filename './netget_db_<name>.db').",
            name,
            bad
        );
    }
    Ok(())
}

/// The one filesystem location a file-backed database of this name may occupy.
///
/// Returns an error for any name [`validate_database_name`] rejects, so callers cannot
/// build the path first and validate later.
pub fn database_file_path(name: &str) -> anyhow::Result<String> {
    validate_database_name(name)?;
    Ok(format!("./netget_db_{}.db", name))
}

/// Unique identifier for a database instance
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct DatabaseId(u32);

impl DatabaseId {
    /// Create a new database ID from a u32
    pub fn new(id: u32) -> Self {
        Self(id)
    }

    /// Get the raw ID value
    pub fn as_u32(&self) -> u32 {
        self.0
    }

    /// Parse from string (expects format "db-123" or just "123")
    pub fn from_string(s: &str) -> Option<Self> {
        let s = s.trim();
        let id_str = s.strip_prefix("db-").unwrap_or(s);
        id_str.parse::<u32>().ok().map(Self)
    }
}

impl std::fmt::Display for DatabaseId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "db-{}", self.0)
    }
}

/// Database owner (server or client)
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DatabaseOwner {
    /// Database owned by a server
    Server(ServerId),
    /// Database owned by a client
    Client(ClientId),
    /// Global database (not tied to any server/client)
    Global,
}

impl std::fmt::Display for DatabaseOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Server(id) => write!(f, "Server {}", id),
            Self::Client(id) => write!(f, "Client {}", id),
            Self::Global => write!(f, "Global"),
        }
    }
}

/// Table schema information
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TableSchema {
    /// Table name
    pub name: String,
    /// Column definitions (e.g., "id INTEGER PRIMARY KEY", "name TEXT NOT NULL")
    pub columns: Vec<String>,
    /// Row count
    pub row_count: u64,
}

/// Database instance metadata and schema
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DatabaseInstance {
    /// Unique database ID
    pub id: DatabaseId,
    /// Database name (user-friendly)
    pub name: String,
    /// Database path (or ":memory:" for in-memory)
    pub path: String,
    /// Owner (server, client, or global)
    pub owner: DatabaseOwner,
    /// Table schemas
    pub tables: Vec<TableSchema>,
    /// When the database was created (not serialized)
    #[serde(skip, default = "Instant::now")]
    pub created_at: Instant,
    /// Last query execution time (not serialized)
    #[serde(skip, default)]
    pub last_query_at: Option<Instant>,
    /// Total number of queries executed
    pub query_count: u64,
}

impl DatabaseInstance {
    /// Create a new database instance
    #[cfg(feature = "sqlite")]
    pub fn new(id: DatabaseId, name: String, path: String, owner: DatabaseOwner) -> Self {
        Self {
            id,
            name,
            path,
            owner,
            tables: Vec::new(),
            created_at: Instant::now(),
            last_query_at: None,
            query_count: 0,
        }
    }

    /// Check if this is an in-memory database
    pub fn is_memory(&self) -> bool {
        self.path == ":memory:"
    }

    /// Update table schemas by introspecting the database
    #[cfg(feature = "sqlite")]
    pub fn refresh_schema(&mut self, conn: &Connection) -> Result<()> {
        self.tables.clear();

        // Get all tables
        let mut stmt = conn.prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name"
        )?;
        let table_names: Vec<String> = stmt
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;

        // Get schema for each table
        for table_name in table_names {
            let mut columns = Vec::new();

            // Get column information
            let mut stmt = conn.prepare(&format!("PRAGMA table_info('{}')", table_name))?;
            let rows = stmt.query_map([], |row| {
                let name: String = row.get(1)?;
                let type_name: String = row.get(2)?;
                let not_null: i32 = row.get(3)?;
                let pk: i32 = row.get(5)?;

                let mut col_def = format!("{} {}", name, type_name);
                if pk > 0 {
                    col_def.push_str(" PRIMARY KEY");
                } else if not_null != 0 {
                    col_def.push_str(" NOT NULL");
                }

                Ok(col_def)
            })?;

            for row in rows {
                columns.push(row?);
            }

            // Get row count
            let row_count: u64 = conn.query_row(
                &format!("SELECT COUNT(*) FROM '{}'", table_name),
                [],
                |row| row.get(0),
            )?;

            self.tables.push(TableSchema {
                name: table_name,
                columns,
                row_count,
            });
        }

        Ok(())
    }

    /// Get a summary of the database schema for LLM prompts
    pub fn schema_summary(&self) -> String {
        if self.tables.is_empty() {
            return format!("{} ({}): No tables", self.name, self.id);
        }

        let mut summary = format!("{} ({}):\n", self.name, self.id);
        for table in &self.tables {
            summary.push_str(&format!("  - {} ({} rows)\n", table.name, table.row_count));
            for column in &table.columns {
                summary.push_str(&format!("      {}\n", column));
            }
        }
        summary
    }

    /// Increment query count and update last query time
    #[cfg(feature = "sqlite")]
    pub fn record_query(&mut self) {
        self.query_count += 1;
        self.last_query_at = Some(Instant::now());
    }

    /// Update row counts for all tables (more efficient than full schema refresh)
    #[cfg(feature = "sqlite")]
    pub fn update_row_counts(&mut self, conn: &Connection) -> Result<()> {
        for table in &mut self.tables {
            table.row_count = conn.query_row(
                &format!("SELECT COUNT(*) FROM '{}'", table.name),
                [],
                |row| row.get(0),
            )?;
        }
        Ok(())
    }
}

/// Database connection wrapper (single connection protected by Mutex)
#[cfg(feature = "sqlite")]
pub struct DatabaseConnection {
    /// SQLite connection protected by Mutex
    conn: Mutex<Connection>,
    /// Database metadata
    instance: DatabaseInstance,
}

#[cfg(feature = "sqlite")]
impl DatabaseConnection {
    /// Create a new database connection
    pub fn new(instance: DatabaseInstance) -> Result<Self> {
        let conn =
            Connection::open(&instance.path).context("Failed to open database connection")?;

        Ok(Self {
            conn: Mutex::new(conn),
            instance,
        })
    }

    /// Get the database metadata
    pub fn instance(&self) -> &DatabaseInstance {
        &self.instance
    }

    /// Get a mutable reference to the database metadata
    pub fn instance_mut(&mut self) -> &mut DatabaseInstance {
        &mut self.instance
    }

    /// Execute a SQL query and return results as JSON
    ///
    /// The connection mutex is taken with `unwrap_or_else(|e| e.into_inner())` rather than
    /// `unwrap()`: a panic anywhere under this lock would otherwise poison it permanently
    /// and every later query on this database — including the ones that would report the
    /// problem — would panic in turn. The `Connection` itself stays usable.
    pub fn execute_query(&mut self, sql: &str) -> Result<QueryResult> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());

        // Record query execution
        self.instance.record_query();

        // Check if this is a SELECT query (read-only)
        let sql_upper = sql.trim().to_uppercase();
        let is_select = sql_upper.starts_with("SELECT")
            || sql_upper.starts_with("PRAGMA")
            || sql_upper.starts_with("EXPLAIN");

        if is_select {
            // Execute SELECT query
            let mut stmt = conn.prepare(sql)?;
            let column_names: Vec<String> =
                stmt.column_names().iter().map(|s| s.to_string()).collect();

            let mut rows = Vec::new();
            let mut query_rows = stmt.query([])?;

            while let Some(row) = query_rows.next()? {
                let mut row_values = Vec::new();
                for i in 0..column_names.len() {
                    let value: serde_json::Value = match row.get_ref(i)? {
                        rusqlite::types::ValueRef::Null => serde_json::Value::Null,
                        rusqlite::types::ValueRef::Integer(i) => {
                            serde_json::Value::Number(i.into())
                        }
                        rusqlite::types::ValueRef::Real(f) => serde_json::Value::Number(
                            serde_json::Number::from_f64(f)
                                .unwrap_or_else(|| serde_json::Number::from(0)),
                        ),
                        rusqlite::types::ValueRef::Text(t) => {
                            serde_json::Value::String(String::from_utf8_lossy(t).to_string())
                        }
                        rusqlite::types::ValueRef::Blob(b) => {
                            serde_json::Value::String(hex::encode(b))
                        }
                    };
                    row_values.push(value);
                }
                rows.push(row_values);
            }

            Ok(QueryResult::Select {
                columns: column_names,
                rows,
            })
        } else {
            // Execute DML/DDL query (INSERT, UPDATE, DELETE, CREATE, etc.)
            let affected_rows = conn.execute(sql, [])?;

            // Refresh schema if this was a DDL statement
            if sql_upper.starts_with("CREATE")
                || sql_upper.starts_with("DROP")
                || sql_upper.starts_with("ALTER")
            {
                self.instance.refresh_schema(&conn)?;
            } else if sql_upper.starts_with("INSERT")
                || sql_upper.starts_with("UPDATE")
                || sql_upper.starts_with("DELETE")
            {
                // For DML operations, just update row counts (more efficient than full schema refresh)
                self.instance.update_row_counts(&conn)?;
            }

            Ok(QueryResult::Modified { affected_rows })
        }
    }

    /// Refresh table schemas
    pub fn refresh_schema(&mut self) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        self.instance.refresh_schema(&conn)
    }
}

/// Result of a SQL query execution
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum QueryResult {
    /// SELECT query result
    Select {
        /// Column names
        columns: Vec<String>,
        /// Rows (each row is an array of values)
        rows: Vec<Vec<serde_json::Value>>,
    },
    /// DML/DDL query result (INSERT, UPDATE, DELETE, CREATE, etc.)
    Modified {
        /// Number of rows affected
        affected_rows: usize,
    },
}

impl QueryResult {
    /// Format result as a human-readable string
    pub fn format(&self) -> String {
        match self {
            Self::Select { columns, rows } => {
                if rows.is_empty() {
                    return format!("No rows returned. Columns: {}", columns.join(", "));
                }

                let mut output = String::new();
                output.push_str(&columns.join(" | "));
                output.push('\n');
                output.push_str(&"-".repeat(output.len()));
                output.push('\n');

                for row in rows {
                    let row_str: Vec<String> = row
                        .iter()
                        .map(|v| match v {
                            serde_json::Value::Null => "NULL".to_string(),
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .collect();
                    output.push_str(&row_str.join(" | "));
                    output.push('\n');
                }

                output
            }
            Self::Modified { affected_rows } => {
                format!("{} row(s) affected", affected_rows)
            }
        }
    }

    /// Get row count
    pub fn row_count(&self) -> usize {
        match self {
            Self::Select { rows, .. } => rows.len(),
            Self::Modified { affected_rows } => *affected_rows,
        }
    }
}

/// Database manager (holds all database connections)
#[cfg(feature = "sqlite")]
pub struct DatabaseManager {
    /// Map of database ID to connection
    connections: HashMap<DatabaseId, DatabaseConnection>,
}

#[cfg(feature = "sqlite")]
impl DatabaseManager {
    /// Create a new database manager
    pub fn new() -> Self {
        Self {
            connections: HashMap::new(),
        }
    }

    /// Create a new database
    pub fn create_database(
        &mut self,
        id: DatabaseId,
        name: String,
        path: String,
        owner: DatabaseOwner,
        init_sql: Option<&str>,
    ) -> Result<()> {
        // Defence in depth: the name reaches this point from a model-authored action, and
        // `delete_database` will `remove_file` whatever path is recorded here. Validate at
        // the boundary that owns the filesystem effect, not only at the caller — the
        // caller's own check is one refactor away from being skipped.
        //
        // Only the *name* is constrained. `path` is a Rust-caller parameter (tests open
        // databases at explicit temp paths) and no model-authored value reaches it except
        // through `database_file_path`, which validates the name first.
        validate_database_name(&name)?;

        // Create database instance
        let instance = DatabaseInstance::new(id, name, path, owner);

        // Create connection
        let mut conn = DatabaseConnection::new(instance)?;

        // Execute initialization SQL if provided
        if let Some(sql) = init_sql {
            // Use execute_batch for multi-statement SQL
            let db_conn = conn.conn.lock().unwrap_or_else(|e| e.into_inner());
            db_conn.execute_batch(sql)?;
            drop(db_conn);
        }

        // Refresh schema
        conn.refresh_schema()?;

        // Store connection
        self.connections.insert(id, conn);

        Ok(())
    }

    /// Get a database instance (metadata only)
    pub fn get_instance(&self, id: DatabaseId) -> Option<&DatabaseInstance> {
        self.connections.get(&id).map(|conn| conn.instance())
    }

    /// Get all database instances
    pub fn get_all_instances(&self) -> Vec<&DatabaseInstance> {
        self.connections
            .values()
            .map(|conn| conn.instance())
            .collect()
    }

    /// Execute a query on a database
    pub fn execute_query(&mut self, id: DatabaseId, sql: &str) -> Result<QueryResult> {
        let conn = self
            .connections
            .get_mut(&id)
            .context("Database not found")?;
        conn.execute_query(sql)
    }

    /// Delete a database
    pub fn delete_database(&mut self, id: DatabaseId) -> Result<()> {
        let conn = self.connections.remove(&id).context("Database not found")?;

        // Delete file if not in-memory
        if !conn.instance().is_memory() {
            let path = PathBuf::from(&conn.instance().path);
            if path.exists() {
                std::fs::remove_file(&path).context("Failed to delete database file")?;
            }
        }

        Ok(())
    }

    /// Get databases owned by a server
    pub fn get_databases_by_server(&self, server_id: ServerId) -> Vec<&DatabaseInstance> {
        self.connections
            .values()
            .map(|conn| conn.instance())
            .filter(|instance| instance.owner == DatabaseOwner::Server(server_id))
            .collect()
    }

    /// Get databases owned by a client
    pub fn get_databases_by_client(&self, client_id: ClientId) -> Vec<&DatabaseInstance> {
        self.connections
            .values()
            .map(|conn| conn.instance())
            .filter(|instance| instance.owner == DatabaseOwner::Client(client_id))
            .collect()
    }

    /// Delete all databases owned by a server (called when server closes)
    pub fn delete_databases_by_server(&mut self, server_id: ServerId) -> Result<()> {
        let db_ids: Vec<DatabaseId> = self
            .connections
            .values()
            .filter(|conn| conn.instance().owner == DatabaseOwner::Server(server_id))
            .map(|conn| conn.instance().id)
            .collect();

        for id in db_ids {
            self.delete_database(id)?;
        }

        Ok(())
    }

    /// Delete all databases owned by a client (called when client disconnects)
    pub fn delete_databases_by_client(&mut self, client_id: ClientId) -> Result<()> {
        let db_ids: Vec<DatabaseId> = self
            .connections
            .values()
            .filter(|conn| conn.instance().owner == DatabaseOwner::Client(client_id))
            .map(|conn| conn.instance().id)
            .collect();

        for id in db_ids {
            self.delete_database(id)?;
        }

        Ok(())
    }
}

#[cfg(not(feature = "sqlite"))]
pub struct DatabaseManager;

#[cfg(not(feature = "sqlite"))]
impl DatabaseManager {
    pub fn new() -> Self {
        Self
    }
}
