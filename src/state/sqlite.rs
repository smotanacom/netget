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

/// Hard limits apply before copying query output into JSON.
pub const MAX_QUERY_ROWS: usize = 10_000;
pub const MAX_QUERY_RESULT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_QUERY_VALUE_BYTES: i32 = 8 * 1024 * 1024;
pub const MAX_QUERY_SQL_BYTES: i32 = 1024 * 1024;
/// Bound CPU work as well as output size (e.g. recursive CTEs with no output).
pub const MAX_QUERY_VM_STEPS: usize = 10_000_000;
pub const MAX_QUERY_DURATION: std::time::Duration = std::time::Duration::from_secs(5);

/// SQLite identifiers may contain quotes and punctuation. Quote names read from
/// sqlite_master before interpolating them into introspection/count statements.
#[cfg(feature = "sqlite")]
fn quoted_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

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
            let quoted_name = quoted_identifier(&table_name);
            let mut stmt = conn.prepare(&format!("PRAGMA table_info({quoted_name})"))?;
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
            let row_count: u64 =
                conn.query_row(&format!("SELECT COUNT(*) FROM {quoted_name}"), [], |row| {
                    row.get(0)
                })?;

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
                &format!("SELECT COUNT(*) FROM {}", quoted_identifier(&table.name)),
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

/// Refuse every statement that would make the connection touch a file other than
/// the one it was opened on.
///
/// `ATTACH` reports its filename only when it is a string literal; a computed one
/// (`'/etc/' || 'x'`) reaches the authorizer as the raw `SQLITE_ATTACH` code with no
/// argument, so that case is matched on the code, not on the parsed variant. An
/// empty filename or `:memory:` is a private temporary database and stays allowed —
/// a plain `VACUUM` attaches one.
#[cfg(feature = "sqlite")]
fn filesystem_authorizer(ctx: rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization {
    use rusqlite::hooks::{AuthAction, Authorization};
    match ctx.action {
        AuthAction::Attach { filename } => {
            if filename.is_empty() || filename == MEMORY_DATABASE_PATH {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        AuthAction::Unknown { code, .. } if code == rusqlite::ffi::SQLITE_ATTACH => {
            Authorization::Deny
        }
        AuthAction::Function { function_name }
            if function_name.eq_ignore_ascii_case("load_extension") =>
        {
            Authorization::Deny
        }
        _ => Authorization::Allow,
    }
}

#[cfg(feature = "sqlite")]
impl DatabaseConnection {
    /// Create a new database connection
    pub fn new(instance: DatabaseInstance) -> Result<Self> {
        let conn =
            Connection::open(&instance.path).context("Failed to open database connection")?;

        // The SQL on this connection is model-authored, so the connection must not be a
        // way to the filesystem. `create_database` validates the *name* precisely so a
        // model can never choose a path; `ATTACH DATABASE '<path>'` and
        // `VACUUM INTO '<path>'` (which attaches its target internally) would otherwise
        // create, overwrite or read any SQLite file the process can reach in one
        // statement. `load_extension` is already disabled at the C API level; denying
        // it here says so where the other refusals live.
        conn.authorizer(Some(filesystem_authorizer));

        conn.set_limit(
            rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH,
            MAX_QUERY_VALUE_BYTES,
        )
        .context("Failed to set SQLite value size limit")?;
        conn.set_limit(
            rusqlite::limits::Limit::SQLITE_LIMIT_SQL_LENGTH,
            MAX_QUERY_SQL_BYTES,
        )
        .context("Failed to set SQLite SQL size limit")?;
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

        let started = std::time::Instant::now();
        let mut steps = 0usize;
        conn.progress_handler(
            1000,
            Some(move || {
                steps += 1000;
                steps >= MAX_QUERY_VM_STEPS || started.elapsed() >= MAX_QUERY_DURATION
            }),
        );
        let result = (|| -> Result<QueryResult> {
            use rusqlite::fallible_iterator::FallibleIterator;
            let mut batch = rusqlite::Batch::new(&conn, sql);
            let mut stmt = batch.next()?.context("query contains no SQL statement")?;
            anyhow::ensure!(
                batch.next()?.is_none(),
                "execute_query accepts exactly one SQL statement"
            );
            let readonly = stmt.readonly();
            let result = if stmt.column_count() > 0 {
                let column_names: Vec<String> = stmt
                    .column_names()
                    .iter()
                    .map(|name| name.to_string())
                    .collect();
                let mut bytes: usize = column_names
                    .iter()
                    .map(|name| name.len().saturating_mul(6).saturating_add(3))
                    .sum();
                let mut rows = Vec::new();
                let mut query_rows = stmt.query([])?;
                while let Some(row) = query_rows.next()? {
                    anyhow::ensure!(rows.len() < MAX_QUERY_ROWS, "query result exceeds {} rows; use LIMIT (a modifying statement may already have executed)", MAX_QUERY_ROWS);
                    let mut values = Vec::with_capacity(column_names.len());
                    bytes = bytes.saturating_add(2);
                    for i in 0..column_names.len() {
                        use rusqlite::types::ValueRef;
                        let value = row.get_ref(i)?;
                        let cost = match value {
                            ValueRef::Text(text) => text.len().saturating_mul(6).saturating_add(3),
                            ValueRef::Blob(blob) => blob.len().saturating_mul(2).saturating_add(3),
                            _ => 32,
                        };
                        bytes = bytes.saturating_add(cost);
                        anyhow::ensure!(bytes <= MAX_QUERY_RESULT_BYTES, "query result exceeds {} bytes; select fewer/smaller values (a modifying statement may already have executed)", MAX_QUERY_RESULT_BYTES);
                        values.push(match value {
                            ValueRef::Null => serde_json::Value::Null,
                            ValueRef::Integer(value) => serde_json::json!(value),
                            ValueRef::Real(value) => serde_json::json!(value),
                            ValueRef::Text(value) => serde_json::Value::String(
                                String::from_utf8_lossy(value).into_owned(),
                            ),
                            ValueRef::Blob(value) => serde_json::Value::String(hex::encode(value)),
                        });
                    }
                    rows.push(values);
                }
                QueryResult::Select {
                    columns: column_names,
                    rows,
                }
            } else {
                QueryResult::Modified {
                    affected_rows: stmt.execute([])?,
                }
            };
            drop(stmt);
            // The prepared statement classifies WITH, comments, REPLACE, RETURNING,
            // PRAGMA and DDL reliably; textual prefix checks do not.
            if !readonly {
                self.instance.refresh_schema(&conn)?;
            }
            Ok(result)
        })();
        conn.progress_handler(0, None::<fn() -> bool>);
        result.map_err(|error| anyhow::anyhow!("SQLite query failed: {error:#}"))
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
