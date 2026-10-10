//! The model's `execute_sql` must not reach the filesystem beyond the one database
//! file `create_database` validated the name of.
//!
//! `ATTACH DATABASE '<path>'` opens (and creates) any SQLite file the process can
//! write; `VACUUM INTO '<path>'` writes a copy anywhere. Both are a single statement,
//! so `execute_query`'s one-statement rule does not stop them, and the name check in
//! `validate_database_name` — which exists so a model-authored value can never choose
//! a path — was the only thing standing between a prompt-injected model and
//! `~/.ssh` or a browser's `cookies.sqlite`. In-memory SQL only; no model.
#![cfg(feature = "sqlite")]
use netget::state::sqlite::{
    DatabaseConnection, DatabaseId, DatabaseInstance, DatabaseOwner, QueryResult,
};

fn database() -> DatabaseConnection {
    DatabaseConnection::new(DatabaseInstance::new(
        DatabaseId::new(1),
        "escape".into(),
        ":memory:".into(),
        DatabaseOwner::Global,
    ))
    .unwrap()
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "netget-sqlite-escape-{}-{}",
        std::process::id(),
        name
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn attach_of_a_file_path_is_refused_and_creates_nothing() {
    let dir = scratch("attach");
    let target = dir.join("planted.db");
    let mut db = database();
    let sql = format!("ATTACH DATABASE '{}' AS other", target.display());
    let err = db
        .execute_query(&sql)
        .expect_err("ATTACH of a path must be refused");
    assert!(
        err.to_string().contains("not authorized"),
        "refusal should come from the authorizer, got: {err:#}"
    );
    assert!(
        !target.exists(),
        "the refused ATTACH must not have created {}",
        target.display()
    );
    // A later statement on the same connection still works: the refusal is per statement.
    db.execute_query("CREATE TABLE t(x)").unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn attach_of_an_existing_database_cannot_read_it() {
    let dir = scratch("read");
    let victim = dir.join("cookies.sqlite");
    {
        let conn = rusqlite::Connection::open(&victim).unwrap();
        conn.execute_batch("CREATE TABLE moz_cookies(value TEXT); INSERT INTO moz_cookies VALUES('session=secret');").unwrap();
    }
    let mut db = database();
    let sql = format!("ATTACH DATABASE 'file:{}?mode=ro' AS v", victim.display());
    assert!(db.execute_query(&sql).is_err());
    let sql = format!("ATTACH DATABASE '{}' AS v", victim.display());
    assert!(db.execute_query(&sql).is_err());
    assert!(db.execute_query("SELECT value FROM v.moz_cookies").is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn vacuum_into_a_path_is_refused_and_writes_nothing() {
    let dir = scratch("vacuum");
    let target = dir.join("copy.db");
    let mut db = database();
    db.execute_query("CREATE TABLE t(x)").unwrap();
    let sql = format!("VACUUM INTO '{}'", target.display());
    assert!(
        db.execute_query(&sql).is_err(),
        "VACUUM INTO a path must be refused"
    );
    assert!(
        !target.exists(),
        "VACUUM INTO must not have written {}",
        target.display()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ordinary_sql_and_a_plain_vacuum_still_work() {
    let mut db = database();
    db.execute_query("CREATE TABLE t(x INTEGER)").unwrap();
    db.execute_query("INSERT INTO t VALUES (1), (2)").unwrap();
    let result = db.execute_query("SELECT count(*) FROM t").unwrap();
    assert!(
        matches!(result, QueryResult::Select { rows, .. } if rows == vec![vec![serde_json::json!(2)]])
    );
    db.execute_query("PRAGMA table_info(t)").unwrap();
    db.execute_query("CREATE TEMP TABLE scratch(y)").unwrap();
    db.execute_query("VACUUM").unwrap();
}
