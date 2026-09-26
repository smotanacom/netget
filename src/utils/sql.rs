//! What kind of answer an SQL statement expects, from its leading keyword.
//!
//! The SQL servers (`postgresql`, `mysql`) hand the model the statement text and three ways to
//! answer it: a result set, a command tag / OK packet, or an error. The real-model eval showed
//! a small model answering `SELECT current_user` with the command tag `INSERT 0 1` — copied from
//! the ok action's example — so the client got "INSERT 0 1" for a query that asked for a value.
//! Both servers know the statement exactly, so each tells the model which shape answers it
//! (`answer_with` in the event) instead of leaving it to map SQL to the wire in its head.
//!
//! This is a classifier over the first keyword, not a parser: it reads past leading whitespace,
//! `--` and `/* */` comments and opening parentheses, and nothing else. A statement it cannot
//! place is [`StatementShape::Unknown`], and the caller says nothing rather than guess.

/// The answer a statement's leading keyword calls for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementShape {
    /// Returns rows: `SELECT`, `SHOW`, `VALUES`, `TABLE`, `WITH`, `EXPLAIN`, `DESCRIBE`/`DESC`,
    /// `FETCH`. Answered with a result set, even when the answer is one value or no rows.
    Rows,
    /// Changes or configures something and returns no rows: `INSERT`, `UPDATE`, `DELETE`,
    /// `CREATE`, `SET`, `BEGIN`, …. Answered with a command tag / OK.
    NoRows,
    /// Empty, or a keyword this does not classify.
    Unknown,
}

/// Classify `sql` by its first keyword. See the module docs for what "first" means.
pub fn statement_shape(sql: &str) -> StatementShape {
    let keyword = first_keyword(sql).to_ascii_uppercase();
    match keyword.as_str() {
        "SELECT" | "SHOW" | "VALUES" | "TABLE" | "WITH" | "EXPLAIN" | "DESCRIBE" | "DESC"
        | "FETCH" => StatementShape::Rows,
        "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "CREATE" | "DROP" | "ALTER" | "TRUNCATE"
        | "SET" | "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" | "USE"
        | "GRANT" | "REVOKE" | "LOCK" | "UNLOCK" | "DISCARD" | "RESET" | "DEALLOCATE"
        | "PREPARE" | "COPY" | "VACUUM" | "ANALYZE" | "COMMENT" | "LISTEN" | "NOTIFY"
        | "UNLISTEN" => StatementShape::NoRows,
        _ => StatementShape::Unknown,
    }
}

/// The first SQL keyword of `sql`, skipping whitespace, comments and opening parentheses.
fn first_keyword(sql: &str) -> &str {
    let mut rest = sql;
    loop {
        let trimmed = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '(');
        if let Some(after) = trimmed.strip_prefix("--") {
            rest = after.split_once('\n').map(|(_, tail)| tail).unwrap_or("");
        } else if let Some(after) = trimmed.strip_prefix("/*") {
            rest = after.split_once("*/").map(|(_, tail)| tail).unwrap_or("");
        } else {
            rest = trimmed;
            break;
        }
    }
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    &rest[..end]
}
